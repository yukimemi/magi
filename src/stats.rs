//! Aggregate statistics over every recorded run.
//!
//! These tables are a by-product of running the graph, not a benchmark. The
//! seat assignment rotates, the task distribution is whatever the operator
//! happened to ask for, and a model that draws harder tasks looks worse. Read
//! them as "relative performance on my workload", which is the only claim the
//! data supports.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::run::{RunState, RunStatus, list_ids};

/// Implementation record for one agent.
#[derive(Debug, Clone, Default)]
pub struct AgentStats {
    /// Agent id.
    pub agent: String,
    /// Candidates it produced that were judged.
    pub entered: usize,
    /// Competitions it won.
    pub wins: usize,
    /// Candidates that produced no change at all.
    pub empty: usize,
}

impl AgentStats {
    /// Win rate over entries, as a percentage.
    pub fn win_rate(&self) -> f64 {
        if self.entered == 0 {
            0.0
        } else {
            100.0 * self.wins as f64 / self.entered as f64
        }
    }
}

/// Review record for one agent.
#[derive(Debug, Clone, Default)]
pub struct ReviewerStats {
    /// Agent id.
    pub agent: String,
    /// Review rounds it sat in whose adoption could be scored — a round whose
    /// fixer never reported back is excluded, so this is the denominator of
    /// [`Self::adopted_per_round`], not a headcount of appearances. For that,
    /// see [`Self::seated`].
    pub rounds: usize,
    /// Review rounds it was on the panel for at all, scoreable or not.
    /// Whether a seat answered is a fact about the seat and does not depend
    /// on what later became of the fixer's report, so this — not `rounds` —
    /// is the honest denominator for [`Self::timeout_rate`].
    pub seated: usize,
    /// Findings it submitted.
    pub submitted: usize,
    /// Findings the fixer acted on.
    pub adopted: usize,
    /// Findings no other reviewer in the same round also raised.
    pub unique: usize,
    /// Rounds it was seated in but never answered (timeout, crash, unparsable
    /// output) — kept apart from `submitted`/`adopted` so a silent seat
    /// cannot read as a seat with nothing to say.
    pub timeouts: usize,
}

impl ReviewerStats {
    /// Adopted findings per round: how much signal one seat produces.
    pub fn adopted_per_round(&self) -> f64 {
        if self.rounds == 0 {
            0.0
        } else {
            self.adopted as f64 / self.rounds as f64
        }
    }

    /// Adopted over submitted: how often its findings are real. Rounds where
    /// the seat never answered are not in `submitted`, so a timeout cannot
    /// dilute (or hide behind) this rate.
    pub fn precision(&self) -> f64 {
        if self.submitted == 0 {
            0.0
        } else {
            100.0 * self.adopted as f64 / self.submitted as f64
        }
    }

    /// Share of its findings that only it saw.
    pub fn unique_rate(&self) -> f64 {
        if self.submitted == 0 {
            0.0
        } else {
            100.0 * self.unique as f64 / self.submitted as f64
        }
    }

    /// Share of the rounds it was seated in where it never answered.
    pub fn timeout_rate(&self) -> f64 {
        if self.seated == 0 {
            0.0
        } else {
            100.0 * self.timeouts as f64 / self.seated as f64
        }
    }
}

/// Design-deliberation record for one agent.
///
/// Approximate by construction, same as [`crate::advise::Reflection`] itself:
/// `strong`/`faint` come from a word-overlap heuristic against the synthesis
/// brief, not from an explicit attribution, so [`Self::reflection_rate`] is a
/// rough read on whose ideas seemed to land, not a precise credit split.
#[derive(Debug, Clone, Default)]
pub struct AdvisorStats {
    /// Agent id.
    pub agent: String,
    /// Advisor seats it occupied, across every run with `[graph] advise` on.
    pub seated: usize,
    /// Seats where it produced a usable proposal (`record.proposal.is_some()`).
    pub proposed: usize,
    /// Seats where it produced no usable proposal at all (crashed, timed
    /// out, or answered with nothing a proposal could be parsed out of).
    /// Counted from `record.proposal.is_none()` directly rather than from
    /// `record.reflection == Absent` — `reflection` is `#[serde(default)]`
    /// and can be left at its default on a record from before
    /// `apply_reflection` ran, which would otherwise double as a false
    /// "absent".
    pub absent: usize,
    /// Proposals the synthesis brief carried little or no recognisable
    /// trace of.
    pub faint: usize,
    /// Proposals the synthesis brief named outright or carried enough of to
    /// count as a clear match.
    pub strong: usize,
}

impl AdvisorStats {
    /// Share of its proposals rated `Strong`, as a percentage. The
    /// denominator is `proposed`, not `seated` — a seat that never produced
    /// a proposal had nothing for the synthesis to reflect, so it cannot
    /// count against this rate any more than a reviewer's silence counts
    /// against [`ReviewerStats::precision`].
    ///
    /// Same caveat as the struct itself: this is a heuristic read on
    /// reflection, not a precise attribution.
    pub fn reflection_rate(&self) -> f64 {
        if self.proposed == 0 {
            0.0
        } else {
            100.0 * self.strong as f64 / self.proposed as f64
        }
    }
}

/// What real-machine verification caught that static review did not.
#[derive(Debug, Clone, Default)]
pub struct E2eStats {
    /// Rounds where E2E commands ran.
    pub rounds: usize,
    /// Rounds where E2E failed.
    pub failures: usize,
    /// Rounds where E2E failed and no reviewer had raised a blocking finding —
    /// a runtime defect that only execution found.
    pub sole_detections: usize,
    /// Rounds where E2E was deferred rather than run: blocking findings
    /// already required a fix, so the round went straight to the fixer
    /// instead of spending a full verify run on a head about to change. Kept
    /// separate from [`Self::rounds`] on purpose — a deferred round never
    /// ran anything, so counting it there would misreport how often E2E
    /// actually executed.
    pub deferred: usize,
}

impl E2eStats {
    /// Share of E2E failures that static review had missed entirely.
    pub fn sole_rate(&self) -> f64 {
        if self.failures == 0 {
            0.0
        } else {
            100.0 * self.sole_detections as f64 / self.failures as f64
        }
    }
}

