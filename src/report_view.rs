//! A structured view of the run report, for the web UI.
//!
//! [`crate::report::run`] renders a run as ANSI text, and that stays the one
//! text implementation (the TUI and `magi show` read it). This module is the
//! second reader of the same [`RunState`]: it decides *which sections exist
//! and in what order* the way `report::run` does, and leaves every judgement
//! to the accessors the text report already uses (`e2e_status`,
//! `gate_status`, `Severity::blocks`, `ReviewRound::incomplete`,
//! `final_votes`, `handed_off_with_open_findings`). Nothing here re-derives a
//! verdict; when a section is added to the text report, add it here too.
//!
//! The one rule a reader of this file must keep: a tally below quorum is
//! never *decided*. `Tally::decided` is `met_quorum`, and the winner it names
//! is `provisional` otherwise, so a stalled run cannot render as a settled one.
use std::collections::BTreeMap;

use serde::Serialize;

use crate::config::{MergeMode, MergeStyle};
use crate::run::{
    CommandOutcome, ContinuationOutcome, E2eStatus, GateStatus, JobStatus, Liveness,
    OperatorFixOutcome, ReviewRound, RunState, tail,
};
use crate::verdict::{Finding, ReviewVote, Severity};

/// Bumped when a field changes meaning; the client falls back to the raw text
/// for a schema it does not know rather than render a guess.
pub const SCHEMA: u32 = 1;

/// How many bytes of a command's output the view carries, the same bound the
/// text report applies.
const OUTPUT_TAIL: usize = 2_000;

/// How a section should read at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    /// Good, finished.
    Ok,
    /// Needs a look, nothing is broken.
    Warn,
    /// Broken or untrustworthy.
    Fail,
    /// Information only.
    Neutral,
}

/// The whole report.
#[derive(Debug, Serialize)]
pub struct RunReportView {
    /// [`SCHEMA`].
    pub schema: u32,
    /// What `report::run` prints before the first section.
    pub header: Header,
    /// In `report::run` order.
    pub sections: Vec<Section>,
    /// `report::active_seats`, which is already text and has no structure to
    /// add. Empty when no seat is mid-answer.
    pub active_seats: String,
}

/// The top block.
#[derive(Debug, Serialize)]
pub struct Header {
    /// Run id.
    pub id: String,
    /// `RunStatus::display_label`.
    pub status: String,
    /// Status as a tone; a stalled run is never `ok`.
    pub tone: Tone,
    /// Landed nothing, and that was the configuration.
    pub unmerged_by_design: bool,
    /// A release step needs a person.
    pub needs_attention: bool,
    /// Repository path.
    pub repo: String,
    /// Base branch.
    pub base_branch: String,
    /// Base commit, abbreviated.
    pub base_commit: String,
    /// Creation time, local.
    pub created: String,
    /// First line of the task.
    pub task: String,
    /// Where the task came from.
    pub origin: String,
    /// The run's state directory.
    pub state_dir: String,
}

/// Fields every section shares.
#[derive(Debug, Serialize)]
pub struct Head {
    /// Heading, as the text report words it.
    pub title: String,
    /// At-a-glance state.
    pub tone: Tone,
    /// Whether the card starts expanded.
    pub default_open: bool,
}

fn head(title: &str, tone: Tone, default_open: bool) -> Head {
    Head {
        title: title.to_owned(),
        tone,
        default_open,
    }
}

/// One card.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Section {
    /// Implementations.
    Candidates {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// Rows.
        items: Vec<CandidateRow>,
    },
    /// Blind judging, deliberation and final votes.
    Judging {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// One row per judge.
        judges: Vec<JudgeRow>,
        /// One line per deliberation turn.
        deliberation: Vec<String>,
        /// Final votes.
        votes: Vec<VoteRow>,
    },
    /// The mechanical count.
    Tally {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// The count.
        tally: TallyView,
    },
    /// Seats handed over, files withheld, blindness warnings: one line each.
    Notes {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// Which kind of note.
        key: &'static str,
        /// One line each.
        lines: Vec<String>,
    },
    /// Review + verification, grouped by round.
    Review {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// One entry per round, oldest first.
        rounds: Vec<RoundView>,
        /// Set when the loop handed off with findings still open.
        handed_off: Option<String>,
    },
    /// Fixes the operator asked for.
    OperatorFixes {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// One entry per request.
        requests: Vec<OperatorFixView>,
    },
    /// The winner against the base's tip.
    BaseSync {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// The state.
        base_sync: BaseSyncView,
    },
    /// Mechanical pre-gate commands.
    PreGate {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// Per-command results.
        commands: Vec<CommandView>,
        /// Mechanical fixes commit, abbreviated.
        committed: Option<String>,
    },
    /// The final gate.
    Gate {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// `passed`, `passed_no_commands` or `failed`.
        status: &'static str,
        /// Per-command results.
        commands: Vec<CommandView>,
    },
    /// What happened to the branch.
    Merge {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// The outcome.
        merge: MergeView,
    },
    /// Tasks filed from open findings.
    Followups {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// Rows.
        items: Vec<FollowupRow>,
    },
    /// Release bump after the merge.
    ReleaseBump {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// The bump.
        bump: ReleaseBumpView,
    },
    /// Where the winner lives.
    WinnerWorktree {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// Worktree path.
        worktree: String,
        /// Branch.
        branch: String,
        /// The tally behind it did not reach quorum.
        provisional: bool,
    },
    /// Commands the seats' own CLIs reported.
    Jobs {
        /// Title, tone and default state.
        #[serde(flatten)]
        head: Head,
        /// Per seat.
        groups: Vec<JobGroup>,
        /// Why absence is not evidence.
        coverage: &'static str,
        /// No evidence yet, though the roster can report it.
        empty_note: Option<&'static str>,
    },
}

