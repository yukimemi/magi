//! Aggregate statistics over every recorded run.
//!
//! These tables are a by-product of running the graph, not a benchmark. The
//! seat assignment rotates, the task distribution is whatever the operator
//! happened to ask for, and a model that draws harder tasks looks worse. Read
//! them as "relative performance on my workload", which is the only claim the
//! data supports.
use std::collections::BTreeMap;

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

/// Everything, aggregated.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    /// Run counters.
    pub totals: Totals,
    /// Per-agent implementation record, best win rate first.
    pub agents: Vec<AgentStats>,
    /// Per-agent review record, most adopted-per-round first.
    pub reviewers: Vec<ReviewerStats>,
    /// Verification record.
    pub e2e: E2eStats,
}

/// Load every run on disk, skipping any that cannot be read.
pub fn load_all() -> Vec<RunState> {
    list_ids()
        .into_iter()
        .filter_map(|id| RunState::load(&id).ok())
        .collect()
}

/// Aggregate `states`.
pub fn collect(states: &[RunState]) -> Stats {
    let mut totals = Totals::default();
    let mut agents: BTreeMap<String, AgentStats> = BTreeMap::new();
    let mut reviewers: BTreeMap<String, ReviewerStats> = BTreeMap::new();
    let mut e2e = E2eStats::default();

    for state in states {
        totals.runs += 1;
        match state.status {
            RunStatus::Merged => totals.merged += 1,
            RunStatus::Ready => totals.ready += 1,
            RunStatus::Blocked => totals.blocked += 1,
            RunStatus::Failed => totals.failed += 1,
            _ => {}
        }

        for c in &state.candidates {
            let entry = agents.entry(c.agent.clone()).or_insert_with(|| AgentStats {
                agent: c.agent.clone(),
                ..AgentStats::default()
            });
            if c.empty {
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

            if !round.e2e.is_empty() {
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

    Stats {
        totals,
        agents,
        reviewers,
        e2e,
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
            reviews: vec![
                ReviewRecord {
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
            fix: Some(FixRecord {
                agent: "alpha".to_owned(),
                addressed: vec!["R1-1-1".to_owned()],
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: None,
                duration_ms: 0,
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
            reviews: vec![ReviewRecord {
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
            reviews: vec![
                ReviewRecord {
                    reviewer: 1,
                    agent: "alpha".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: None,
                    failed: None,
                    duration_ms: 0,
                },
                ReviewRecord {
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
            reviews: vec![
                ReviewRecord {
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
            fix: Some(FixRecord {
                agent: "alpha".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: Some("unparsable fix report".to_owned()),
                duration_ms: 0,
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
        };
        let sole = ReviewRound {
            round: 1,
            head: "h".to_owned(),
            reviews: Vec::new(),
            e2e: vec![fail.clone()],
            verify_retried: false,
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
            reviews: Vec::new(),
            e2e: vec![fail],
            verify_retried: false,
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
    fn empty_input_yields_zeroed_rates_not_nan() {
        let stats = collect(&[]);
        assert_eq!(stats.totals.completion_rate(), 0.0);
        assert_eq!(stats.totals.split_rate(), 0.0);
        assert_eq!(stats.e2e.sole_rate(), 0.0);
        assert!(stats.agents.is_empty());
    }

    #[test]
    fn same_defect_matches_titles_across_files() {
        let a = finding("1", "src/a.rs", 1, "Panics On Empty!", Severity::Major);
        let b = finding("2", "src/z.rs", 900, "panics on empty", Severity::Nit);
        assert!(same_defect(&a, &b));
        let c = finding("3", "src/z.rs", 900, "totally different", Severity::Nit);
        assert!(!same_defect(&a, &c));
    }
}