/// Run-level counters.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    /// Runs on disk.
    pub runs: usize,
    /// Reached a merge.
    pub merged: usize,
    /// Passed the gate, merge not requested.
    pub ready: usize,
    /// Stopped with findings open or a red gate.
    pub blocked: usize,
    /// Could not complete.
    pub failed: usize,
    /// The judging panel never reached a quorum; the work is kept but the
    /// verdict is not trustworthy. Never counted as `blocked` — see
    /// `RunStatus::Stalled`'s own doc — so it needs its own counter to stay
    /// visible in a breakdown rather than vanishing from every bucket.
    pub stalled: usize,
    /// Every candidate wrote nothing, and said why in a way that survived
    /// the adoption guard: a claim, not a failure, and not yet verified.
    pub verified_noop: usize,
    /// A later attempt at the same task already finished it; this run's own
    /// `blocked`/`stalled` no longer needs anyone's attention.
    pub superseded: usize,
    /// Still moving: any non-terminal status (prep through landing). Kept as
    /// one bucket rather than one counter per node — the per-node position of
    /// a live run belongs to `magi show`/the deck, not to a workload-wide
    /// tally that is read well after the run in question has finished.
    pub in_progress: usize,
    /// Runs that reached a tally.
    pub tallied: usize,
    /// Tallies where the judges' first choices disagreed.
    pub split: usize,
    /// Tallies that went through deliberation.
    pub deliberated: usize,
    /// Deliberated runs where at least one judge moved.
    pub minds_changed: usize,
    /// Deliberated runs that ended unanimous.
    pub converged: usize,
    /// Review rounds across all runs.
    pub review_rounds: usize,
}

impl Totals {
    /// Merged or ready over all runs.
    pub fn completion_rate(&self) -> f64 {
        if self.runs == 0 {
            0.0
        } else {
            100.0 * (self.merged + self.ready) as f64 / self.runs as f64
        }
    }

    /// Share of tallies that were split.
    pub fn split_rate(&self) -> f64 {
        if self.tallied == 0 {
            0.0
        } else {
            100.0 * self.split as f64 / self.tallied as f64
        }
    }
}

/// Per-node duration breakdown, aggregated across every loaded run.
///
/// A duration here is the span between a node's *first* and *last* recorded
/// event within one run — not time actually spent working. A node visited
/// more than twice in one run (a retry, or a park/resume gap) has any idle
/// time in between folded into that span, so this reads as an upper bound on
/// the node's wall-clock cost, not a measurement of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeDuration {
    /// Node name, as it appears in `Event::node`.
    pub node: String,
    /// Runs where the node's span could be measured (at least two events).
    pub runs: usize,
    /// Sum of every measured run's span, in seconds.
    pub total_secs: i64,
    /// The longest single run's span, in seconds.
    pub max_secs: i64,
    /// Runs where the node had exactly one event — visited, but with no
    /// second timestamp to measure a span against. Counted apart from `runs`
    /// so a caller never has to guess whether a low mean hides unmeasured
    /// visits.
    pub single: usize,
}

impl NodeDuration {
    /// Mean span across the runs that could be measured, in seconds.
    ///
    /// `0.0` when `runs` is zero, never a division by zero.
    pub fn mean_secs(&self) -> f64 {
        if self.runs == 0 {
            0.0
        } else {
            self.total_secs as f64 / self.runs as f64
        }
    }
}

/// Derive a per-node duration breakdown from every loaded run's events.
///
/// For each run, a node's span is the gap between its earliest and latest
/// [`crate::run::Event`] in that run. A node with fewer than two events in a
/// run contributes no span (see [`NodeDuration::single`] for the one-event
/// case); a run with no events for a node contributes nothing at all. Spans
/// from every run are then summed per node.
pub fn node_durations<'a>(states: impl IntoIterator<Item = &'a RunState>) -> Vec<NodeDuration> {
    let mut nodes: BTreeMap<String, NodeDuration> = BTreeMap::new();

    for state in states {
        let mut spans: BTreeMap<&str, (jiff::Timestamp, jiff::Timestamp, usize)> = BTreeMap::new();
        for e in &state.events {
            spans
                .entry(e.node.as_str())
                .and_modify(|(min, max, count)| {
                    if e.at < *min {
                        *min = e.at;
                    }
                    if e.at > *max {
                        *max = e.at;
                    }
                    *count += 1;
                })
                .or_insert((e.at, e.at, 1));
        }

        for (node, (min, max, count)) in spans {
            let entry = nodes
                .entry(node.to_owned())
                .or_insert_with(|| NodeDuration {
                    node: node.to_owned(),
                    ..NodeDuration::default()
                });
            if count < 2 {
                entry.single += 1;
                continue;
            }
            // `max - min` is never negative: both are the extremes of the
            // same event set, and equal timestamps yield a zero-second span
            // rather than being mistaken for an unmeasured visit.
            let span_secs = (max - min).get_seconds();
            entry.runs += 1;
            entry.total_secs += span_secs;
            if span_secs > entry.max_secs {
                entry.max_secs = span_secs;
            }
        }
    }

    let mut nodes: Vec<NodeDuration> = nodes.into_values().collect();
    nodes.sort_by(|a, b| b.total_secs.cmp(&a.total_secs).then(a.node.cmp(&b.node)));
    nodes
}

/// What the post-merge release-bump step did, across every merged run.
///
/// `merged` is the denominator for [`Self::coverage_rate`]: a repository
/// that never uses the release-bump step (no `auto-tag.yml`, see
/// `kata:agents:rust:*`) should read as "0 of N merged runs recorded a
/// bump", not vanish from the report the way it would if `recorded` were the
/// denominator instead — silently excluding those runs would hide the
/// coverage gap itself.
#[derive(Debug, Clone, Default)]
pub struct ReleaseBumpStats {
    /// Merged runs, the denominator for [`Self::coverage_rate`].
    pub merged: usize,
    /// Merged runs that carry a [`crate::run::ReleaseBump`] record at all.
    pub recorded: usize,
    /// Recorded bumps that opened a release pull request.
    pub pr_opened: usize,
    /// Recorded bumps whose `automerge_enabled` is `true` at the point the
    /// bump step finished. This is the *final* state, not "was automerge
    /// enabled at some point" — `bump::surface_problem` flips it back to
    /// `false` when GitHub later rejects automerge, so a bump that briefly
    /// enabled it and then had it rejected counts here as not enabled.
    pub automerge_enabled: usize,
    /// Recorded bumps magi merged directly because GitHub refused automerge
    /// on an already-green pull request. Counted apart from
    /// `automerge_enabled` on purpose: a `merged_directly` bump needed no
    /// human even though automerge itself never took, so folding it into
    /// (or leaving it out of) the automerge count would misstate either
    /// number.
    pub merged_directly: usize,
    /// Recorded bumps that ended with [`crate::run::RunState::needs_attention`]
    /// true — a human has something to do.
    pub needs_attention: usize,
}