/// One implementation.
#[derive(Debug, Serialize)]
pub struct CandidateRow {
    /// Blind label.
    pub label: String,
    /// Agent that wrote it.
    pub agent: String,
    /// `failed`, `verified_noop`, `no_change` or `changed`.
    pub state: &'static str,
    /// The failure, the no-op evidence or the candidate's summary, one line.
    pub detail: Option<String>,
    /// Files touched.
    pub files: usize,
    /// Commits ahead of base.
    pub commits: usize,
    /// Wall-clock seconds.
    pub duration_secs: u64,
    /// The tally's winner, and the tally met quorum.
    pub winner: bool,
    /// The tally's winner, but the tally did not meet quorum.
    pub provisional_winner: bool,
}

/// One judge's ranking.
#[derive(Debug, Serialize)]
pub struct JudgeRow {
    /// Seat number.
    pub judge: usize,
    /// Agent.
    pub agent: String,
    /// Ranking, best first; empty when none.
    pub ranking: String,
    /// Confidence, when given.
    pub confidence: Option<u8>,
    /// Why no ranking, when none.
    pub failed: Option<String>,
}

/// One judge's final vote.
#[derive(Debug, Serialize)]
pub struct VoteRow {
    /// Seat number.
    pub judge: usize,
    /// Agent.
    pub agent: String,
    /// The label voted for, `?` when unreadable.
    pub vote: String,
    /// Moved after deliberation.
    pub changed: bool,
}

/// The count.
#[derive(Debug, Serialize)]
pub struct TallyView {
    /// `present >= quorum`, or none was required.
    pub met_quorum: bool,
    /// Always `met_quorum`; named so the client has one field to gate on.
    pub decided: bool,
    /// The winner is only provisional.
    pub provisional: bool,
    /// Winning label.
    pub winner: String,
    /// Configured panel size.
    pub judges: usize,
    /// Judges who contributed.
    pub present: usize,
    /// Judges required.
    pub quorum: usize,
    /// Why no panel sat.
    pub uncontested: Option<String>,
    /// First choices per label.
    pub first_choice: BTreeMap<String, usize>,
    /// `none`, `one`, `unanimous` or `split`.
    pub initial: &'static str,
    /// Final votes agreed.
    pub unanimous_final: bool,
    /// Judges who moved.
    pub changed_votes: usize,
    /// How a tie was broken.
    pub tie_break: Option<String>,
    /// Seats lost to a rate limit.
    pub rate_limited: Vec<String>,
}

/// One review round.
#[derive(Debug, Serialize)]
pub struct RoundView {
    /// 1-based.
    pub round: usize,
    /// `clean`, `open` or `incomplete`.
    pub status: &'static str,
    /// Tone of the round.
    pub tone: Tone,
    /// Reviewed commit, abbreviated.
    pub head: String,
    /// Commit e2e ran against, when recorded.
    pub verified_head: Option<String>,
    /// Findings raised across reviewers.
    pub raised: usize,
    /// Blocking findings the round recorded.
    pub blocking: usize,
    /// Reviewers that answered.
    pub answered: usize,
    /// Reviewers expected.
    pub expected: usize,
    /// Seats that never answered, `review-N: why`.
    pub missing: Vec<String>,
    /// The e2e leg.
    pub e2e: E2eView,
    /// The panel's verdict.
    pub verdict: Option<VerdictView>,
    /// The fixer's report.
    pub fix: Option<FixView>,
    /// Per-command e2e results.
    pub commands: Vec<CommandView>,
    /// Per-reviewer votes.
    pub reviewers: Vec<ReviewerRow>,
    /// Every finding, plus declined ones the fixer named.
    pub findings: Vec<FindingView>,
    /// Revotes after a split.
    pub reconsideration: Vec<RevoteRow>,
    /// Starts expanded: the last round, or any round not clean.
    pub default_open: bool,
}

/// The e2e leg of a round.
#[derive(Debug, Serialize)]
pub struct E2eView {
    /// `pass`, `fail`, `blocked`, `deferred` or `not_configured`.
    pub state: &'static str,
    /// The failure was a build or link failure, not a verdict on the patch.
    pub build_failed: bool,
    /// Why it was deferred.
    pub defer_reason: Option<String>,
    /// Retried once.
    pub retried: bool,
}

/// A verdict vote.
#[derive(Debug, Serialize)]
pub struct VerdictView {
    /// `approve`, `approve_with_findings` or `reject`.
    pub vote: &'static str,
    /// Operator wording.
    pub label: &'static str,
    /// The panel split on the way here.
    pub split: bool,
}

/// What the fixer reported.
#[derive(Debug, Serialize)]
pub struct FixView {
    /// Agent.
    pub agent: String,
    /// Finding ids the fixer reported acting on.
    pub addressed: usize,
    /// Finding ids it declined.
    pub rejected: usize,
    /// The fix produced a commit.
    pub committed: bool,
    /// The tree changed this round.
    pub tree_changed: bool,
    /// Why the adoption report is missing; addressed/rejected are unknown then.
    pub report_lost: Option<String>,
    /// How the report was recovered, when it needed to be.
    pub continuation: Option<String>,
}