impl ReleaseBumpStats {
    /// Share of merged runs that recorded a release bump at all.
    pub fn coverage_rate(&self) -> f64 {
        if self.merged == 0 {
            0.0
        } else {
            100.0 * self.recorded as f64 / self.merged as f64
        }
    }

    /// Share of opened release PRs that ended with automerge enabled.
    pub fn automerge_rate(&self) -> f64 {
        if self.pr_opened == 0 {
            0.0
        } else {
            100.0 * self.automerge_enabled as f64 / self.pr_opened as f64
        }
    }

    /// Share of recorded bumps that needed a human.
    pub fn attention_rate(&self) -> f64 {
        if self.recorded == 0 {
            0.0
        } else {
            100.0 * self.needs_attention as f64 / self.recorded as f64
        }
    }

    /// Recorded bumps that finished with nothing for a human to do.
    ///
    /// Deliberately the difference `recorded - needs_attention`, not a
    /// separate counter kept in step with `automerge_enabled`: a bump that
    /// GitHub refused automerge on but that magi merged directly
    /// (`merged_directly`) is clean — nobody had to act — even though
    /// `automerge_enabled` is `false` for it. Counting clean bumps as
    /// `automerge_enabled` alone would leave `merged_directly` cases in
    /// neither the clean nor the attention bucket, and the two would stop
    /// summing to `recorded`.
    pub fn clean(&self) -> usize {
        self.recorded.saturating_sub(self.needs_attention)
    }
}

/// Everything, aggregated.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    /// Run counters.
    pub totals: Totals,
    /// Per-agent implementation record, best win rate first.
    pub agents: Vec<AgentStats>,
    /// Per-agent review record, most adopted-per-round first.
    pub reviewers: Vec<ReviewerStats>,
    /// Per-agent design-deliberation record, highest reflection rate first.
    /// Only runs where `[graph] advise` produced an [`crate::advise::Advice`]
    /// contribute — a run with the stage off carries no signal either way,
    /// and counting it would water down every agent's rate with seats that
    /// were never asked.
    pub advisors: Vec<AdvisorStats>,
    /// Verification record.
    pub e2e: E2eStats,
    /// Per-node duration breakdown, longest total first.
    pub nodes: Vec<NodeDuration>,
    /// Post-merge release-bump record, over every merged run.
    pub release_bumps: ReleaseBumpStats,
}

/// Load every run on disk, skipping any that cannot be read.
pub fn load_all() -> Vec<RunState> {
    list_ids()
        .into_iter()
        .filter_map(|id| RunState::load(&id).ok())
        .collect()
}

/// One repository's [`Stats`], as grouped by [`by_repo`].
#[derive(Debug, Clone)]
pub struct RepoStats {
    /// The grouping key: `RunState.repo` exactly as recorded, a normalised
    /// full path. Never a display name — this is what a caller (the CLI's
    /// `--repo` fallback, the web `?repo=` query) matches back against, and
    /// matching by name would conflate two different checkouts that happen
    /// to share a leaf directory.
    pub repo: PathBuf,
    /// Display name: `repo`'s file name, or the full path when it has none
    /// (e.g. `/`). Collisions between repositories are the caller's problem
    /// to disambiguate (see `report::repo_summary`), not this struct's.
    pub name: String,
    /// This repository's own aggregate, counted exactly as [`collect`]
    /// counts the whole workload.
    pub stats: Stats,
}

/// Group `states` by [`RunState::repo`] and aggregate each group with
/// [`collect_refs`] — the same counting logic as [`collect`], just scoped to
/// one repository at a time.
///
/// Sorted by run count descending, then by repo path ascending on ties, so
/// the busiest repository leads the summary table.
pub fn by_repo(states: &[RunState]) -> Vec<RepoStats> {
    let mut groups: BTreeMap<PathBuf, Vec<&RunState>> = BTreeMap::new();
    for state in states {
        groups.entry(state.repo.clone()).or_default().push(state);
    }

    let mut out: Vec<RepoStats> = groups
        .into_iter()
        .map(|(repo, group)| {
            let stats = collect_refs(group);
            let name = repo
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| repo.to_string_lossy().into_owned());
            RepoStats { repo, name, stats }
        })
        .collect();
    out.sort_by(|a, b| {
        b.stats
            .totals
            .runs
            .cmp(&a.stats.totals.runs)
            .then(a.repo.cmp(&b.repo))
    });
    out
}

/// Narrow `states` to those recorded against `repo` exactly.
///
/// Matches [`RunState::repo`] by full-path equality only, never by name — the
/// name-fallback resolution a caller may want (a checkout that has since
/// moved or been deleted) belongs one layer up, where a config or filesystem
/// lookup can decide what "the same repository" means; this function has no
/// such context and would otherwise risk conflating two different checkouts
/// that happen to share a leaf directory.
pub fn filter_repo<'a>(states: &'a [RunState], repo: &Path) -> Vec<&'a RunState> {
    states.iter().filter(|s| s.repo == repo).collect()
}

/// Aggregate `states`.
pub fn collect(states: &[RunState]) -> Stats {
    collect_refs(states)
}