/// A reviewer's own line.
#[derive(Debug, Serialize)]
pub struct ReviewerRow {
    /// Seat number.
    pub reviewer: usize,
    /// Agent.
    pub agent: String,
    /// Initial vote.
    pub vote: Option<&'static str>,
    /// Why it produced nothing.
    pub failed: Option<String>,
}

/// A revote.
#[derive(Debug, Serialize)]
pub struct RevoteRow {
    /// Seat number.
    pub reviewer: usize,
    /// The vote, `None` when it was lost.
    pub vote: Option<&'static str>,
    /// Reason, or the failure.
    pub reason: String,
}

/// One finding.
#[derive(Debug, Serialize)]
pub struct FindingView {
    /// magi-assigned id.
    pub id: String,
    /// Reviewer seat that raised it; `None` for a declined id with no record.
    pub reviewer: Option<usize>,
    /// The real severity: `nit`, `minor`, `major` or `blocker`.
    pub severity: Option<&'static str>,
    /// One-line summary.
    pub title: String,
    /// File.
    pub file: Option<String>,
    /// Line.
    pub line: Option<u32>,
    /// Holds the merge (`Severity::blocks`).
    pub blocking: bool,
    /// `open`, `fixed` (the fixer *reported* acting on it) or `declined`.
    pub state: &'static str,
    /// Why the fixer declined.
    pub declined_why: Option<String>,
}

/// A shell command's outcome.
#[derive(Debug, Serialize)]
pub struct CommandView {
    /// The command.
    pub command: String,
    /// `pass`, `fail` or `blocked`.
    pub state: &'static str,
    /// Exit code.
    pub code: Option<i32>,
    /// Wall-clock milliseconds.
    pub duration_ms: u64,
    /// The last of the output; only for a command that did not pass.
    pub output_tail: Option<String>,
}

/// An operator-requested fix.
#[derive(Debug, Serialize)]
pub struct OperatorFixView {
    /// When asked.
    pub requested_at: String,
    /// The head was stale.
    pub stale: bool,
    /// Why.
    pub reason: String,
    /// Findings and what happened to each.
    pub findings: Vec<OperatorFindingRow>,
    /// The follow-up review's run.
    pub follow_up_review_run: Option<String>,
    /// Committed, but nothing re-verifies it.
    pub unverified_commit: bool,
}

/// One operator-fix finding.
#[derive(Debug, Serialize)]
pub struct OperatorFindingRow {
    /// Id.
    pub id: String,
    /// Severity.
    pub severity: &'static str,
    /// Title.
    pub title: String,
    /// `pending`, `addressed`, `rejected` or `unreported`.
    pub outcome: &'static str,
    /// Why, for a rejection.
    pub why: Option<String>,
}

/// The base-sync state.
#[derive(Debug, Serialize)]
pub struct BaseSyncView {
    /// Base branch.
    pub base_branch: String,
    /// Tip checked against.
    pub tip: String,
    /// `already_in`, `conflict`, `in_sync` or `behind`.
    pub state: &'static str,
    /// Commits behind.
    pub behind: usize,
    /// Rebase attempts.
    pub attempts: usize,
    /// First line of the conflict.
    pub conflict: Option<String>,
    /// `a as b (proof match)`, when already in the base.
    pub already_in: Option<String>,
}

/// What happened to the branch.
#[derive(Debug, Serialize)]
pub struct MergeView {
    /// Merge mode, lowercase.
    pub mode: String,
    /// `not_landed_by_design`, `ok`, `empty` or `not_merged`.
    pub state: &'static str,
    /// First line of magi's own detail.
    pub detail: String,
    /// The branch left unmerged, for the by-design case.
    pub branch: Option<String>,
    /// A squash merge inherits the placeholder subject.
    pub squash_caveat: bool,
    /// Checks that were red at merge.
    pub red_at_merge: Vec<String>,
}

/// A filed follow-up.
#[derive(Debug, Serialize)]
pub struct FollowupRow {
    /// Task id, abbreviated.
    pub task: String,
    /// Finding ids it carries.
    pub findings: Vec<String>,
}

/// The release bump.
#[derive(Debug, Serialize)]
pub struct ReleaseBumpView {
    /// Version.
    pub version: Option<String>,
    /// Pull request.
    pub pr_url: Option<String>,
    /// `enabled`, `merged_directly`, `local`, `failed` or `not_enabled`.
    pub automerge: &'static str,
    /// First line of the problem.
    pub problem: Option<String>,
    /// What a person must do.
    pub action_required: Option<String>,
}

/// Jobs of one seat.
#[derive(Debug, Serialize)]
pub struct JobGroup {
    /// Graph node.
    pub node: String,
    /// Seat.
    pub seat: String,
    /// Commands.
    pub jobs: Vec<JobRow>,
}

/// One reported command.
#[derive(Debug, Serialize)]
pub struct JobRow {
    /// The CLI's id.
    pub id: String,
    /// `completed`, `failed` or `unknown`.
    pub status: &'static str,
    /// Exit code.
    pub exit_code: Option<i32>,
    /// Review round.
    pub round: Option<usize>,
    /// When the evidence was read.
    pub checked_at: String,
    /// The command.
    pub description: String,
    /// Tail of its output.
    pub result_summary: String,
}

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn severity_name(s: Severity) -> &'static str {
    match s {
        Severity::Nit => "nit",
        Severity::Minor => "minor",
        Severity::Major => "major",
        Severity::Blocker => "blocker",
    }
}

fn vote_name(v: ReviewVote) -> &'static str {
    match v {
        ReviewVote::Approve => "approve",
        ReviewVote::ApproveWithFindings => "approve_with_findings",
        ReviewVote::Reject => "reject",
    }
}

fn command(o: &CommandOutcome, warn_instead_of_fail: bool) -> CommandView {
    let state = if o.resource_blocked {
        "blocked"
    } else if o.ok() {
        "pass"
    } else if warn_instead_of_fail {
        "warn"
    } else {
        "fail"
    };
    CommandView {
        command: o.command.clone(),
        state,
        code: o.code,
        duration_ms: o.duration_ms,
        output_tail: (!o.ok()).then(|| tail(&o.output_tail, OUTPUT_TAIL)),
    }
}

fn status_tone(state: &RunState) -> Tone {
    use crate::run::RunStatus as S;
    if state.needs_attention() {
        return Tone::Warn;
    }
    match state.status {
        S::Merged | S::Ready => Tone::Ok,
        S::Failed => Tone::Fail,
        S::Stalled | S::Blocked => Tone::Warn,
        _ => Tone::Neutral,
    }
}

fn round_view(r: &ReviewRound, last: bool) -> RoundView {
    let raised = r.reviews.iter().map(|x| x.findings.len()).sum();
    let status = if r.incomplete() {
        "incomplete"
    } else if r.clean {
        "clean"
    } else {
        "open"
    };
    let e2e_state = match r.e2e_status() {
        E2eStatus::NotConfigured => "not_configured",
        E2eStatus::Deferred => "deferred",
        E2eStatus::Passed => "pass",
        E2eStatus::Failed => "fail",
        E2eStatus::ResourceBlocked => "blocked",
    };
    let e2e = E2eView {
        state: e2e_state,
        build_failed: r.e2e_status() == E2eStatus::Failed
            && r.e2e.iter().any(CommandOutcome::build_failed),
        defer_reason: r
            .e2e_defer_reason
            .clone()
            .filter(|_| e2e_state == "deferred"),
        retried: r.verify_retried,
    };
    let tone = if e2e_state == "fail" && !e2e.build_failed {
        Tone::Fail
    } else if status == "clean" {
        Tone::Ok
    } else {
        Tone::Warn
    };
    let missing = if r.incomplete() {
        r.reviews
            .iter()
            .filter_map(|x| {
                x.failed
                    .as_ref()
                    .map(|why| format!("review-{}: {why}", x.reviewer))
            })
            .collect()
    } else {
        Vec::new()
    };
    let fix = r.fix.as_ref().map(|f| FixView {
        agent: f.agent.clone(),
        addressed: f.addressed.len(),
        rejected: f.rejected.len(),
        committed: f.committed,
        tree_changed: r.progressed,
        report_lost: f.failed.clone(),
        continuation: f.continuation.as_ref().and_then(|c| match c.outcome {
            ContinuationOutcome::NotNeeded => None,
            ContinuationOutcome::Resumed => Some(format!("resumed x{}", c.attempts)),
            ContinuationOutcome::Exhausted => Some(format!("exhausted x{}", c.attempts)),
            ContinuationOutcome::QuotaLost => Some("quota".to_owned()),
            ContinuationOutcome::NoSession => Some("no session to resume".to_owned()),
        }),
    });

    let mut findings: Vec<FindingView> = Vec::new();
    for rec in &r.reviews {
        for f in &rec.findings {
            findings.push(finding_view(r, rec.reviewer, f));
        }
    }
    // A declined id is shown once, on its own finding. One that matches no
    // finding (a fixer naming an id the record does not hold) still has to be
    // visible, never dropped.
    if let Some(fix) = &r.fix {
        for rej in &fix.rejected {
            if !findings.iter().any(|f| f.id == rej.id) {
                findings.push(FindingView {
                    id: rej.id.clone(),
                    reviewer: None,
                    severity: None,
                    title: String::new(),
                    file: None,
                    line: None,
                    blocking: false,
                    state: "declined",
                    declined_why: Some(rej.why.clone()),
                });
            }
        }
    }

    RoundView {
        round: r.round,
        status,
        tone,
        head: short(&r.head),
        verified_head: r.verified_head.as_deref().map(short),
        raised,
        blocking: r.blocking,
        answered: r.answered,
        expected: r.expected,
        missing,
        e2e,
        verdict: r.verdict.map(|v| VerdictView {
            vote: vote_name(v),
            label: v.label(),
            split: r.vote_split,
        }),
        fix,
        commands: r.e2e.iter().map(|o| command(o, false)).collect(),
        reviewers: r
            .reviews
            .iter()
            .map(|x| ReviewerRow {
                reviewer: x.reviewer,
                agent: x.agent.clone(),
                vote: x.vote.map(vote_name),
                failed: x.failed.clone(),
            })
            .collect(),
        findings,
        reconsideration: r
            .reconsideration
            .iter()
            .map(|rv| RevoteRow {
                reviewer: rv.reviewer,
                vote: rv.vote.map(vote_name),
                reason: match rv.vote {
                    Some(_) => rv.reason.clone(),
                    None => format!("no revote ({})", rv.failed.as_deref().unwrap_or("unknown")),
                },
            })
            .collect(),
        default_open: last || status != "clean",
    }
}