/// Aggregate `states`, over any iterator of references rather than a slice —
/// what [`by_repo`] uses to run the same counting logic over each repository's
/// own group without collecting it into an owned `Vec<RunState>` first.
/// [`collect`] is a thin wrapper around this for the common whole-slice case.
pub fn collect_refs<'a>(states: impl IntoIterator<Item = &'a RunState>) -> Stats {
    let states: Vec<&'a RunState> = states.into_iter().collect();
    let mut totals = Totals::default();
    let mut agents: BTreeMap<String, AgentStats> = BTreeMap::new();
    let mut reviewers: BTreeMap<String, ReviewerStats> = BTreeMap::new();
    let mut advisors: BTreeMap<String, AdvisorStats> = BTreeMap::new();
    let mut e2e = E2eStats::default();
    let mut release_bumps = ReleaseBumpStats::default();

    for state in &states {
        totals.runs += 1;
        if state.status == RunStatus::Merged {
            release_bumps.merged += 1;
            if let Some(b) = &state.release_bump {
                release_bumps.recorded += 1;
                if b.pr_url.is_some() {
                    release_bumps.pr_opened += 1;
                }
                if b.automerge_enabled {
                    release_bumps.automerge_enabled += 1;
                }
                if b.merged_directly {
                    release_bumps.merged_directly += 1;
                }
            }
            if state.needs_attention() {
                release_bumps.needs_attention += 1;
            }
        }
        match state.status {
            RunStatus::Merged => totals.merged += 1,
            RunStatus::Ready => totals.ready += 1,
            RunStatus::Blocked => totals.blocked += 1,
            RunStatus::Failed => totals.failed += 1,
            RunStatus::Stalled => totals.stalled += 1,
            RunStatus::VerifiedNoop => totals.verified_noop += 1,
            // Counted on its own rather than folded into `blocked`/`stalled`:
            // the task it belongs to already landed through a later run,
            // which is the one this tally counts as the merge/ready outcome,
            // and folding it back in would inflate the denominator with two
            // outcomes for one task.
            RunStatus::Superseded => totals.superseded += 1,
            RunStatus::Prep
            | RunStatus::Implementing
            | RunStatus::Judging
            | RunStatus::Deliberating
            | RunStatus::Voting
            | RunStatus::Reviewing
            | RunStatus::Gating
            | RunStatus::Landing => totals.in_progress += 1,
        }

        for c in &state.candidates {
            let entry = agents.entry(c.agent.clone()).or_insert_with(|| AgentStats {
                agent: c.agent.clone(),
                ..AgentStats::default()
            });
            // A verified no-op is not counted as the ordinary empty loss it
            // would otherwise look like: the candidate gave evidence for
            // writing nothing, which `entry.empty` exists to flag the
            // *absence* of.
            if c.empty && c.verified_noop.is_none() {
                entry.empty += 1;
            }
            if c.viable() {
                entry.entered += 1;
            }
        }

        if let Some(t) = &state.tally {
            // A tally with no panel (`uncontested`) never split, never
            // deliberated and never converged — it never happened, and
            // folding it into the denominator would understate the real
            // split rate with runs that carry no panel-agreement signal at
            // all. The winner still earns its agent a win either way: an
            // uncontested candidate is still the one that shipped.
            if t.uncontested.is_none() {
                totals.tallied += 1;
                if !t.unanimous_initial {
                    totals.split += 1;
                }
                if t.deliberated {
                    totals.deliberated += 1;
                    if t.changed_votes > 0 {
                        totals.minds_changed += 1;
                    }
                    if t.unanimous_final {
                        totals.converged += 1;
                    }
                }
            }
            if let Some(w) = state.candidates.iter().find(|c| c.label == t.winner) {
                agents
                    .entry(w.agent.clone())
                    .or_insert_with(|| AgentStats {
                        agent: w.agent.clone(),
                        ..AgentStats::default()
                    })
                    .wins += 1;
            }
        }

        if let Some(advice) = &state.advice {
            for rec in &advice.records {
                let entry = advisors
                    .entry(rec.agent.clone())
                    .or_insert_with(|| AdvisorStats {
                        agent: rec.agent.clone(),
                        ..AdvisorStats::default()
                    });
                entry.seated += 1;
                if rec.proposal.is_none() {
                    entry.absent += 1;
                    continue;
                }
                entry.proposed += 1;
                match rec.reflection {
                    crate::advise::Reflection::Strong => entry.strong += 1,
                    crate::advise::Reflection::Faint => entry.faint += 1,
                    // A proposal exists, so this is not a real "no proposal"
                    // reading — see `AdvisorStats::absent`'s own doc for why
                    // that count comes from `proposal.is_none()` instead of
                    // this field. `graph::Runner::advise` always calls
                    // `apply_reflection` before saving, so the only way a
                    // proposed record keeps the default `Absent` is a run.json
                    // predating the `reflection` field. Fold it into `faint`
                    // rather than dropping it from the breakdown entirely:
                    // that is what `classify()` itself falls back to when
                    // there is nothing to score against.
                    crate::advise::Reflection::Absent => entry.faint += 1,
                }
            }
        }

        for round in &state.reviews {
            totals.review_rounds += 1;

            // A round whose fixer never reported back (crashed, timed out, or
            // replied with something magi could not parse) leaves adoption
            // unknown, not zero. Counting it would score every reviewer in
            // that round as having been ignored, when the truth is simply
            // unrecorded — so it stays out of the adoption-rate denominator
            // entirely rather than silently becoming a round of 0 adoptions.
            let report_lost = round.fix.as_ref().is_some_and(|f| f.failed.is_some());
            let adopted: Vec<&String> = round
                .fix
                .as_ref()
                .map(|f| f.addressed.iter().collect())
                .unwrap_or_default();

            for rec in &round.reviews {
                let entry = reviewers
                    .entry(rec.agent.clone())
                    .or_insert_with(|| ReviewerStats {
                        agent: rec.agent.clone(),
                        ..ReviewerStats::default()
                    });
                // Seating and answering are facts about the seat itself: they
                // hold whether or not this round's adoption is scoreable, so
                // they are counted before the lost-report guard. A seat that
                // never answered stays out of every scoring denominator —
                // silence is not a review that found nothing.
                entry.seated += 1;
                if rec.failed.is_some() {
                    entry.timeouts += 1;
                    continue;
                }
                if report_lost {
                    continue;
                }
                entry.rounds += 1;
                entry.submitted += rec.findings.len();
                for f in &rec.findings {
                    if adopted.iter().any(|a| **a == f.id) {
                        entry.adopted += 1;
                    }
                    let overlapped = round
                        .reviews
                        .iter()
                        .filter(|other| other.reviewer != rec.reviewer)
                        .flat_map(|other| other.findings.iter())
                        .any(|g| same_defect(f, g));
                    if !overlapped {
                        entry.unique += 1;
                    }
                }
            }

            if round.e2e_deferred {
                e2e.deferred += 1;
            } else if !round.e2e.is_empty() {
                e2e.rounds += 1;
                if round.e2e.iter().any(|o| !o.ok()) {
                    e2e.failures += 1;
                    if round.blocking == 0 {
                        e2e.sole_detections += 1;
                    }
                }
            }
        }
    }

    let mut agents: Vec<AgentStats> = agents.into_values().collect();
    agents.sort_by(|a, b| {
        b.win_rate()
            .total_cmp(&a.win_rate())
            .then(b.entered.cmp(&a.entered))
    });
    let mut reviewers: Vec<ReviewerStats> = reviewers.into_values().collect();
    // A seat only sighted in rounds whose adoption could not be scored has
    // nothing to report: no scoreable round, no silence to flag. It stays out
    // of the table entirely rather than appearing as a row of zeroes, which
    // would read as a reviewer that produced nothing.
    reviewers.retain(|r| r.rounds > 0 || r.timeouts > 0);
    reviewers.sort_by(|a, b| {
        b.adopted_per_round()
            .total_cmp(&a.adopted_per_round())
            .then(b.rounds.cmp(&a.rounds))
    });

    let mut advisors: Vec<AdvisorStats> = advisors.into_values().collect();
    advisors.sort_by(|a, b| {
        b.reflection_rate()
            .total_cmp(&a.reflection_rate())
            .then(b.proposed.cmp(&a.proposed))
    });

    let nodes = node_durations(states);

    Stats {
        totals,
        agents,
        reviewers,
        advisors,
        e2e,
        nodes,
        release_bumps,
    }
}

/// Do two findings describe the same defect?
///
/// A deliberate heuristic: same normalised title, or the same file within five
/// lines. Two reviewers rarely word a finding identically, and exact matching
/// would report every overlap as a unique find.
fn same_defect(a: &crate::verdict::Finding, b: &crate::verdict::Finding) -> bool {
    if normalize(&a.title) == normalize(&b.title) {
        return true;
    }
    match (&a.file, &b.file) {
        (Some(fa), Some(fb)) if fa == fb => match (a.line, b.line) {
            (Some(la), Some(lb)) => la.abs_diff(lb) <= 5,
            _ => false,
        },
        _ => false,
    }
}

fn normalize(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::{Candidate, CommandOutcome, FixRecord, ReviewRecord, ReviewRound, Tally};
    use crate::verdict::{Finding, Severity};
    use std::path::PathBuf;

    fn finding(id: &str, file: &str, line: u32, title: &str, sev: Severity) -> Finding {
        Finding {
            id: id.to_owned(),
            severity: sev,
            file: Some(file.to_owned()),
            line: Some(line),
            title: title.to_owned(),
            detail: String::new(),
        }
    }

    fn candidate(label: char, agent: &str) -> Candidate {
        Candidate {
            index: 0,
            label,
            agent: agent.to_owned(),
            branch: format!("magi/x/{label}"),
            worktree: PathBuf::from("/w"),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }
    }

    fn state_with(reviews: Vec<ReviewRound>, winner: char, status: RunStatus) -> RunState {
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abcdef".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        s.candidates = vec![candidate('A', "alpha"), candidate('B', "beta")];
        s.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1), ('B', 2)]),
            borda: BTreeMap::new(),
            winner,
            rankings: 3,
            unanimous_initial: false,
            deliberated: true,
            changed_votes: 1,
            unanimous_final: true,
            tie_break: None,
            judges: 3,
            present: 3,
            quorum: 2,
            met_quorum: true,
            uncontested: None,
        });
        s.reviews = reviews;
        s.status = status;
        s
    }

    fn state_with_repo(
        reviews: Vec<ReviewRound>,
        winner: char,
        status: RunStatus,
        repo: &str,
    ) -> RunState {
        let mut s = state_with(reviews, winner, status);
        s.repo = PathBuf::from(repo);
        s
    }

    #[test]
    fn win_rates_and_completion_are_counted_per_agent() {
        let states = vec![
            state_with(Vec::new(), 'B', RunStatus::Merged),
            state_with(Vec::new(), 'A', RunStatus::Blocked),
        ];
        let stats = collect(&states);
        assert_eq!(stats.totals.runs, 2);
        assert_eq!(stats.totals.merged, 1);
        assert_eq!(stats.totals.blocked, 1);
        assert_eq!(stats.totals.completion_rate(), 50.0);
        assert_eq!(stats.totals.split, 2);
        assert_eq!(stats.totals.minds_changed, 2);
        assert_eq!(stats.totals.converged, 2);

        let beta = stats.agents.iter().find(|a| a.agent == "beta").unwrap();
        assert_eq!(beta.entered, 2);
        assert_eq!(beta.wins, 1);
        assert_eq!(beta.win_rate(), 50.0);
    }

    #[test]
    fn reviewer_precision_and_uniqueness() {
        let round = ReviewRound {
            round: 1,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![
                ReviewRecord {
                    attempts: 0,
                    reviewer: 1,
                    agent: "alpha".to_owned(),
                    summary: String::new(),
                    findings: vec![
                        finding(
                            "R1-1-1",
                            "src/a.rs",
                            10,
                            "panics on empty",
                            Severity::Blocker,
                        ),
                        finding("R1-1-2", "src/b.rs", 40, "leaks a handle", Severity::Major),
                    ],
                    vote: None,
                    failed: None,
                    duration_ms: 0,
                },
                ReviewRecord {
                    attempts: 0,
                    reviewer: 2,
                    agent: "beta".to_owned(),
                    summary: String::new(),
                    // Same defect as R1-1-1, three lines off: an overlap.
                    findings: vec![finding(
                        "R1-2-1",
                        "src/a.rs",
                        13,
                        "empty input panic",
                        Severity::Blocker,
                    )],
                    vote: None,
                    failed: None,
                    duration_ms: 0,
                },
            ],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(FixRecord {
                agent: "alpha".to_owned(),
                addressed: vec!["R1-1-1".to_owned()],
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: None,
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 3,
            answered: 2,
            expected: 2,
            clean: false,
            progressed: true,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let stats = collect(&[state_with(vec![round], 'A', RunStatus::Ready)]);
        let alpha = stats.reviewers.iter().find(|r| r.agent == "alpha").unwrap();
        assert_eq!(alpha.submitted, 2);
        assert_eq!(alpha.adopted, 1);
        assert_eq!(alpha.precision(), 50.0);
        assert_eq!(alpha.adopted_per_round(), 1.0);
        // The src/a.rs finding overlaps beta's; src/b.rs does not.
        assert_eq!(alpha.unique, 1);

        let beta = stats.reviewers.iter().find(|r| r.agent == "beta").unwrap();
        assert_eq!(beta.submitted, 1);
        assert_eq!(beta.adopted, 0);
        assert_eq!(beta.unique, 0);
    }

    #[test]
    fn a_lost_fix_report_does_not_count_as_zero_adoption() {
        let submitted = ReviewRound {
            round: 1,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![ReviewRecord {
                attempts: 0,
                reviewer: 1,
                agent: "alpha".to_owned(),
                summary: String::new(),
                findings: vec![finding(
                    "R1-1-1",
                    "src/a.rs",
                    10,
                    "panics on empty",
                    Severity::Blocker,
                )],
                vote: None,
                failed: None,
                duration_ms: 0,
            }],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            // The fixer's diff may well have landed (blocking counts do fall
            // round over round) — only its adoption report never came back.
            fix: Some(FixRecord {
                agent: "alpha".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: Some("unparsable fix report".to_owned()),
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 4,
            answered: 1,
            expected: 1,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let stats = collect(&[state_with(vec![submitted], 'A', RunStatus::Ready)]);
        assert!(
            stats.reviewers.is_empty(),
            "a round with no adoption signal must not enter any reviewer's \
             denominator: {:?}",
            stats.reviewers
        );
    }

    #[test]
    fn timed_out_seat_counts_as_a_timeout_not_a_clean_submission() {
        let round = ReviewRound {
            round: 1,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![
                ReviewRecord {
                    attempts: 0,
                    reviewer: 1,
                    agent: "alpha".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: None,
                    failed: None,
                    duration_ms: 0,
                },
                ReviewRecord {
                    attempts: 0,
                    reviewer: 2,
                    agent: "beta".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: None,
                    failed: Some("agent timed out".to_owned()),
                    duration_ms: 0,
                },
            ],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 1,
            expected: 2,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let stats = collect(&[state_with(vec![round], 'A', RunStatus::Blocked)]);

        let alpha = stats.reviewers.iter().find(|r| r.agent == "alpha").unwrap();
        assert_eq!(alpha.seated, 1);
        assert_eq!(alpha.rounds, 1);
        assert_eq!(alpha.timeouts, 0);
        assert_eq!(alpha.submitted, 0);

        let beta = stats.reviewers.iter().find(|r| r.agent == "beta").unwrap();
        assert_eq!(beta.seated, 1);
        assert_eq!(beta.timeouts, 1);
        assert_eq!(beta.submitted, 0);
        // A timeout must never read as a submission with nothing found: it
        // stays out of the scoring denominators entirely rather than becoming
        // a 0/0 that looks identical to a reviewer who answered and passed.
        assert_eq!(beta.rounds, 0);
        assert_eq!(beta.timeout_rate(), 100.0);
    }

    #[test]
    fn a_timeout_is_still_recorded_when_the_round_also_lost_its_fix_report() {
        // Two independent gaps in one round: `beta` never answered, and the
        // fixer's adoption report never came back. The lost report suppresses
        // adoption scoring (see `a_lost_fix_report_does_not_count_as_zero_
        // adoption`) — it must not also swallow the fact that a seat was
        // silent, which is a property of the seat and not of the fixer.
        let round = ReviewRound {
            round: 1,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![
                ReviewRecord {
                    attempts: 0,
                    reviewer: 1,
                    agent: "alpha".to_owned(),
                    summary: String::new(),
                    findings: vec![finding(
                        "R1-1-1",
                        "src/a.rs",
                        10,
                        "panics on empty",
                        Severity::Blocker,
                    )],
                    vote: None,
                    failed: None,
                    duration_ms: 0,
                },
                ReviewRecord {
                    attempts: 0,
                    reviewer: 2,
                    agent: "beta".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: None,
                    failed: Some("agent timed out".to_owned()),
                    duration_ms: 0,
                },
            ],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(FixRecord {
                agent: "alpha".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: Some("unparsable fix report".to_owned()),
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 1,
            answered: 1,
            expected: 2,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let stats = collect(&[state_with(vec![round], 'A', RunStatus::Blocked)]);

        let beta = stats.reviewers.iter().find(|r| r.agent == "beta").unwrap();
        assert_eq!(beta.timeouts, 1);
        assert_eq!(beta.timeout_rate(), 100.0);
        // `alpha` answered, so the lost report keeps it out of the table
        // altogether — nothing about its findings can be scored.
        assert!(
            !stats.reviewers.iter().any(|r| r.agent == "alpha"),
            "{:?}",
            stats.reviewers
        );
    }

    #[test]
    fn e2e_sole_detection_needs_a_clean_static_review() {
        let fail = CommandOutcome {
            command: "cargo test".to_owned(),
            code: Some(101),
            output_tail: "boom".to_owned(),
            duration_ms: 1,
            resource_blocked: false,
        };
        let sole = ReviewRound {
            round: 1,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: vec![fail.clone()],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let alongside = ReviewRound {
            round: 2,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: vec![fail],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 2,
            answered: 0,
            expected: 0,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let stats = collect(&[state_with(vec![sole, alongside], 'A', RunStatus::Ready)]);
        assert_eq!(stats.e2e.rounds, 2);
        assert_eq!(stats.e2e.failures, 2);
        assert_eq!(stats.e2e.sole_detections, 1);
        assert_eq!(stats.e2e.sole_rate(), 50.0);
    }

    #[test]
    fn every_run_status_lands_in_exactly_one_breakdown_bucket() {
        let states = vec![
            state_with(Vec::new(), 'A', RunStatus::Merged),
            state_with(Vec::new(), 'A', RunStatus::Ready),
            state_with(Vec::new(), 'A', RunStatus::Blocked),
            state_with(Vec::new(), 'A', RunStatus::Failed),
            state_with(Vec::new(), 'A', RunStatus::Stalled),
            state_with(Vec::new(), 'A', RunStatus::VerifiedNoop),
            state_with(Vec::new(), 'A', RunStatus::Superseded),
            state_with(Vec::new(), 'A', RunStatus::Implementing),
            state_with(Vec::new(), 'A', RunStatus::Landing),
        ];
        let stats = collect(&states);
        let t = &stats.totals;
        assert_eq!(t.runs, 9);
        assert_eq!(t.merged, 1);
        assert_eq!(t.ready, 1);
        assert_eq!(t.blocked, 1);
        assert_eq!(t.failed, 1);
        assert_eq!(t.stalled, 1);
        assert_eq!(t.verified_noop, 1);
        assert_eq!(t.superseded, 1);
        // `Implementing` and `Landing` both fall into the one non-terminal
        // bucket.
        assert_eq!(t.in_progress, 2);
        assert_eq!(
            t.merged
                + t.ready
                + t.blocked
                + t.failed
                + t.stalled
                + t.verified_noop
                + t.superseded
                + t.in_progress,
            t.runs,
            "every run must land in exactly one bucket of the breakdown"
        );
    }

    #[test]
    fn empty_input_yields_zeroed_rates_not_nan() {
        let stats = collect(&[]);
        assert_eq!(stats.totals.completion_rate(), 0.0);
        assert_eq!(stats.totals.split_rate(), 0.0);
        assert_eq!(stats.e2e.sole_rate(), 0.0);
        assert!(stats.agents.is_empty());
        assert!(stats.advisors.is_empty());
        assert_eq!(AdvisorStats::default().reflection_rate(), 0.0);
    }

    fn advisor_record(
        seat: &str,
        agent: &str,
        proposal: Option<crate::verdict::Proposal>,
        reflection: crate::advise::Reflection,
    ) -> crate::advise::AdvisorRecord {
        crate::advise::AdvisorRecord {
            seat: seat.to_owned(),
            agent: agent.to_owned(),
            proposal,
            error: None,
            duration_ms: 0,
            reflection,
        }
    }

    fn a_proposal() -> crate::verdict::Proposal {
        crate::verdict::Proposal {
            approach: "do the thing".to_owned(),
            key_tradeoff: "speed over memory".to_owned(),
            risks: Vec::new(),
            touches: Vec::new(),
            why_not_naive: "the naive version breaks under load".to_owned(),
        }
    }

    #[test]
    fn advisor_stats_count_proposed_absent_and_reflection_split() {
        use crate::advise::{Advice, Reflection};

        let mut s = state_with(Vec::new(), 'A', RunStatus::Merged);
        s.advice = Some(Advice {
            records: vec![
                advisor_record("advisor-1", "alpha", Some(a_proposal()), Reflection::Strong),
                advisor_record("advisor-2", "alpha", Some(a_proposal()), Reflection::Faint),
                advisor_record("advisor-3", "alpha", None, Reflection::Absent),
            ],
            synthesis: Some("blended brief".to_owned()),
        });

        let stats = collect(&[s]);
        let alpha = stats.advisors.iter().find(|a| a.agent == "alpha").unwrap();
        assert_eq!(alpha.seated, 3);
        assert_eq!(alpha.proposed, 2);
        assert_eq!(alpha.absent, 1);
        assert_eq!(alpha.strong, 1);
        assert_eq!(alpha.faint, 1);
        assert_eq!(alpha.reflection_rate(), 50.0);
    }

    #[test]
    fn advisor_stats_count_absent_from_the_proposal_not_the_reflection_default() {
        // A record whose `proposal` is `None` but whose `reflection` was
        // never classified (predates `apply_reflection`, or the field's own
        // serde default) must still count as `absent` — and a run whose
        // synthesis never ran leaves every *proposed* record `Faint` by the
        // same default, which must not spill into `absent` either.
        //
        // A third case: a proposed record whose `reflection` was *never*
        // classified at all (a run.json predating the `reflection` field)
        // must not vanish from the breakdown either — it has to land
        // somewhere in faint/strong, not be silently dropped from all three
        // counters while still counting toward `proposed`.
        use crate::advise::{Advice, Reflection};

        let mut s = state_with(Vec::new(), 'A', RunStatus::Merged);
        s.advice = Some(Advice {
            records: vec![
                advisor_record("advisor-1", "alpha", None, Reflection::Absent),
                advisor_record("advisor-2", "alpha", Some(a_proposal()), Reflection::Faint),
                advisor_record("advisor-3", "alpha", Some(a_proposal()), Reflection::Absent),
            ],
            synthesis: None,
        });

        let stats = collect(&[s]);
        let alpha = stats.advisors.iter().find(|a| a.agent == "alpha").unwrap();
        assert_eq!(alpha.seated, 3);
        assert_eq!(alpha.proposed, 2);
        assert_eq!(alpha.absent, 1);
        assert_eq!(alpha.faint, 2);
        assert_eq!(alpha.strong, 0);
    }

    #[test]
    fn advisor_stats_ignore_runs_with_advise_off() {
        let s = state_with(Vec::new(), 'A', RunStatus::Merged);
        assert!(s.advice.is_none());
        let stats = collect(&[s]);
        assert!(stats.advisors.is_empty());
    }

    #[test]
    fn same_defect_matches_titles_across_files() {
        let a = finding("1", "src/a.rs", 1, "Panics On Empty!", Severity::Major);
        let b = finding("2", "src/z.rs", 900, "panics on empty", Severity::Nit);
        assert!(same_defect(&a, &b));
        let c = finding("3", "src/z.rs", 900, "totally different", Severity::Nit);
        assert!(!same_defect(&a, &c));
    }

    #[test]
    fn release_bump_stats_split_clean_from_attention_and_track_coverage() {
        use crate::run::ReleaseBump;

        // Not recorded at all: uses the release-bump step? unknown.
        let unrecorded = state_with(Vec::new(), 'A', RunStatus::Merged);

        // Clean: automerge worked, nobody had to look at it.
        let mut automerged = state_with(Vec::new(), 'A', RunStatus::Merged);
        automerged.release_bump = Some(ReleaseBump {
            pr_url: Some("https://github.com/o/r/pull/1".to_owned()),
            version: Some("1.2.3".to_owned()),
            automerge_enabled: true,
            merged_directly: false,
            problem: None,
            action_required: None,
        });

        // Also clean, but automerge itself was rejected by GitHub and magi
        // merged the already-green PR directly - `automerge_enabled` reads
        // false here, and that must not make this count as needing a human.
        let mut merged_directly = state_with(Vec::new(), 'A', RunStatus::Merged);
        merged_directly.release_bump = Some(ReleaseBump {
            pr_url: Some("https://github.com/o/r/pull/2".to_owned()),
            version: Some("1.2.4".to_owned()),
            automerge_enabled: false,
            merged_directly: true,
            problem: None,
            action_required: None,
        });

        // Needs a human, PR opened.
        let mut blocked_with_pr = state_with(Vec::new(), 'A', RunStatus::Merged);
        blocked_with_pr.release_bump = Some(ReleaseBump {
            pr_url: Some("https://github.com/o/r/pull/3".to_owned()),
            version: Some("1.2.5".to_owned()),
            automerge_enabled: false,
            merged_directly: false,
            problem: Some("checks red".to_owned()),
            action_required: Some("look at the PR".to_owned()),
        });

        // Needs a human, no PR ever opened.
        let mut blocked_without_pr = state_with(Vec::new(), 'A', RunStatus::Merged);
        blocked_without_pr.release_bump = Some(ReleaseBump {
            pr_url: None,
            version: Some("1.2.6".to_owned()),
            automerge_enabled: false,
            merged_directly: false,
            problem: Some("gh pr create failed".to_owned()),
            action_required: Some("open the PR by hand".to_owned()),
        });

        let stats = collect(&[
            unrecorded,
            automerged,
            merged_directly,
            blocked_with_pr,
            blocked_without_pr,
        ]);
        let b = &stats.release_bumps;
        assert_eq!(b.merged, 5);
        assert_eq!(b.recorded, 4);
        assert_eq!(b.pr_opened, 3);
        assert_eq!(b.automerge_enabled, 1);
        assert_eq!(b.merged_directly, 1);
        assert_eq!(b.needs_attention, 2);
        assert_eq!(b.clean(), 2);
        // Clean and attention must always split `recorded` exactly.
        assert_eq!(b.clean() + b.needs_attention, b.recorded);
        assert_eq!(b.coverage_rate(), 80.0);
        assert!((b.automerge_rate() - 33.333_333_333_333_336).abs() < 1e-9);
        assert_eq!(b.attention_rate(), 50.0);
    }

    #[test]
    fn release_bump_ignores_runs_that_are_not_merged() {
        use crate::run::ReleaseBump;

        let mut blocked = state_with(Vec::new(), 'A', RunStatus::Blocked);
        blocked.release_bump = Some(ReleaseBump {
            pr_url: Some("https://github.com/o/r/pull/9".to_owned()),
            version: Some("9.9.9".to_owned()),
            automerge_enabled: true,
            merged_directly: false,
            problem: None,
            action_required: None,
        });

        let stats = collect(&[blocked]);
        let b = &stats.release_bumps;
        assert_eq!(b.merged, 0);
        assert_eq!(b.recorded, 0);
        assert_eq!(b.pr_opened, 0);
    }

    #[test]
    fn release_bump_stats_are_zero_on_merged_runs_with_no_bump_or_no_runs() {
        let stats = collect(&[state_with(Vec::new(), 'A', RunStatus::Merged)]);
        let b = &stats.release_bumps;
        assert_eq!(b.merged, 1);
        assert_eq!(b.recorded, 0);
        assert_eq!(b.coverage_rate(), 0.0);
        assert_eq!(b.automerge_rate(), 0.0);
        assert_eq!(b.attention_rate(), 0.0);
        assert_eq!(b.clean(), 0);

        let empty = collect(&[]);
        let b = &empty.release_bumps;
        assert_eq!(b.merged, 0);
        assert_eq!(b.coverage_rate(), 0.0);
        assert_eq!(b.automerge_rate(), 0.0);
        assert_eq!(b.attention_rate(), 0.0);
    }

    fn event(node: &str, at_secs: i64, message: &str) -> crate::run::Event {
        crate::run::Event {
            at: jiff::Timestamp::from_second(at_secs).unwrap(),
            node: node.to_owned(),
            message: message.to_owned(),
        }
    }

    fn state_with_events(events: Vec<crate::run::Event>) -> RunState {
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abcdef".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        s.events = events;
        s
    }

    #[test]
    fn a_node_with_multiple_events_spans_first_to_last() {
        let s = state_with_events(vec![
            event("implement", 1_000, "start"),
            event("implement", 1_030, "still running"),
            event("implement", 1_090, "done"),
        ]);
        let nodes = node_durations(&[s]);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node, "implement");
        assert_eq!(nodes[0].runs, 1);
        assert_eq!(nodes[0].total_secs, 90);
        assert_eq!(nodes[0].max_secs, 90);
        assert_eq!(nodes[0].single, 0);
        assert_eq!(nodes[0].mean_secs(), 90.0);
    }

    #[test]
    fn a_node_with_a_single_event_is_unmeasured_not_zero() {
        let s = state_with_events(vec![event("gate", 2_000, "ran once")]);
        let nodes = node_durations(&[s]);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node, "gate");
        assert_eq!(nodes[0].runs, 0);
        assert_eq!(nodes[0].total_secs, 0);
        assert_eq!(nodes[0].single, 1);
        // No measured runs: the mean must read as zero, never NaN or a
        // fabricated span from the lone event.
        assert_eq!(nodes[0].mean_secs(), 0.0);
    }

    #[test]
    fn a_run_with_no_events_produces_no_node_rows() {
        let s = state_with_events(vec![]);
        let nodes = node_durations(&[s]);
        assert!(nodes.is_empty());
    }

    #[test]
    fn multiple_runs_aggregate_the_same_node() {
        let a = state_with_events(vec![event("judge", 0, "start"), event("judge", 60, "done")]);
        let b = state_with_events(vec![
            event("judge", 0, "start"),
            event("judge", 200, "done"),
        ]);
        // A single-event run for the same node must add to `single` without
        // disturbing the measured runs' total or max.
        let c = state_with_events(vec![event("judge", 5, "start")]);
        let nodes = node_durations(&[a, b, c]);
        assert_eq!(nodes.len(), 1);
        let judge = &nodes[0];
        assert_eq!(judge.node, "judge");
        assert_eq!(judge.runs, 2);
        assert_eq!(judge.total_secs, 260);
        assert_eq!(judge.max_secs, 200);
        assert_eq!(judge.single, 1);
        assert_eq!(judge.mean_secs(), 130.0);
    }

    #[test]
    fn by_repo_splits_states_and_group_totals_sum_to_the_whole() {
        let states = vec![
            state_with_repo(Vec::new(), 'A', RunStatus::Merged, "/repos/a"),
            state_with_repo(Vec::new(), 'A', RunStatus::Blocked, "/repos/a"),
            state_with_repo(Vec::new(), 'B', RunStatus::Merged, "/repos/b"),
        ];
        let groups = by_repo(&states);
        assert_eq!(groups.len(), 2);

        let total_runs: usize = groups.iter().map(|g| g.stats.totals.runs).sum();
        assert_eq!(total_runs, collect(&states).totals.runs);
        let total_merged: usize = groups.iter().map(|g| g.stats.totals.merged).sum();
        assert_eq!(total_merged, collect(&states).totals.merged);

        // Busiest repository (2 runs) sorts first.
        assert_eq!(groups[0].repo, PathBuf::from("/repos/a"));
        assert_eq!(groups[0].name, "a");
        assert_eq!(groups[0].stats.totals.runs, 2);
        assert_eq!(groups[1].repo, PathBuf::from("/repos/b"));
        assert_eq!(groups[1].name, "b");
        assert_eq!(groups[1].stats.totals.runs, 1);
    }

    #[test]
    fn by_repo_breaks_a_run_count_tie_by_path() {
        let states = vec![
            state_with_repo(Vec::new(), 'A', RunStatus::Merged, "/repos/z"),
            state_with_repo(Vec::new(), 'A', RunStatus::Merged, "/repos/a"),
        ];
        let groups = by_repo(&states);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].repo, PathBuf::from("/repos/a"));
        assert_eq!(groups[1].repo, PathBuf::from("/repos/z"));
    }

    #[test]
    fn by_repo_on_empty_input_yields_no_groups() {
        assert!(by_repo(&[]).is_empty());
    }

    #[test]
    fn filter_repo_matches_the_full_path_exactly() {
        let states = vec![
            state_with_repo(Vec::new(), 'A', RunStatus::Merged, "/repos/a"),
            state_with_repo(Vec::new(), 'A', RunStatus::Merged, "/repos/ab"),
        ];
        let hits = filter_repo(&states, Path::new("/repos/a"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo, PathBuf::from("/repos/a"));
    }

    #[test]
    fn filter_repo_returns_nothing_for_an_unknown_repo_or_empty_input() {
        let states = vec![state_with_repo(
            Vec::new(),
            'A',
            RunStatus::Merged,
            "/repos/a",
        )];
        assert!(filter_repo(&states, Path::new("/repos/nope")).is_empty());
        assert!(filter_repo(&[], Path::new("/repos/a")).is_empty());
    }
}