fn finding_view(r: &ReviewRound, reviewer: usize, f: &Finding) -> FindingView {
    let fixed = r
        .fix
        .as_ref()
        .is_some_and(|fix| fix.addressed.contains(&f.id));
    let declined = r
        .fix
        .as_ref()
        .and_then(|fix| fix.rejected.iter().find(|x| x.id == f.id));
    let (state, declined_why) = match (fixed, declined) {
        (true, _) => ("fixed", None),
        (false, Some(rej)) => ("declined", Some(rej.why.clone())),
        (false, None) => ("open", None),
    };
    FindingView {
        id: f.id.clone(),
        reviewer: Some(reviewer),
        severity: Some(severity_name(f.severity)),
        title: f.title.clone(),
        file: f.file.clone(),
        line: f.line,
        blocking: f.severity.blocks(),
        state,
        declined_why,
    }
}

fn jobs_section(state: &RunState) -> Option<Section> {
    let codex = state
        .config
        .agents
        .iter()
        .any(|a| a.kind == crate::config::AgentKind::Codex);
    if state.jobs.is_empty() && !codex {
        return None;
    }
    let mut by_seat: BTreeMap<(&str, &str), Vec<&crate::run::JobRecord>> = BTreeMap::new();
    for j in &state.jobs {
        by_seat
            .entry((j.node.as_str(), j.seat.as_str()))
            .or_default()
            .push(j);
    }
    let groups = by_seat
        .into_iter()
        .map(|((node, seat), records)| JobGroup {
            node: node.to_owned(),
            seat: seat.to_owned(),
            jobs: records
                .into_iter()
                .map(|j| JobRow {
                    id: j.id.clone(),
                    status: match j.status {
                        JobStatus::Completed => "completed",
                        JobStatus::Failed => "failed",
                        JobStatus::Unknown => "unknown",
                    },
                    exit_code: j.exit_code,
                    round: j.round,
                    checked_at: j.checked_at.to_string(),
                    description: first_line(&j.description),
                    result_summary: tail(&j.result_summary, OUTPUT_TAIL),
                })
                .collect(),
        })
        .collect();
    Some(Section::Jobs {
        head: head("background jobs", Tone::Neutral, false),
        groups,
        coverage: "adapter coverage: codex only today; other backends, and a command a CLI \
                   never reported finishing, leave no entry here - that is unknown, never \
                   \"nothing ran\"",
        empty_note: state.jobs.is_empty().then_some(
            "no completed command evidence yet for this run (see active seats for what is \
             still mid-turn)",
        ),
    })
}

/// Build the view. `live` is [`RunState::liveness`], exactly what the text
/// route passes to `report::active_seats`.
pub fn build(state: &RunState, live: Liveness) -> RunReportView {
    let mut sections: Vec<Section> = Vec::new();
    let met_quorum = state.tally.as_ref().is_none_or(|t| t.met_quorum);

    sections.push(Section::Candidates {
        head: head(
            "candidates",
            if state.candidates.iter().any(|c| c.failed.is_some()) {
                Tone::Warn
            } else {
                Tone::Neutral
            },
            true,
        ),
        items: state
            .candidates
            .iter()
            .map(|c| {
                let is_winner = state.tally.as_ref().is_some_and(|t| t.winner == c.label);
                let (st, detail) = match (&c.failed, c.empty, &c.verified_noop) {
                    (Some(e), _, _) => ("failed", Some(e.clone())),
                    (None, true, Some(ev)) => ("verified_noop", Some(first_line(ev))),
                    (None, true, None) => ("no_change", None),
                    _ => (
                        "changed",
                        Some(first_line(&c.summary)).filter(|s| !s.is_empty()),
                    ),
                };
                CandidateRow {
                    label: c.label.to_string(),
                    agent: c.agent.clone(),
                    state: st,
                    detail,
                    files: c.files,
                    commits: c.commits,
                    duration_secs: c.duration_ms / 1000,
                    winner: is_winner && met_quorum,
                    provisional_winner: is_winner && !met_quorum,
                }
            })
            .collect(),
    });

    if !state.judgements.is_empty() {
        sections.push(Section::Judging {
            head: head("blind judging", Tone::Neutral, false),
            judges: state
                .judgements
                .iter()
                .map(|j| JudgeRow {
                    judge: j.judge,
                    agent: j.agent.clone(),
                    ranking: j.ranking.iter().collect(),
                    confidence: j.confidence,
                    failed: j.failed.clone(),
                })
                .collect(),
            deliberation: state
                .tally
                .as_ref()
                .filter(|t| t.deliberated)
                .map(|_| {
                    state
                        .deliberation
                        .iter()
                        .flat_map(|round| {
                            round.turns.iter().map(move |turn| {
                                format!(
                                    "r{} judge {} -> {}",
                                    round.round,
                                    turn.judge,
                                    turn.tentative.map_or("-".to_owned(), |c| c.to_string())
                                )
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            votes: state
                .votes
                .iter()
                .map(|v| VoteRow {
                    judge: v.judge,
                    agent: v.agent.clone(),
                    vote: v.vote.unwrap_or('?').to_string(),
                    changed: v.changed,
                })
                .collect(),
        });
    }

    if let Some(t) = &state.tally {
        let initial = match (t.rankings, t.unanimous_initial) {
            (0, _) => "none",
            (1, _) => "one",
            (_, true) => "unanimous",
            (_, false) => "split",
        };
        sections.push(Section::Tally {
            head: head(
                "tally",
                if t.met_quorum { Tone::Ok } else { Tone::Fail },
                true,
            ),
            tally: TallyView {
                met_quorum: t.met_quorum,
                decided: t.met_quorum,
                provisional: !t.met_quorum,
                winner: t.winner.to_string(),
                judges: t.judges,
                present: t.present,
                quorum: t.quorum,
                uncontested: t.uncontested.clone(),
                first_choice: t
                    .first_choice
                    .iter()
                    .map(|(k, v)| (k.to_string(), *v))
                    .collect(),
                initial,
                unanimous_final: t.unanimous_final,
                changed_votes: t.changed_votes,
                tie_break: t.tie_break.clone(),
                rate_limited: state.quota.iter().map(|q| q.seat.clone()).collect(),
            },
        });
    }

    let notes = |key: &'static str, title: &str, tone: Tone, lines: Vec<String>| {
        (!lines.is_empty()).then(|| Section::Notes {
            head: head(title, tone, true),
            key,
            lines,
        })
    };
    sections.extend(notes(
        "handovers",
        "seats handed over",
        Tone::Warn,
        state
            .handovers
            .iter()
            .map(|h| {
                format!(
                    "{}  {} -> {}  ({}: {})",
                    h.seat, h.from, h.to, h.node, h.reason
                )
            })
            .collect(),
    ));
    sections.extend(notes(
        "withheld",
        "withheld from commit",
        Tone::Warn,
        state
            .withheld
            .iter()
            .map(|w| {
                format!(
                    "{}  ({} lockfile; the directory uses {})",
                    w.path, w.manager, w.kept_by
                )
            })
            .collect(),
    ));

    if !state.reviews.is_empty() {
        let n = state.reviews.len();
        let rounds: Vec<RoundView> = state
            .reviews
            .iter()
            .enumerate()
            .map(|(i, r)| round_view(r, i + 1 == n))
            .collect();
        let handed_off = state.handed_off_with_open_findings().then(|| {
            format!(
                "handed off with {} finding(s) still open - gate and e2e were green; see above \
                 for what a person should still look at",
                state.open_findings().len()
            )
        });
        let tone = if handed_off.is_some() {
            Tone::Warn
        } else {
            rounds.last().map_or(Tone::Neutral, |r| r.tone)
        };
        sections.push(Section::Review {
            head: head("review + verification", tone, true),
            rounds,
            handed_off,
        });
    }

    if !state.operator_fixes.is_empty() {
        sections.push(Section::OperatorFixes {
            head: head("operator fix(es)", Tone::Neutral, true),
            requests: state
                .operator_fixes
                .iter()
                .map(|req| OperatorFixView {
                    requested_at: req.requested_at.to_string(),
                    stale: req.stale,
                    reason: req.reason.clone(),
                    findings: req
                        .findings
                        .iter()
                        .map(|f| {
                            let (outcome, why) = match &f.outcome {
                                OperatorFixOutcome::Pending => ("pending", None),
                                OperatorFixOutcome::Addressed => ("addressed", None),
                                OperatorFixOutcome::Rejected { why } => {
                                    ("rejected", Some(why.clone()))
                                }
                                OperatorFixOutcome::Unreported => ("unreported", None),
                            };
                            OperatorFindingRow {
                                id: f.id.clone(),
                                severity: severity_name(f.severity),
                                title: f.title.clone(),
                                outcome,
                                why,
                            }
                        })
                        .collect(),
                    follow_up_review_run: req.follow_up_review_run.clone(),
                    unverified_commit: req.follow_up_review_run.is_none()
                        && req.fix.as_ref().is_some_and(|fx| fx.committed),
                })
                .collect(),
        });
    }

    if let Some(bs) = &state.base_sync {
        let (st, tone) = if bs.already_in.is_some() {
            ("already_in", Tone::Neutral)
        } else if bs.conflict.is_some() {
            ("conflict", Tone::Fail)
        } else if bs.behind == 0 {
            ("in_sync", Tone::Ok)
        } else {
            ("behind", Tone::Warn)
        };
        sections.push(Section::BaseSync {
            head: head("base sync", tone, true),
            base_sync: BaseSyncView {
                base_branch: state.base_branch.clone(),
                tip: short(&bs.tip),
                state: st,
                behind: bs.behind,
                attempts: bs.attempts,
                conflict: bs.conflict.as_deref().map(first_line),
                already_in: bs
                    .already_in
                    .as_ref()
                    .map(|a| format!("{} ({} match)", a.names(), a.proof.as_str())),
            },
        });
    }

    if !state.pre_gate.is_empty() {
        let all_ok = state.pre_gate.iter().all(CommandOutcome::ok);
        sections.push(Section::PreGate {
            head: head(
                "pre_gate",
                if all_ok { Tone::Ok } else { Tone::Warn },
                !all_ok,
            ),
            commands: state.pre_gate.iter().map(|o| command(o, true)).collect(),
            committed: state.pre_gate_commit.as_deref().map(short),
        });
    }

    match state.gate_status() {
        GateStatus::NotRun => {}
        gs => {
            let st = match gs {
                GateStatus::PassedWithNoCommands => "passed_no_commands",
                GateStatus::Passed => "passed",
                _ => "failed",
            };
            sections.push(Section::Gate {
                head: head("gate", if gs.ok() { Tone::Ok } else { Tone::Fail }, true),
                status: st,
                commands: state.gate.iter().map(|o| command(o, false)).collect(),
            });
        }
    }

    if let Some(m) = &state.merge {
        let by_design = m.mode == MergeMode::None;
        let (st, tone) = if by_design {
            ("not_landed_by_design", Tone::Neutral)
        } else if m.ok {
            ("ok", Tone::Ok)
        } else if m.empty {
            ("empty", Tone::Warn)
        } else {
            ("not_merged", Tone::Warn)
        };
        sections.push(Section::Merge {
            head: head("merge", tone, true),
            merge: MergeView {
                mode: format!("{:?}", m.mode).to_lowercase(),
                state: st,
                detail: m.detail.lines().next().unwrap_or("").to_owned(),
                branch: by_design
                    .then(|| state.winner().map(|w| w.branch.clone()))
                    .flatten(),
                squash_caveat: by_design && state.config.merge.style == MergeStyle::Squash,
                red_at_merge: state
                    .pr
                    .as_ref()
                    .map(|pr| pr.red_at_merge.clone())
                    .unwrap_or_default(),
            },
        });
    }

    if !state.followups.is_empty() {
        sections.push(Section::Followups {
            head: head("follow-ups", Tone::Neutral, true),
            items: state
                .followups
                .iter()
                .map(|f| FollowupRow {
                    task: crate::queue::short(&f.task).to_owned(),
                    findings: f.findings.clone(),
                })
                .collect(),
        });
    }

    if let Some(b) = &state.release_bump {
        let automerge = if b.automerge_enabled {
            "enabled"
        } else if b.merged_directly {
            "merged_directly"
        } else if b.local {
            "local"
        } else if b.problem.is_some() {
            "failed"
        } else {
            "not_enabled"
        };
        sections.push(Section::ReleaseBump {
            head: head(
                "release bump",
                if automerge == "failed" || b.action_required.is_some() {
                    Tone::Warn
                } else {
                    Tone::Neutral
                },
                true,
            ),
            bump: ReleaseBumpView {
                version: b.version.clone(),
                pr_url: b.pr_url.clone(),
                automerge,
                problem: b.problem.as_deref().map(first_line),
                action_required: b.action_required.clone(),
            },
        });
    }

    sections.extend(notes(
        "leaks",
        "blindness warnings",
        Tone::Warn,
        state
            .leaks
            .iter()
            .map(|l| format!("{} x{} in {}", l.token, l.count, l.site))
            .collect(),
    ));

    if let Some(w) = state.winner()
        && !w.folded
    {
        sections.push(Section::WinnerWorktree {
            head: head("winner worktree", Tone::Neutral, true),
            worktree: w.worktree.display().to_string(),
            branch: w.branch.clone(),
            provisional: !met_quorum,
        });
    }

    sections.extend(jobs_section(state));

    RunReportView {
        schema: SCHEMA,
        header: Header {
            id: state.id.clone(),
            status: state.status.display_label().to_owned(),
            tone: status_tone(state),
            unmerged_by_design: state.unmerged_by_design(),
            needs_attention: state.needs_attention(),
            repo: state.repo.display().to_string(),
            base_branch: state.base_branch.clone(),
            base_commit: short(&state.base_commit),
            created: state.created_local(),
            task: first_line(&state.instruction),
            origin: crate::run::origin_label(state.origin.as_ref()),
            state_dir: state.dir().display().to_string(),
        },
        sections,
        active_seats: crate::report::active_seats(state, live),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::{CommandOutcome, FixRecord, ReviewRecord, Tally};
    use crate::verdict::Rejection;
    use std::path::PathBuf;

    fn state() -> RunState {
        // `build` names `state.dir()`, which reads the process-global home.
        crate::run::pin_test_home();
        RunState::new(
            PathBuf::from("/repo/magi"),
            "main".to_owned(),
            "0123456789abcdef".to_owned(),
            "Add a thing\n\nlong".to_owned(),
            Config::default(),
        )
    }

    fn finding(id: &str, severity: Severity) -> Finding {
        Finding {
            id: id.to_owned(),
            severity,
            file: Some("src/a.rs".to_owned()),
            line: Some(7),
            title: format!("title {id}"),
            detail: String::new(),
        }
    }

    fn cmd(code: Option<i32>, blocked: bool) -> CommandOutcome {
        CommandOutcome {
            command: "cargo test".to_owned(),
            code,
            output_tail: "boom\n".repeat(1_000),
            duration_ms: 5,
            resource_blocked: blocked,
        }
    }

    fn round(n: usize) -> ReviewRound {
        let mut r: ReviewRound = serde_json::from_value(serde_json::json!({
            "round": n, "head": "abcdef0123", "reviews": []
        }))
        .expect("round");
        r.answered = 1;
        r.expected = 1;
        r.reviews.push(
            serde_json::from_value::<ReviewRecord>(serde_json::json!({
                "reviewer": 1, "agent": "a"
            }))
            .expect("record"),
        );
        r
    }

    fn tally(met: bool) -> Tally {
        serde_json::from_value(serde_json::json!({
            "first_choice": {"A": 1}, "borda": {"A": 2}, "winner": "A",
            "unanimous_initial": true, "deliberated": false, "changed_votes": 0,
            "unanimous_final": true, "judges": 3, "present": if met { 3 } else { 1 },
            "quorum": 2, "met_quorum": met, "rankings": 1
        }))
        .expect("tally")
    }

    fn kinds(v: &RunReportView) -> Vec<String> {
        serde_json::to_value(&v.sections)
            .expect("json")
            .as_array()
            .expect("array")
            .iter()
            .map(|s| s["kind"].as_str().expect("kind").to_owned())
            .collect()
    }

    #[test]
    fn sections_follow_the_text_report_order() {
        let mut s = state();
        s.tally = Some(tally(true));
        s.reviews.push(round(1));
        s.base_sync = Some(
            serde_json::from_value(serde_json::json!({"tip": "abc", "behind": 0, "attempts": 0}))
                .expect("sync"),
        );
        s.pre_gate.push(cmd(Some(0), false));
        s.jobs.push(
            serde_json::from_value(serde_json::json!({
                "node": "fix", "seat": "fix", "id": "j1", "description": "ls",
                "checked_at": "2026-01-01T00:00:00Z", "status": "Completed",
                "exit_code": 0, "source": "codex"
            }))
            .expect("job"),
        );
        let v = build(&s, Liveness::Unknown);
        assert_eq!(
            kinds(&v),
            [
                "candidates",
                "tally",
                "review",
                "base_sync",
                "pre_gate",
                "jobs"
            ]
        );
        let json = serde_json::to_value(&v).expect("json");
        assert_eq!(json["schema"], SCHEMA);
        let jobs = json["sections"].as_array().expect("a").last().expect("l");
        assert_eq!(jobs["default_open"], false, "jobs start collapsed");
        assert_eq!(jobs["groups"][0]["jobs"][0]["status"], "completed");
    }

    #[test]
    fn a_stalled_tally_is_never_decided() {
        let mut s = state();
        s.tally = Some(tally(false));
        let json = serde_json::to_value(build(&s, Liveness::Unknown)).expect("json");
        let t = json["sections"]
            .as_array()
            .expect("a")
            .iter()
            .find(|x| x["kind"] == "tally")
            .expect("tally");
        assert_eq!(t["tally"]["met_quorum"], false);
        assert_eq!(t["tally"]["decided"], false);
        assert_eq!(t["tally"]["provisional"], true);
        assert_eq!(t["tone"], "fail");
    }

    #[test]
    fn findings_keep_severity_blocking_and_what_the_fixer_said() {
        let mut s = state();
        let mut r = round(1);
        r.reviews[0].findings = vec![
            finding("R1-1-1", Severity::Blocker),
            finding("R1-1-2", Severity::Major),
            finding("R1-1-3", Severity::Minor),
            finding("R1-1-4", Severity::Nit),
        ];
        r.fix = Some(
            serde_json::from_value::<FixRecord>(serde_json::json!({"agent": "f"})).expect("fix"),
        );
        let fix = r.fix.as_mut().expect("fix");
        fix.addressed = vec!["R1-1-1".to_owned()];
        fix.rejected = vec![
            Rejection {
                id: "R1-1-2".to_owned(),
                why: "not a bug".to_owned(),
            },
            Rejection {
                id: "R9-9-9".to_owned(),
                why: "unknown id".to_owned(),
            },
        ];
        s.reviews.push(r);
        let json = serde_json::to_value(build(&s, Liveness::Unknown)).expect("json");
        let review = json["sections"]
            .as_array()
            .expect("a")
            .iter()
            .find(|x| x["kind"] == "review")
            .expect("review");
        let f = &review["rounds"][0]["findings"];
        let at = |i: usize| {
            (
                f[i]["severity"].clone(),
                f[i]["blocking"].clone(),
                f[i]["state"].clone(),
            )
        };
        assert_eq!(at(0), ("blocker".into(), true.into(), "fixed".into()));
        assert_eq!(at(1), ("major".into(), true.into(), "declined".into()));
        assert_eq!(f[1]["declined_why"], "not a bug");
        assert_eq!(at(2), ("minor".into(), false.into(), "open".into()));
        assert_eq!(at(3), ("nit".into(), false.into(), "open".into()));
        assert_eq!(
            f[4]["id"], "R9-9-9",
            "an unmatched declined id is not dropped"
        );
        assert_eq!(f[4]["state"], "declined");
    }

    #[test]
    fn a_deferred_e2e_is_not_a_failed_one() {
        let mut s = state();
        let mut deferred = round(1);
        deferred.e2e_deferred = true;
        deferred.e2e_defer_reason = Some("blocking findings".to_owned());
        let mut failed = round(2);
        failed.e2e = vec![cmd(Some(1), false)];
        let mut blocked = round(3);
        blocked.e2e = vec![cmd(None, true)];
        let none = round(4);
        s.reviews = vec![deferred, failed, blocked, none];
        let json = serde_json::to_value(build(&s, Liveness::Unknown)).expect("json");
        let rounds = &json["sections"]
            .as_array()
            .expect("a")
            .iter()
            .find(|x| x["kind"] == "review")
            .expect("review")["rounds"];
        let st = |i: usize| {
            rounds[i]["e2e"]["state"]
                .as_str()
                .expect("state")
                .to_owned()
        };
        assert_eq!(
            [st(0), st(1), st(2), st(3)],
            ["deferred", "fail", "blocked", "not_configured"]
        );
        assert_eq!(rounds[0]["e2e"]["defer_reason"], "blocking findings");
        let tail = rounds[1]["commands"][0]["output_tail"]
            .as_str()
            .expect("tail");
        assert!(tail.len() < 2_100, "output is bounded: {}", tail.len());
    }
}
