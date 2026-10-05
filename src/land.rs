//! Landing the winner: watch the pull request, fix what it complains about,
//! and merge it.
//!
//! Opening the pull request used to be where magi stopped and the operator
//! started: watch the checks, read what the review bots found, push a fix,
//! wait again, merge. That loop is mechanical, it takes an hour of wall-clock
//! time per pull request, and doing it by hand six times in one session is how
//! a queue that drains unattended stops being unattended. So it lives here.
//!
//! # Shape
//!
//! [`PrState`] is one observation of a pull request and [`decide`] is the whole
//! policy as a *pure* function of it. Nothing in [`decide`] talks to `gh`,
//! which is what makes "green with an unresolved comment is a fix, not a merge"
//! an assertion in a test rather than a claim in a comment. [`land`] is the
//! only part that performs I/O: observe, decide, act, repeat.
//!
//! # What it refuses to do
//!
//! Merging is the one irreversible thing magi can do to a repository, so the
//! loop is built to stop rather than to guess:
//!
//! * A red pull request is never merged. When the budget runs out the pull
//!   request is left open with a comment naming what is still failing, because
//!   a magi that force-merges a red pull request is worse than one that stops.
//! * A pull request whose checks cannot be read at all (`gh` reported no
//!   rollup) is not merged either. Landing is for repositories with CI; with no
//!   signal there is nothing to be green.
//! * A pull request a human merged or closed underneath us is
//!   [`Step::Done`] - the person won, and their decision is not an error.
//!
//! # Why `--subject` is not optional
//!
//! A candidate branch holds one commit whose subject is
//! `magi: candidate A (uncommitted work)`, and `gh pr merge --squash` prefers a
//! single commit's message over the pull request title. Merging without
//! [`merge_argv`]'s explicit `--subject` therefore writes a `main` history that
//! says nothing about what landed. `AGENTS.md` records the trap; this module is
//! where it is prevented.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::ask;
use crate::config::{AgentSpec, MergeMode};
use crate::git;
use crate::proc::Quiet as _;
use crate::prompt;
use crate::run::{ContestedHandoff, LandApproval, MergeOutcome, RunState, RunStatus, tail};

/// How often the pull request is re-read while its checks are still running.
///
/// Thirty seconds: a CI matrix takes minutes, so anything shorter is spent
/// entirely on `gh` invocations, and anything much longer adds latency to every
/// single round of a loop that already waits for agents.
pub const POLL: Duration = Duration::from_secs(30);

/// How long one wait may last before landing gives up on the checks finishing.
///
/// A workflow that has not settled in forty-five minutes is stuck on a runner
/// queue, a missing approval, or a hung job - none of which more polling fixes,
/// and all of which a person needs to see.
pub const WAIT_CEILING: Duration = Duration::from_secs(45 * 60);

/// How long the checks may stay unreadable before landing gives up on them.
///
/// GitHub registers a workflow run some seconds after the branch is pushed, so
/// immediately after a pull request is opened "no checks" and "no CI in this
/// repository" look identical. Measured on run 01c2: magi opened pull request
/// 22, read `unknown` four seconds later, refused to merge on a guess and
/// marked the run blocked - and every check on that pull request was green
/// minutes afterwards, with the whole competition then re-run from scratch for
/// a task that was already finished. Three minutes is well past the observed
/// registration delay and still bounded, so a repository that genuinely has no
/// checks costs one three-minute wait and then says so.
pub const CHECKS_GRACE: Duration = Duration::from_secs(3 * 60);

/// Bytes of failing log kept per check. The fixer needs the assertion and the
/// frame around it, not the forty thousand lines of `cargo` output above it.
const LOG_TAIL: usize = 4_000;

/// Failing checks whose logs are fetched. Beyond a handful the failures share a
/// cause, and fetching each one costs a `gh` round trip.
const MAX_LOGS: usize = 3;

/// Marker carried by every comment magi posts on a pull request.
///
/// Without it magi's own "still failing" comment is indistinguishable from a
/// reviewer's, and the next observation would hand magi's own prose to the
/// fixer as a finding.
pub const MARKER: &str = "<!-- magi:land -->";

/// Markers a bot puts in a comment to say that the comment is not a review.
///
/// CodeRabbit labels its own machinery in HTML comments - the trigger notice,
/// the walkthrough summary, the "thanks for using" footer - and its actual
/// findings arrive as *inline* review comments with a path and a line. Taking
/// the bot at its word is more honest than guessing from prose, and it is the
/// difference between a fix round that has something to fix and one that asks
/// an agent to act on a quota notice.
const NOT_A_REVIEW: [&str; 3] = [
    "skip review by coderabbit.ai",
    "summarize by coderabbit.ai",
    "<!-- tips_start -->",
];

/// Where a pull request is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLifecycle {
    /// Still ours to land.
    Open,
    /// Already merged, by us or by a person.
    Merged,
    /// Closed without merging.
    Closed,
}

/// The aggregate verdict of a pull request's checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checks {
    /// At least one check has not finished.
    Pending,
    /// Every check passed (a skipped check counts as passed: the review
    /// workflow skips release and bot pull requests by design).
    Green,
    /// At least one check finished without passing.
    Red,
    /// Nothing readable - no rollup at all, or a status magi does not know.
    Unknown,
}

impl PrLifecycle {
    /// Stable lower-case name, as the API and the reports spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

impl Checks {
    /// Stable lower-case name, as the API and the reports spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Green => "green",
            Self::Red => "red",
            Self::Unknown => "unknown",
        }
    }
}

/// One outstanding review comment, human or bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    /// Login of whoever wrote it.
    pub author: String,
    /// File it was left on, for inline review comments.
    pub path: Option<String>,
    /// Line it was left on, when the comment is inline and still anchored.
    pub line: Option<u64>,
    /// The comment itself, as written.
    pub body: String,
}

/// One observation of a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrState {
    /// Pull request url, as `gh` reports it.
    pub url: String,
    /// Pull request number.
    pub number: u64,
    /// Open, merged, or closed.
    pub state: PrLifecycle,
    /// Aggregate check verdict.
    pub checks: Checks,
    /// Names of the checks that finished without passing.
    pub failing: Vec<String>,
    /// Comments that still want an answer, human and bot.
    pub review_comments: Vec<ReviewComment>,
    /// Whether the forge itself considers the failures blocking.
    pub blocking: Blocking,
}

/// Whether a failing check actually stands between the pull request and
/// `main`, according to the forge.
///
/// The rollup lists every check equally, so `coverage` going red on a
/// repository that deliberately does not require it looked exactly like a
/// broken build - and magi answered by spending a fix round on a change that
/// was fine. Pull request 37 had to be merged by hand for that reason: the
/// only red check was `editorconfig`, which was failing because the *action*
/// could not fetch its own binary, and which the repository does not require.
///
/// `mergeStateStatus` is where GitHub applies the required-check set, so it
/// is the one field that can tell the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocking {
    /// Required checks are satisfied and the branch merges cleanly.
    No,
    /// Something required is failing or missing.
    Yes,
    /// The branch no longer merges: the base moved under it.
    Conflict,
    /// The forge did not say - an older `gh`, or a token without the scope.
    /// Treated as `Yes`, because refusing to guess is the rule everywhere
    /// else in this module.
    Unsaid,
}

impl Blocking {
    /// Read `mergeStateStatus`, which is upper-case in `gh`'s output.
    fn of(raw: &str) -> Self {
        match raw.to_ascii_uppercase().as_str() {
            // Mergeable. `UNSTABLE` is the interesting one: mergeable, with a
            // non-required check failing or still running.
            "CLEAN" | "UNSTABLE" | "HAS_HOOKS" => Self::No,
            "DIRTY" => Self::Conflict,
            "" | "UNKNOWN" => Self::Unsaid,
            // BLOCKED, BEHIND, DRAFT: something has to change first.
            _ => Self::Yes,
        }
    }

    /// Does this stand between the pull request and the base branch?
    #[must_use]
    pub fn stops_a_merge(self) -> bool {
        !matches!(self, Self::No)
    }
}

/// What the loop decided to do next. Pure, so the policy is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Checks are still running; re-read the pull request after [`POLL`].
    Wait,
    /// The base moved and the branch no longer merges: rebase it.
    ///
    /// Not a fix round. Nothing is wrong with the change - a competition
    /// that runs for two hours against a repository merging pull requests
    /// all day conflicts on the way in, and that is arithmetic rather than a
    /// defect. Pull requests 35 and 37 were both rebased by hand for exactly
    /// this.
    Rebase,
    /// Red checks or unresolved comments; run a fix round.
    Fix {
        /// What is unhappy, in one line, for the run log and the fix prompt.
        reason: String,
    },
    /// Green and nothing outstanding; merge it.
    Merge,
    /// The pull request left our hands.
    Done {
        /// Did it land, or was it closed?
        merged: bool,
    },
    /// Stop and leave the pull request to a person.
    GiveUp {
        /// Why magi stopped, in one line.
        reason: String,
    },
}

/// The outcome to record when `gh pr merge` exits non-zero, given what the
/// pull request looked like immediately afterwards.
///
/// `gh pr merge` merges server-side first and only then does local work -
/// deleting the branch, switching back to a base branch - so a non-zero exit
/// does not mean the merge did not happen. In a jj-colocated repository it
/// reliably does not mean that: git HEAD is detached, and `--delete-branch`
/// ends with "could not determine current branch: not on any branch" *after*
/// the merge has landed. Run ec12 merged pull request 28 into `main` and
/// recorded `ok: false`, and its task was held waiting for a merge that was
/// already done.
///
/// So the forge is asked, and its answer wins - the same authority [`decide`]
/// gives the pull request's own state over everything else. The recorded
/// detail carries both facts, because "the command failed and the merge
/// happened anyway" is exactly what someone reading the run later needs to
/// know.
///
/// `None` means the merge really did not happen, including when the pull
/// request could not be read at all: an unreadable answer is not evidence of
/// success.
pub(crate) fn merged_after_all(
    argv: &[String],
    stderr: &str,
    after: Option<PrLifecycle>,
) -> Option<MergeOutcome> {
    if after? != PrLifecycle::Merged {
        return None;
    }
    Some(MergeOutcome {
        mode: MergeMode::Pr,
        ok: true,
        detail: format!(
            "gh {} (the command reported `{}`, but the pull request is merged)",
            argv.join(" "),
            stderr.trim()
        ),
        empty: false,
    })
}

/// Decide the next step. No I/O.
///
/// `round` counts the fix rounds already spent, so `round == budget` means the
/// budget is gone. A wait never spends a round: waiting is free, and a slow CI
/// must not consume the allowance meant for actual fixes.
///
/// The order of the tests is the policy:
///
/// 1. **The pull request's own state wins.** A merge or a close that happened
///    underneath us is the end of the story regardless of what the checks say.
/// 2. **Pending beats red.** A check that is still running may yet fail, and one
///    fix round that addresses every failure is cheaper than two that each
///    address half - the fix pushes and restarts the whole suite anyway.
/// 3. **Comments outrank green.** An unresolved comment holds the merge even
///    when CI is happy; that is what a review is for.
/// 4. **Unreadable is not absent.** Checks that cannot be read yet are waited
///    on for [`CHECKS_GRACE`], because a pull request opened a moment ago has
///    not been given its workflow runs yet. Past the grace they are treated as
///    genuinely missing and magi stops rather than merge on a guess.
pub fn decide(pr: &PrState, round: usize, budget: usize, waited: Duration) -> Step {
    match pr.state {
        PrLifecycle::Merged => return Step::Done { merged: true },
        PrLifecycle::Closed => return Step::Done { merged: false },
        PrLifecycle::Open => {}
    }

    // Before the checks: every check on a branch that cannot land is an
    // answer about a state that cannot land.
    if pr.blocking == Blocking::Conflict {
        return Step::Rebase;
    }

    let spent = round >= budget;
    match pr.checks {
        Checks::Pending => Step::Wait,
        Checks::Unknown if waited < CHECKS_GRACE => Step::Wait,
        Checks::Unknown => Step::GiveUp {
            reason: format!(
                "no check status is readable on the pull request after {} minute(s); \
                 refusing to merge on a guess",
                CHECKS_GRACE.as_secs() / 60
            ),
        },
        // Red, but the forge says it does not stand in the way: the failing
        // checks are ones this repository chose not to require. Spending a fix
        // round on them asks an agent to repair something nobody is gating on
        // - and pull request 37's only red check was an *action* that could
        // not fetch its own binary. Merge, and name them so the record is
        // honest about what was red when it landed.
        Checks::Red if !pr.blocking.stops_a_merge() && pr.review_comments.is_empty() => Step::Merge,
        Checks::Red => {
            let what = format!(
                "{} check(s) failing: {}",
                pr.failing.len(),
                pr.failing.join(", ")
            );
            if spent {
                Step::GiveUp {
                    reason: format!("{what} — still red after {budget} fix round(s)"),
                }
            } else {
                Step::Fix { reason: what }
            }
        }
        Checks::Green if pr.review_comments.is_empty() => Step::Merge,
        Checks::Green => {
            let what = format!(
                "checks are green but {} review comment(s) are unresolved: {}",
                pr.review_comments.len(),
                authors(&pr.review_comments)
            );
            if spent {
                Step::GiveUp {
                    reason: format!("{what} — still unresolved after {budget} fix round(s)"),
                }
            } else {
                Step::Fix { reason: what }
            }
        }
    }
}

/// Distinct comment authors, in the order they first appear.
fn authors(comments: &[ReviewComment]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for c in comments {
        if !seen.contains(&c.author.as_str()) {
            seen.push(&c.author);
        }
    }
    seen.join(", ")
}

/// The argv magi merges with, minus the program name.
///
/// `--subject` is the point of this function existing: see the module docs.
pub fn merge_argv(number: u64, subject: &str) -> Vec<String> {
    vec![
        "pr".to_owned(),
        "merge".to_owned(),
        number.to_string(),
        "--squash".to_owned(),
        "--delete-branch".to_owned(),
        "--subject".to_owned(),
        subject.to_owned(),
    ]
}

/// [`merge_argv`] pinned to the commit the decision was made about.
///
/// `--match-head-commit` makes the forge refuse the merge if the branch moved
/// after it was observed, so a push landing between the look and the merge is
/// never merged unseen.
pub fn merge_argv_at(number: u64, subject: &str, head: &str) -> Vec<String> {
    let mut argv = merge_argv(number, subject);
    argv.push("--match-head-commit".to_owned());
    argv.push(head.to_owned());
    argv
}

/// Arm GitHub auto-merge on the commit the owner approved.
///
/// The forge then waits for the required checks against its own view and
/// merges only while the head is still exactly `head`, so a stale observation
/// on magi's side can never become an unverified merge. Same squash and
/// subject as [`merge_argv`]; `--admin` is deliberately never passed.
pub fn automerge_argv_at(number: u64, subject: &str, head: &str) -> Vec<String> {
    let mut argv = merge_argv_at(number, subject, head);
    argv.push("--auto".to_owned());
    argv
}

/// Take auto-merge back. Run before anything that changes the head, so an
/// armed merge never outlives the commit that was approved for it.
pub fn disable_automerge_argv(number: u64) -> Vec<String> {
    ["pr", "merge", &number.to_string(), "--disable-auto"]
        .map(str::to_owned)
        .to_vec()
}

/// Is this refusal to arm auto-merge "not available here" rather than "the
/// requirements are not met"?
///
/// Only wordings known to mean that: the repository has auto-merge switched
/// off, or the base branch has no requirements so the pull request is already
/// clean and there is nothing to wait for. Anything else - including every
/// message not recognised - is treated as unmet requirements, because reading
/// an unknown refusal as "unavailable" would turn it into a direct merge.
fn automerge_unavailable(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("auto merge is not allowed")
        || m.contains("auto-merge is not allowed")
        || m.contains("is in clean status")
        || (m.contains("protected branch rules") && m.contains("not configured"))
}

/// May the direct-merge fallback go ahead, judged from a fresh read?
///
/// The pull request must still be open on the approved head, the checks must
/// have been read for that very head, and the policy must still say merge.
/// Pure, so the head guard is asserted without a forge.
fn direct_merge_is_safe(
    fresh: Option<&Seen>,
    approved_head: &str,
    shown: &BTreeSet<String>,
    round: usize,
    budget: usize,
    waited: Duration,
) -> bool {
    let Some(fresh) = fresh else {
        return false;
    };
    if fresh.pr.state != PrLifecycle::Open {
        return false;
    }
    let Some(bound) = bound_head(&fresh.head, &fresh.rollup_head, None) else {
        return false;
    };
    if !bound.eq_ignore_ascii_case(approved_head) {
        return false;
    }
    let mut pr = fresh.pr.clone();
    pr.review_comments.retain(|c| !shown.contains(&c.body));
    decide(&pr, round, budget, waited) == Step::Merge
}

/// What GitHub is still waiting for, for the stop reason when an armed merge
/// does not happen in time. No I/O.
///
/// Required checks that are pending or failing are named. A check whose
/// required-ness could not be read is never assumed optional: every unsettled
/// check is listed and the reason says so.
fn waiting_on(merge_state: &str, contexts: &[CheckInfo]) -> String {
    let state = if merge_state.is_empty() {
        "unknown"
    } else {
        merge_state
    };
    let tag = |c: &CheckInfo| match c.verdict {
        Verdict::Fail => "failed",
        _ => "pending",
    };
    let unsettled: Vec<&CheckInfo> = contexts
        .iter()
        .filter(|c| c.verdict != Verdict::Pass)
        .collect();
    let required: Vec<String> = unsettled
        .iter()
        .filter(|c| c.required == Some(true))
        .map(|c| format!("{} ({})", c.label, tag(c)))
        .collect();
    let unknown: Vec<String> = unsettled
        .iter()
        .filter(|c| c.required.is_none())
        .map(|c| format!("{} ({})", c.label, tag(c)))
        .collect();
    let mut out = format!("merge state: {state}");
    if !required.is_empty() {
        let _ = write!(
            out,
            "; required checks not passing: {}",
            required.join(", ")
        );
    }
    if !unknown.is_empty() {
        let _ = write!(
            out,
            "; whether these are required could not be read, so they may be: {}",
            unknown.join(", ")
        );
    }
    if required.is_empty() && unknown.is_empty() {
        out.push_str(
            "; no required check is pending or failing, so GitHub is probably waiting for a \
             review or another branch rule",
        );
    }
    out
}

/// The squash subject to merge under.
///
/// The pull request title, unless it is empty or is a candidate branch's commit
/// subject that leaked into the title - in which case the task's own first line
/// is used, because `magi: candidate A (uncommitted work)` in `main` tells a
/// reader nothing about what landed.
pub fn merge_subject(pr_title: &str, instruction: &str) -> String {
    let title = pr_title.trim();
    if !title.is_empty() && !title.starts_with("magi: candidate") {
        return title.to_owned();
    }
    let first = instruction
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("magi: land the winning candidate");
    first.trim_start_matches(['#', ' ']).to_owned()
}

/// The choice that lets the merge happen, verbatim as the owner taps it.
pub const APPROVE: &str = "merge";

/// The choice that leaves the pull request open.
pub const HOLD: &str = "hold";

/// Graph node recorded on the approval question.
///
/// The phone keys its high-stakes card off this rather than off the choice
/// strings, so renaming a button cannot silently downgrade the card that
/// guards the one irreversible action magi takes.
pub const APPROVAL_NODE: &str = "land-approval";

/// Unified diff lines carried in the panel before it is truncated.
///
/// Four hundred: the panel is read on a 390px phone, where a diff line often
/// wraps to two rows, so this is already a few thousand rows of scrolling -
/// past that nobody is reading, and the bytes still count against the panel's
/// 8 MiB cap. A larger diff is not hidden: the note says how many lines were
/// cut and which worktree holds the whole patch.
pub const DIFF_MAX_LINES: usize = 400;

/// What the owner's answer to the approval question means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// The owner said [`APPROVE`]. Merge.
    Merge,
    /// Anything else, including silence. Leave the pull request open.
    Hold,
}

/// Read the owner's answer, where `None` is an unanswered question.
///
/// Silence is a hold. A timed-out question means the owner never saw it or
/// never decided, and defaulting an irreversible merge to "yes" would make this
/// gate worse than no gate at all: it would merge unattended while claiming to
/// have asked. Only the exact [`APPROVE`] choice merges, so an answer this
/// function does not recognise holds too.
pub fn approval(answer: Option<&str>) -> Approval {
    match answer {
        Some(a) if a.trim().eq_ignore_ascii_case(APPROVE) => Approval::Merge,
        _ => Approval::Hold,
    }
}

/// What [`approval_gate`] found on one check of the owner's merge decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalGate {
    /// The owner said [`APPROVE`]. Merge.
    Approved,
    /// The owner said anything else, the question timed out, or it was
    /// closed with no decision recorded.
    Held,
    /// Filed and still waiting - the caller parks rather than blocking on it.
    Pending,
}

/// Escape text for HTML, including both quote characters.
///
/// Every string in the panel is agent-influenced: a branch name, a file path, a
/// commit subject, a review comment. The sandboxed frame stops such text from
/// *running*, but it does not stop a `<` from ending the document early or a
/// `"` from ending an attribute and inventing a new one - the panel would then
/// render a lie, or not render at all. Both quotes are escaped because the same
/// function is used inside attributes, where remembering which quote style the
/// caller used is one mistake away from an injected attribute.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// One row of the diffstat table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatRow {
    path: String,
    /// `None` for a binary file, which `git` reports as `-`.
    added: Option<u64>,
    removed: Option<u64>,
}

impl StatRow {
    /// Lines touched, for sorting. A binary file counts as zero rather than as
    /// unknown, which puts it at the bottom where it needs no attention.
    fn churn(&self) -> u64 {
        self.added.unwrap_or(0) + self.removed.unwrap_or(0)
    }
}

/// Parse `git diff --numstat` into rows, biggest churn first.
///
/// `--numstat` and not `--stat`: the `+++---` bar in `--stat` is *scaled* to the
/// terminal width, so counting its characters would print fabricated numbers in
/// the one table an operator approves an irreversible action from.
fn parse_numstat(numstat: &str) -> Vec<StatRow> {
    let mut rows: Vec<StatRow> = numstat
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?.trim();
            let removed = parts.next()?.trim();
            let path = parts.next()?.trim();
            if path.is_empty() {
                return None;
            }
            Some(StatRow {
                path: path.to_owned(),
                added: added.parse().ok(),
                removed: removed.parse().ok(),
            })
        })
        .collect();
    // Path breaks the tie so the same change always renders the same table; an
    // operator comparing two panels should not see rows shuffle.
    rows.sort_by(|a, b| b.churn().cmp(&a.churn()).then_with(|| a.path.cmp(&b.path)));
    rows
}

/// How one diff line is shown: a gutter character, a style, and the body to
/// print - which is the line minus its marker, so the marker appears exactly
/// once, in the gutter.
///
/// The gutter is why this exists at all. The operator may be colour blind, or
/// reading in sunlight with the screen dimmed, so an added line is never
/// distinguished by its background alone: `+` and `-` sit in a fixed column,
/// the same mark they already read in a terminal.
fn diff_row(line: &str) -> (&'static str, &'static str, &str) {
    if line.starts_with("+++") || line.starts_with("---") {
        (" ", "color:#57606a;font-weight:600", line)
    } else if let Some(body) = line.strip_prefix('+') {
        ("+", "background:#e6ffec;color:#0a3622", body)
    } else if let Some(body) = line.strip_prefix('-') {
        ("-", "background:#ffebe9;color:#5c1a17", body)
    } else if line.starts_with("@@") {
        ("~", "background:#eef2ff;color:#3730a3", line)
    } else if let Some(body) = line.strip_prefix(' ') {
        (" ", "", body)
    } else {
        (" ", "color:#57606a;font-weight:600", line)
    }
}

/// The handful of words the approval panel says in its own voice.
///
/// magi's own text, not an agent's, so `[graph] language` has to reach it too:
/// the operator asked why the merge question spoke English on a repository
/// configured for Japanese, and "because that string is a literal in Rust" is
/// not an answer. Only the languages magi can actually check are translated;
/// anything else falls back to English rather than shipping a guess, and that
/// fallback is deliberate.
struct Words {
    html_lang: &'static str,
    task: &'static str,
    what_changed: &'static str,
    review_verdict: &'static str,
    reviewer: &'static str,
    reviewer_no_answer: &'static str,
    checks: &'static str,
    nothing_failing: &'static str,
    files_changed: &'static str,
    commits: &'static str,
    no_commits: &'static str,
    comments: &'static str,
    no_comments: &'static str,
    diff: &'static str,
    truncated: &'static str,
    lands_as: &'static str,
}

const EN: Words = Words {
    html_lang: "en",
    task: "Task",
    what_changed: "What changed",
    review_verdict: "Review verdict",
    reviewer: "Reviewer",
    reviewer_no_answer: "produced no answer",
    checks: "Checks",
    nothing_failing: "Nothing failing.",
    files_changed: "file(s) changed",
    commits: "Commits being squashed",
    no_commits: "No commit subjects could be read from the branch.",
    comments: "Review comments",
    no_comments: "Nothing outstanding at this observation.",
    diff: "Diff",
    truncated: "Truncated",
    lands_as: "They land as one commit titled",
};

const JA: Words = Words {
    html_lang: "ja",
    task: "タスク",
    what_changed: "変更内容",
    review_verdict: "レビューの結論",
    reviewer: "レビュアー",
    reviewer_no_answer: "回答なし",
    checks: "チェック",
    nothing_failing: "失敗しているものはありません。",
    files_changed: "ファイル変更",
    commits: "squash されるコミット",
    no_commits: "ブランチからコミット件名を読めませんでした。",
    comments: "レビューコメント",
    no_comments: "この時点で未対応のものはありません。",
    diff: "差分",
    truncated: "省略",
    lands_as: "これらは次の件名の1コミットとして入ります:",
};

impl Words {
    /// The clause after the merge subject. Split out because word order moves:
    /// Japanese puts the subject before the verb, so a shared template with a
    /// hole in the middle would read as machine translation.
    fn lands_as_tail(&self) -> &'static str {
        if self.html_lang == "ja" {
            "。この件名も承認の対象です。"
        } else {
            ", which you are approving too."
        }
    }

    /// The question's own one-line summary, which is what a phone shows first.
    fn approval_summary(&self, number: u64, subject: &str) -> String {
        if self.html_lang == "ja" {
            format!("プルリクエスト #{number} をマージ: {subject}")
        } else {
            format!("merge pull request #{number}: {subject}")
        }
    }

    /// The body under the summary, above the panel.
    fn approval_detail(
        &self,
        url: &str,
        base: &str,
        subject: &str,
        contested: Option<&ContestedHandoff>,
    ) -> String {
        let body = if self.html_lang == "ja" {
            format!(
                "{url} はチェックが緑で、`{base}` へ `{subject}` として squash \
                 できる状態です。差分の要約・パッチ・squash されるコミットは\
                 下のパネルにあります。"
            )
        } else {
            format!(
                "{url} is green and ready to squash into `{base}` as `{subject}`. \
                 The panel holds the diffstat, the patch and the commits being squashed."
            )
        };
        match contested {
            Some(c) => format!("{}\n\n{body}", self.contested_reason(url, c)),
            None => body,
        }
    }

    /// Why this question exists although merge approvals are off: the open
    /// blocking findings and who rejected. Short enough for a phone.
    fn contested_reason(&self, url: &str, c: &ContestedHandoff) -> String {
        const SHOWN: usize = 5;
        const TITLE_CHARS: usize = 100;
        let ja = self.html_lang == "ja";
        let mut out = if ja {
            format!(
                "{url} は、マージ承認がオフでも保留しています。レビューが予算切れで終わった\
                 時点で、却下票を伴う重大な未解決の指摘が残っているためです。\n"
            )
        } else {
            format!(
                "{url} is held for approval although merge approvals are off: the \
                 review ended with blocking findings still open and a reviewer \
                 voting reject.\n"
            )
        };
        for f in c.findings.iter().take(SHOWN) {
            let at = match (&f.file, f.line) {
                (Some(file), Some(line)) => format!("{file}:{line}"),
                (Some(file), None) => file.clone(),
                _ => (if ja { "場所未指定" } else { "no location" }).to_owned(),
            };
            let title: String = f.title.chars().take(TITLE_CHARS).collect();
            let _ = writeln!(out, "- {} {:?} {at}: {title}", f.id, f.severity);
        }
        if c.findings.len() > SHOWN {
            let more = c.findings.len() - SHOWN;
            let _ = writeln!(
                out,
                "{}",
                if ja {
                    format!("- ほか {more} 件")
                } else {
                    format!("- and {more} more")
                }
            );
        }
        let seats: Vec<String> = c
            .rejecters
            .iter()
            .map(|(seat, agent)| format!("#{seat} ({agent})"))
            .collect();
        let _ = write!(
            out,
            "{} {}",
            if ja {
                "却下したレビュアー:"
            } else {
                "Rejected by reviewer:"
            },
            seats.join(", ")
        );
        out
    }

    /// The truncation note, written whole in each language for the same reason.
    fn truncated_note(
        &self,
        omitted: usize,
        total: usize,
        shown: usize,
        where_: &str,
        base: &str,
        head: &str,
    ) -> String {
        if self.html_lang == "ja" {
            format!(
                "先頭 {shown} 行のあと、差分 {total} 行のうち {omitted} 行を省略しました。\
                 全体は <code>{where_}</code>(<code>git diff {base}...{head}</code>)と\
                 プルリクエストにあります。"
            )
        } else {
            format!(
                "{omitted} of {total} diff lines omitted after the first {shown}. \
                 The whole patch is in <code>{where_}</code> \
                 (<code>git diff {base}...{head}</code>) and on the pull request."
            )
        }
    }
}

/// Pick the panel's language. Codes and names both, because `[graph] language`
/// has always accepted either.
fn words(language: &str) -> &'static Words {
    if crate::lang::is_japanese(language) {
        &JA
    } else {
        &EN
    }
}

/// The approval panel's html: what is about to land, and the evidence for it.
///
/// Pure, so the whole document is asserted in tests without `gh`, without a
/// network and without a repository. The caller gathers `diffstat`
/// (`git diff --numstat`), `diff` (the unified patch), `commits` (the subjects
/// being squashed) and `subject` (what the squash will be called) from the
/// winner's worktree.
///
/// It emits no `<script>`, no `<form>` and no remote url, because the frame's
/// content security policy blocks all three: anything of the sort here would be
/// dead markup that misleads the next reader into thinking it works.
pub fn approval_panel(
    state: &RunState,
    pr: &PrState,
    diffstat: &str,
    diff: &str,
    commits: &[String],
    subject: &str,
) -> String {
    let rows = parse_numstat(diffstat);
    let w = words(&state.config.graph.language);
    let mut h = String::with_capacity(4_096 + diff.len().min(200_000));

    let _ = writeln!(
        h,
        "<!doctype html>\n<html lang=\"{}\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
        w.html_lang
    );
    let _ = writeln!(
        h,
        "<title>merge #{} — {}</title>\n</head>",
        pr.number,
        esc(subject)
    );
    h.push_str(
        "<body style=\"margin:0;padding:12px;font:15px/1.5 -apple-system,\
         'Segoe UI',system-ui,sans-serif;color:#1f2328;background:#fff;\
         word-break:break-word\">\n",
    );

    // The decision, in the words the operator is approving.
    let _ = writeln!(
        h,
        "<h1 style=\"margin:0 0 4px;font-size:19px\">Merge #{} into \
         <code style=\"background:#f6f8fa;padding:1px 4px;border-radius:4px\">{}</code></h1>\n\
         <p style=\"margin:0 0 4px;font-size:17px;font-weight:600\">{}</p>\n\
         <p style=\"margin:0 0 12px;font-size:13px;color:#57606a\">squash merge · run {} · \
         <a href=\"{}\" style=\"color:#0969da\">{}</a></p>",
        pr.number,
        esc(&state.base_branch),
        esc(subject),
        esc(&state.id),
        esc(&pr.url),
        esc(&pr.url),
    );

    // The task, verbatim: the operator's own words for what was asked, so the
    // panel does not make them reconstruct the request from a diffstat.
    let _ = writeln!(
        h,
        "<h2 style=\"margin:16px 0 6px;font-size:15px\">{}</h2>\n\
         <p style=\"margin:0;font-size:13px;white-space:pre-wrap\">{}</p>",
        w.task,
        esc(&state.instruction)
    );

    // The winner's own account of what it did and why, when there is one.
    if let Some(summary) = state
        .winner()
        .map(|c| c.summary.as_str())
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(
            h,
            "<h2 style=\"margin:16px 0 6px;font-size:15px\">{}</h2>\n\
             <p style=\"margin:0;font-size:13px;white-space:pre-wrap\">{}</p>",
            w.what_changed,
            esc(summary)
        );
    }

    // The verdict from the round that actually cleared this for merge - the
    // last one, since only that round's word is still standing.
    if let Some(round) = state.reviews.last() {
        let _ = writeln!(
            h,
            "<h2 style=\"margin:16px 0 6px;font-size:15px\">{}</h2>",
            w.review_verdict
        );
        for r in &round.reviews {
            // A seat the review loop counted as answered has real prose in
            // `summary`; one it counted against `incomplete` (see
            // `graph::Runner::review_loop`) never produced any and left it
            // empty - which must not be read back as a blank verdict, since
            // an empty box here looks like "nothing to say" rather than
            // "never answered".
            let body = match &r.failed {
                Some(reason) => format!("{}: {}", w.reviewer_no_answer, esc(reason)),
                None => esc(&r.summary),
            };
            let _ = writeln!(
                h,
                "<div style=\"margin:0 0 8px;padding:8px;background:#f6f8fa;\
                 border-radius:6px\">\
                 <div style=\"font-size:12px;color:#57606a\">{} {} · {}</div>\
                 <div style=\"white-space:pre-wrap;font-size:13px\">{}</div></div>",
                w.reviewer,
                r.reviewer,
                esc(&r.agent),
                body,
            );
        }
    }

    let _ = writeln!(
        h,
        "<h2 style=\"margin:16px 0 6px;font-size:15px\">{}: {}</h2>",
        w.checks,
        esc(pr.checks.as_str())
    );
    if pr.failing.is_empty() {
        let _ = writeln!(
            h,
            "<p style=\"margin:0;font-size:13px;color:#57606a\">{}</p>",
            w.nothing_failing
        );
    } else {
        h.push_str("<ul style=\"margin:0;padding-left:20px;font-size:13px\">\n");
        for f in &pr.failing {
            let _ = writeln!(h, "<li>{}</li>", esc(f));
        }
        h.push_str("</ul>\n");
    }

    // Diffstat as a real table, so a phone reads what moved without scrolling
    // sideways through a terminal bar chart.
    let _ = writeln!(
        h,
        "<h2 style=\"margin:16px 0 6px;font-size:15px\">{} {}</h2>",
        rows.len(),
        w.files_changed
    );
    h.push_str(
        "<table style=\"width:100%;border-collapse:collapse;font-size:13px\">\n\
         <thead><tr>\
         <th style=\"text-align:left;border-bottom:1px solid #d0d7de;padding:4px 2px\">file</th>\
         <th style=\"text-align:right;border-bottom:1px solid #d0d7de;padding:4px 2px\">added</th>\
         <th style=\"text-align:right;border-bottom:1px solid #d0d7de;padding:4px 2px\">removed\
         </th></tr></thead>\n<tbody>\n",
    );
    let mut total_added = 0u64;
    let mut total_removed = 0u64;
    for r in &rows {
        total_added += r.added.unwrap_or(0);
        total_removed += r.removed.unwrap_or(0);
        let cell = |n: Option<u64>| match n {
            Some(n) => n.to_string(),
            None => "bin".to_owned(),
        };
        let _ = writeln!(
            h,
            "<tr>\
             <td style=\"padding:4px 2px;border-bottom:1px solid #eaeef2;\
             font-family:ui-monospace,monospace\">{}</td>\
             <td style=\"padding:4px 2px;border-bottom:1px solid #eaeef2;text-align:right;\
             color:#0a3622\">{}</td>\
             <td style=\"padding:4px 2px;border-bottom:1px solid #eaeef2;text-align:right;\
             color:#5c1a17\">{}</td></tr>",
            esc(&r.path),
            cell(r.added),
            cell(r.removed),
        );
    }
    let _ = writeln!(
        h,
        "</tbody>\n<tfoot><tr style=\"font-weight:600\">\
         <td style=\"padding:4px 2px\">total</td>\
         <td style=\"padding:4px 2px;text-align:right\">{total_added}</td>\
         <td style=\"padding:4px 2px;text-align:right\">{total_removed}</td>\
         </tr></tfoot>\n</table>"
    );

    // The commits being squashed, and the subject that replaces them.
    let _ = writeln!(
        h,
        "<h2 style=\"margin:16px 0 6px;font-size:15px\">{}</h2>",
        w.commits
    );
    if commits.is_empty() {
        h.push_str(&format!(
            "<p style=\"margin:0;font-size:13px;color:#57606a\">{}</p>\n",
            w.no_commits
        ));
    } else {
        h.push_str("<ol style=\"margin:0;padding-left:20px;font-size:13px\">\n");
        for c in commits {
            let _ = writeln!(h, "<li>{}</li>", esc(c));
        }
        h.push_str("</ol>\n");
    }
    let _ = writeln!(
        h,
        "<p style=\"margin:8px 0 0;font-size:13px\">{} <strong>{}</strong>{}</p>",
        w.lands_as,
        esc(subject),
        w.lands_as_tail()
    );

    // The review comments that shaped this branch, and who asked for them.
    let _ = writeln!(
        h,
        "<h2 style=\"margin:16px 0 6px;font-size:15px\">{}</h2>",
        w.comments
    );
    if pr.review_comments.is_empty() {
        h.push_str(&format!(
            "<p style=\"margin:0;font-size:13px;color:#57606a\">{}</p>\n",
            w.no_comments
        ));
    } else {
        for c in &pr.review_comments {
            let anchor = match (&c.path, c.line) {
                (Some(p), Some(l)) => format!("{p}:{l}"),
                (Some(p), None) => p.clone(),
                _ => "pull request thread".to_owned(),
            };
            let _ = writeln!(
                h,
                "<div style=\"margin:0 0 8px;padding:8px;background:#f6f8fa;border-radius:6px\">\
                 <div style=\"font-size:12px;color:#57606a\">{} · {}</div>\
                 <div style=\"white-space:pre-wrap;font-size:13px\">{}</div></div>",
                esc(&c.author),
                esc(&anchor),
                esc(&tail(&c.body, 800)),
            );
        }
    }

    // The patch itself.
    let total = diff.lines().count();
    let shown = total.min(DIFF_MAX_LINES);
    let _ = writeln!(
        h,
        "<h2 style=\"margin:16px 0 6px;font-size:15px\">{}</h2>",
        w.diff
    );
    h.push_str(
        "<div style=\"font:12px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;\
         border:1px solid #d0d7de;border-radius:6px;overflow-x:auto\">\n",
    );
    for line in diff.lines().take(shown) {
        let (gutter, style, body) = diff_row(line);
        let _ = writeln!(
            h,
            "<div style=\"display:flex;{style}\">\
             <span style=\"flex:0 0 1.4em;text-align:center;user-select:none;\
             border-right:1px solid #d0d7de\">{gutter}</span>\
             <span style=\"white-space:pre;padding-left:6px\">{}</span></div>",
            esc(body),
        );
    }
    h.push_str("</div>\n");
    if total > shown {
        let omitted = total - shown;
        let head = state.winner().map_or("HEAD", |w| w.branch.as_str());
        let where_ = state.winner().map_or_else(
            || state.repo.display().to_string(),
            |w| w.worktree.display().to_string(),
        );
        let _ = writeln!(
            h,
            "<p style=\"margin:8px 0 0;padding:8px;background:#fff8c5;border-radius:6px;\
             font-size:13px\">{}: {}</p>",
            w.truncated,
            w.truncated_note(
                omitted,
                total,
                shown,
                &esc(&where_),
                &esc(&state.base_branch),
                &esc(head),
            ),
        );
    }

    h.push_str("</body>\n</html>\n");
    h
}

/// The contested hand-off `land` must ask about, if any: recorded by the
/// review loop and not switched off by `graph.hold_contested_merge`. The one
/// place `land` reads that record.
fn contested_to_ask(state: &RunState) -> Option<ContestedHandoff> {
    if state.config.graph.hold_contested_merge {
        state.contested_handoff.clone()
    } else {
        None
    }
}

/// What a merge-approval question's deputy is told: the pull request, the run,
/// what each answer does, and - when the run's record is readable - the
/// contested hand-off the question was filed over.
///
/// A snapshot taken when the deputy is attached, so it says so and points at
/// `magi show` / `gh pr view` for anything current. `state` is `None` for a run
/// that cannot be read; what is missing is named as missing, and no past
/// approval basis is rebuilt from today's state.
pub fn deputy_brief(q: &ask::Question, state: Option<&RunState>) -> String {
    let mut s = format!(
        "This is the merge approval for run {run} (`magi show {run}`). The question's \
         own text above names the pull request. Answering `{APPROVE}` squash-merges \
         it into the base branch, which cannot be undone; `{HOLD}` leaves the pull \
         request open. Silence is a hold: the owner not answering never merges. Only \
         the owner choosing `{APPROVE}`, or writing the single word `{APPROVE}`, \
         merges; no other wording is a decision.\n\n\
         This brief is a snapshot from when you were attached: check `magi show {run}` \
         and `gh pr view` (read-only) before telling the owner anything current. \
         You run with permission to write the question record, and what keeps you \
         from touching anything else is this brief and your instructions - so do \
         not change files, branches or the pull request.",
        run = q.run
    );
    let Some(state) = state else {
        s.push_str(
            "\n\nThe run's record could not be read, so the pull request, the panel \
             summary and any contested findings are not known to you beyond the \
             question's own text. Say so to the owner rather than guessing.",
        );
        return s;
    };
    if let Some(pr) = &state.pr {
        s.push_str(&format!(
            "\n\nPull request #{} {} (recorded state: {}, last seen).",
            pr.number, pr.url, pr.state
        ));
    }
    s.push_str(&format!("\nBase branch: `{}`.", state.base_branch));
    if let Some(w) = state.winner() {
        s.push_str(&format!("\nWinning branch: `{}`.", w.branch));
    }
    match contested_to_ask(state) {
        Some(c) => {
            s.push_str(
                "\n\nThis question was filed although merge approvals are off, because \
                 the review hand-off is contested. Open findings:",
            );
            for f in &c.findings {
                let at = match (&f.file, f.line) {
                    (Some(file), Some(line)) => format!(" ({file}:{line})"),
                    (Some(file), None) => format!(" ({file})"),
                    _ => String::new(),
                };
                s.push_str(&format!("\n- [{}] {:?}{at}: {}", f.id, f.severity, f.title));
            }
            let seats: Vec<String> = c.rejecters.iter().map(|(n, _)| format!("#{n}")).collect();
            s.push_str(&format!("\nReviewers who rejected: {}.", seats.join(", ")));
        }
        None => s.push_str("\n\nThe review hand-off was not recorded as contested."),
    }
    s
}

/// Ask the owner before merging, with the whole case attached as a panel.
///
/// The evidence is gathered from the winner's own worktree with the `git` CLI,
/// never from the network, so a phone on a slow link gets the diff magi is
/// looking at rather than a link it has to go and open.
///
/// Never blocks. `land` used to sit inside [`ask::ask_and_wait`]'s poll loop
/// for up to a day right here, which held the whole run's task claim - and
/// the daemon's one slot with it - for exactly as long as the owner took to
/// notice their phone. [`ApprovalGate::Pending`] is the answer that lets the
/// caller park the run and hand the slot back instead: the question is on
/// disk either way, so nothing about the wait itself changes, only who is
/// blocked on it.
///
/// Idempotent across resumes: called again for a run already waiting on its
/// own question, this finds that question by [`crate::ask::Questions::list`]
/// rather than filing a second one - asking twice would double the
/// notification for one decision, and leave the first question's panel an
/// orphan nobody's answer ever reaches.
async fn approval_gate(
    state: &mut RunState,
    pr: &PrState,
    subject: &str,
    contested: Option<&ContestedHandoff>,
    head: &str,
) -> Result<ApprovalGate> {
    let store = ask::Questions::open();
    // An answer belongs to the commit its panel showed. A question recorded
    // for another head - or none recorded at all, as for a question filed
    // before heads were tracked - is never reused: a fix or rebase push made
    // a commit the owner has not seen, and their word does not carry over.
    let reusable = state
        .land_approval
        .as_ref()
        .filter(|a| a.head.eq_ignore_ascii_case(head))
        .and_then(|a| store.list().into_iter().find(|q| q.id == a.question));
    if reusable.is_none() {
        for stale in store
            .list()
            .into_iter()
            .filter(|q| q.run == state.id && q.node == APPROVAL_NODE && q.status.open())
        {
            let why = "the pull request moved to a different head commit; asked again about it";
            if let Err(e) = store.update(&stale.id, |q| {
                q.abandon(why);
                Ok(())
            }) {
                tracing::warn!("could not retire the superseded approval question: {e:#}");
            }
        }
    }

    let q = match reusable {
        Some(q) => q,
        None => {
            let worktree = match state.winner() {
                Some(w) => w.worktree.clone(),
                None => state.repo.clone(),
            };
            // The diff is built from the commit being approved, not from the
            // branch name, which may already point somewhere else.
            let head = if head.is_empty() {
                state
                    .winner()
                    .map_or_else(|| "HEAD".to_owned(), |w| w.branch.clone())
            } else {
                head.to_owned()
            };
            let base = state.base_branch.clone();
            let range = format!("{base}...{head}");
            // A failed `git` must not decide the merge: the panel degrades to
            // less evidence and the owner still chooses. Merging because the
            // diff could not be read would be the worst of both.
            let numstat = git::git_raw(&worktree, &["diff", "--numstat", "-M", &range])
                .await
                .map(|o| o.stdout)
                .unwrap_or_default();
            let diff = git::diff(&worktree, &base, &head).await.unwrap_or_default();
            let commits: Vec<String> = git::git_raw(
                &worktree,
                &[
                    "log",
                    "--reverse",
                    "--format=%s",
                    &format!("{base}..{head}"),
                ],
            )
            .await
            .map(|o| o.stdout)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect();

            let w = words(&state.config.graph.language);
            let html = approval_panel(state, pr, &numstat, &diff, &commits, subject);
            let mut fresh = ask::Question::new(
                state.id.clone(),
                APPROVAL_NODE.to_owned(),
                "land".to_owned(),
                w.approval_summary(pr.number, subject),
                w.approval_detail(&pr.url, &base, subject, contested),
                vec![APPROVE.to_owned(), HOLD.to_owned()],
            );
            store
                .put_panel(&mut fresh, &html, &[])
                .context("write the merge approval panel")?;
            store
                .put(&mut fresh)
                .context("file the merge approval question")?;
            state.land_approval = Some(LandApproval {
                question: fresh.id.clone(),
                head: head.clone(),
            });
            state.event(
                "land",
                format!("asking for merge approval ({})", fresh.short()),
            );
            state.save()?;
            if let Err(e) = ask::notify(&state.config.notify, &fresh).await {
                // A broken webhook is not a reason to lose the merge: the
                // question is already on disk and the web UI already shows
                // it, so the operator still has a way in.
                tracing::warn!(
                    "could not notify about merge approval question {}: {e:#} - \
                     the web UI is the only surface for it now",
                    fresh.short()
                );
            }
            fresh
        }
    };

    Ok(match q.status {
        ask::QuestionStatus::Open => ApprovalGate::Pending,
        // Nobody answered before `state.config.graph.answer_timeout` passed,
        // or the question was closed with no decision recorded underneath
        // this run - either way there is nothing left to wait on.
        ask::QuestionStatus::Abandoned => ApprovalGate::Held,
        // The merge gate does not speak `--thread`: an owner who talked back
        // instead of choosing never reaches `Answered`, so this arm only
        // ever sees an actual decision.
        ask::QuestionStatus::Answered => match approval(q.resolution().as_deref()) {
            Approval::Merge => ApprovalGate::Approved,
            Approval::Hold => ApprovalGate::Held,
        },
    })
}

/// Fold a rollup's entries into one verdict plus the failing checks' labels.
/// No I/O. An empty rollup is `Unknown`, never green.
fn rollup_verdict(rollup: &[GhCheck]) -> (Checks, Vec<String>) {
    let mut failing = Vec::new();
    let mut pending = false;
    let mut unknown = false;
    for check in rollup {
        match check.verdict() {
            Verdict::Pass => {}
            Verdict::Pending => pending = true,
            Verdict::Fail => failing.push(check.label()),
            Verdict::Unknown => unknown = true,
        }
    }
    let checks = if rollup.is_empty() {
        Checks::Unknown
    } else if pending {
        Checks::Pending
    } else if !failing.is_empty() {
        Checks::Red
    } else if unknown {
        Checks::Unknown
    } else {
        Checks::Green
    };
    (checks, failing)
}

/// Parse `gh pr view --json url,number,state,statusCheckRollup,reviews,comments`
/// output into a [`PrState`]. No I/O.
pub fn parse_pr(json: &str) -> Result<PrState> {
    let raw: GhPr = serde_json::from_str(json).context("parse `gh pr view --json ...` output")?;
    let state = match raw.state.to_ascii_uppercase().as_str() {
        "OPEN" => PrLifecycle::Open,
        "MERGED" => PrLifecycle::Merged,
        "CLOSED" => PrLifecycle::Closed,
        other => bail!("unknown pull request state `{other}`"),
    };

    let (checks, failing) = rollup_verdict(&raw.status_check_rollup);

    let mut review_comments = Vec::new();
    for r in raw.reviews {
        push_if_outstanding(
            &mut review_comments,
            ReviewComment {
                author: r.author.login,
                path: None,
                line: None,
                body: r.body,
            },
        );
    }
    for c in raw.comments {
        push_if_outstanding(
            &mut review_comments,
            ReviewComment {
                author: c.author.login,
                path: None,
                line: None,
                body: c.body,
            },
        );
    }

    Ok(PrState {
        url: raw.url,
        number: raw.number,
        state,
        checks,
        failing,
        review_comments,
        blocking: Blocking::of(&raw.merge_state_status),
    })
}

/// Read just a pull request's lifecycle state - open, merged, or closed -
/// with none of the checks/reviews/comments [`land`] itself needs to decide
/// what to do next.
///
/// For a caller that only ever wants one fact and must not risk anything
/// else: `magi fold --merged` uses this to confirm a URL the operator hands
/// it is actually a merged pull request *before* touching a run's state, so a
/// typo or a still-open PR fails loudly instead of quietly recording a merge
/// that never happened.
pub async fn lifecycle(repo: &Path, pr_url: &str) -> Result<PrLifecycle> {
    let view = gh(
        repo,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            pr_url.to_owned(),
            "--json".to_owned(),
            "state".to_owned(),
        ],
    )
    .await?;
    if !view.0 {
        bail!("gh pr view {pr_url}: {}", view.1);
    }
    // `parse_pr` reads every other field of `GhPr` as its serde default
    // (empty string, empty vec, zero) when this narrower `--json` selection
    // does not carry them - harmless, since only `.state` is read back.
    Ok(parse_pr(&view.1)?.state)
}

/// A pull request the operator merged outside of `land::land`'s own loop,
/// found by asking GitHub about the run's own winning branch rather than
/// requiring the operator to go and find the URL themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalMerge {
    /// The pull request's URL, ready to hand to [`correct_manual_merge`].
    pub url: String,
    /// The pull request's number.
    pub number: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhMergedPr {
    url: String,
    number: u64,
    merged_at: String,
    base_ref_name: String,
}

/// Pure half of [`find_external_merge`]: given the raw `gh pr list --head
/// <branch> --state merged --json url,number,mergedAt,baseRefName` output,
/// decide whether exactly one of the pull requests it lists could actually
/// be *this* run's.
///
/// A branch name alone does not prove it: [`RunState::branch_for`] derives it
/// from the run's own short id, so a collision with some other, unrelated
/// task's merged pull request from a same-named branch is rare but not
/// impossible once branches are deleted and ids run out. Filtering on
/// `base_ref_name` (the branch this run actually targets) and `merged_at`
/// (which cannot predate the run itself) rules that case out. More than one
/// survivor is exactly as uninformative as zero — something this run cannot
/// tell apart from another — so only a unique survivor is returned.
fn pick_merged_pr(
    json: &str,
    base_branch: &str,
    created_at: Timestamp,
) -> Result<Option<ExternalMerge>> {
    let raw: Vec<GhMergedPr> =
        serde_json::from_str(json).context("parse `gh pr list ... --json ...` output")?;
    let mut matches: Vec<ExternalMerge> = Vec::new();
    for pr in raw {
        if pr.base_ref_name != base_branch {
            continue;
        }
        let Ok(merged_at) = pr.merged_at.parse::<Timestamp>() else {
            continue;
        };
        if merged_at < created_at {
            continue;
        }
        matches.push(ExternalMerge {
            url: pr.url,
            number: pr.number,
        });
    }
    if matches.len() == 1 {
        Ok(matches.pop())
    } else {
        Ok(None)
    }
}

/// What `gh pr list --head <branch> --base <base> --state open` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPr {
    /// Nothing open: the caller creates one.
    None,
    /// Exactly one: the caller adopts it instead of creating a second.
    One {
        /// The pull request's URL.
        url: String,
        /// Its current title.
        title: String,
    },
    /// More than one: magi does not pick between them.
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhOpenPr {
    // `url` and `baseRefName` are required: a record missing either must be a
    // parse error, not a pull request that silently fails the base filter and
    // reads as "none open" (which would go on to create a duplicate).
    url: String,
    #[serde(default)]
    title: String,
    base_ref_name: String,
}

/// Pure half of [`find_open_pr`]: classify the raw `--json
/// number,url,title,baseRefName` output. Entries whose base is not `base` are
/// dropped even though the query already filtered on it, so a stub or an old
/// `gh` that ignores `--base` cannot get a pull request into the wrong branch
/// adopted.
pub fn pick_open_pr(json: &str, base: &str) -> Result<OpenPr> {
    let raw: Vec<GhOpenPr> =
        serde_json::from_str(json).context("parse `gh pr list ... --json ...` output")?;
    let mut hits: Vec<GhOpenPr> = raw
        .into_iter()
        .filter(|p| p.base_ref_name == base)
        .collect();
    Ok(match hits.len() {
        0 => OpenPr::None,
        1 => {
            let p = hits.remove(0);
            OpenPr::One {
                url: p.url,
                title: p.title,
            }
        }
        _ => OpenPr::Many(hits.into_iter().map(|p| p.url).collect()),
    })
}

/// Open pull requests whose head is `branch` and whose base is `base`. A
/// failing `gh` is an error carrying its own output, never "none": guessing
/// there is how a duplicate gets created.
pub async fn find_open_pr(repo: &Path, branch: &str, base: &str) -> Result<OpenPr> {
    let (ok, out) = gh(
        repo,
        &[
            "pr".to_owned(),
            "list".to_owned(),
            "--head".to_owned(),
            branch.to_owned(),
            "--base".to_owned(),
            base.to_owned(),
            "--state".to_owned(),
            "open".to_owned(),
            "--json".to_owned(),
            "number,url,title,baseRefName".to_owned(),
        ],
    )
    .await?;
    if !ok {
        bail!("gh pr list failed: {out}");
    }
    pick_open_pr(&out, base)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPrHead {
    head_ref_name: String,
    base_ref_name: String,
    state: String,
    // Required, like `GhOpenPr`'s fields: a record that cannot say whether the
    // head lives in a fork must be a parse error, never "same repository".
    is_cross_repository: bool,
    // Required too: it is what ties the pull request to the commits that were
    // actually proven to be on the base.
    head_ref_oid: String,
}

/// Why [`closable`] said no, and whether asking again later could say yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// A later attempt may succeed (the head moved, the view was unreadable);
    /// `false` means this pull request is simply not the run's to close.
    pub retry: bool,
    /// What was wrong.
    pub why: String,
}

impl Refusal {
    fn final_(why: String) -> Self {
        Self { retry: false, why }
    }
}

/// Pure half of [`close_superseded_pr`]: read `gh pr view --json
/// headRefName,baseRefName,state,isCrossRepository` and say whether closing
/// is safe, `Err` carrying the reason when it is not.
///
/// Closing is outward-facing, so every property is checked on what the forge
/// says now, not on what magi recorded: the head must be exactly `branch` in
/// this repository (a fork's branch of the same name is somebody else's), the
/// base must be `base`, and the pull request must still be open.
pub fn closable(
    json: &str,
    branch: &str,
    base: &str,
    verified: &[String],
) -> std::result::Result<(), Refusal> {
    let pr: GhPrHead = serde_json::from_str(json).map_err(|e| Refusal {
        retry: true,
        why: format!("could not read the pull request ({e})"),
    })?;
    if pr.head_ref_name != branch {
        return Err(Refusal::final_(format!(
            "its head is `{}`, not this run's `{branch}`",
            pr.head_ref_name
        )));
    }
    if pr.is_cross_repository {
        return Err(Refusal::final_("its head lives in a fork".to_owned()));
    }
    if pr.base_ref_name != base {
        return Err(Refusal::final_(format!(
            "it targets `{}`, not `{base}`",
            pr.base_ref_name
        )));
    }
    if !pr.state.eq_ignore_ascii_case("open") {
        return Err(Refusal::final_(format!(
            "it is already {}",
            pr.state.to_ascii_lowercase()
        )));
    }
    // Last, so a pull request that is not this run's at all is reported as
    // such. A head that is this branch but not a commit checked against the
    // base (somebody pushed since) may well get checked next time: retry.
    if !verified.contains(&pr.head_ref_oid) {
        return Err(Refusal {
            retry: true,
            why: format!(
                "its head {} is not a commit this run checked against the base",
                crate::already::short_sha(&pr.head_ref_oid)
            ),
        });
    }
    Ok(())
}

/// Does `remote` point at something a forge could host - a URL or an scp-style
/// `user@host:path` - rather than a filesystem path or nothing at all? Judged
/// from the URL alone, so it answers the same on every machine, whatever `gh`
/// happens to be installed or logged in to.
async fn remote_is_forge(repo: &Path, remote: &str) -> bool {
    let Ok(url) = git::git(repo, &["remote", "get-url", remote]).await else {
        return false;
    };
    is_forge_url(url.trim())
}

fn is_forge_url(url: &str) -> bool {
    url.contains("://") && !url.starts_with("file://")
        || url
            .split_once(':')
            .is_some_and(|(host, _)| host.contains('@') && !host.contains(['/', '\\']))
}

/// Does this `gh` failure mean there is no GitHub to ask, as opposed to a
/// request that failed?
fn forge_unavailable(message: &str) -> bool {
    message.contains("known GitHub host") || message.contains("spawn gh")
}

/// The comment left on a pull request closed because its change is already on
/// the base.
pub fn superseded_comment(base: &str, evidence: &crate::already::Evidence) -> String {
    let how = match evidence.proof {
        crate::already::Proof::PatchId => format!(
            "carried by commit {} on `{base}` with the same patch",
            evidence.names()
        ),
        crate::already::Proof::Ancestry => {
            format!("already in the history of `{base}` as {}", evidence.names())
        }
        crate::already::Proof::Tree => format!(
            "already part of `{base}` (merging this branch changes nothing at {})",
            crate::already::short_sha(&evidence.tip)
        ),
    };
    format!(
        "Closing: everything this branch adds is {how}, so there is nothing left to \
         land. This pull request was closed automatically after that was verified; \
         reopen it if you disagree."
    )
}

/// Close the open pull request for `branch`, if there is one and it is
/// provably this run's, with a comment naming what supersedes it. `Ok(Ok(url))` is
/// the URL closed; `Ok(Err(why))` is a final "nothing to close" (no pull request,
/// not this run's, no forge); `Err` is anything a later attempt could resolve (a
/// failed `gh` call, a head that moved since it was checked), which the caller
/// must not treat as settled.
///
/// The recorded `state.pr` is preferred, else the forge is asked for an open
/// pull request on `branch`; either way the candidate is re-read and passed
/// through [`closable`] before anything is written; `verified` lists the
/// commits proven to be on the base, and the pull request's head must be one.
pub async fn close_superseded_pr(
    state: &mut RunState,
    branch: &str,
    evidence: &crate::already::Evidence,
    verified: &[String],
) -> Result<std::result::Result<String, String>> {
    let repo = state.repo.clone();
    let base = state.base_branch.clone();
    let url = match state.pr.as_ref().filter(|p| p.state == "open") {
        Some(p) => p.url.clone(),
        // No recorded pull request. A forge that cannot be asked at all (no
        // GitHub remote, no `gh`) says nothing about this run, so that is
        // "none found"; any other lookup failure is an error, because "could
        // not look" is not "nothing there" and the caller must retry.
        None if !remote_is_forge(&repo, &state.config.merge.remote).await => {
            return Ok(Err(
                "the remote is not a forge, so there is no pull request".to_owned(),
            ));
        }
        None => match find_open_pr(&repo, branch, &base).await {
            Err(e) if forge_unavailable(&format!("{e:#}")) => {
                return Ok(Err(format!("no forge to ask: {e:#}")));
            }
            Err(e) => return Err(e),
            Ok(OpenPr::One { url, .. }) => url,
            Ok(OpenPr::None) => return Ok(Err("no open pull request".to_owned())),
            Ok(OpenPr::Many(urls)) => {
                return Ok(Err(format!(
                    "{} open pull requests name it; not choosing between them",
                    urls.len()
                )));
            }
        },
    };
    let (ok, view) = gh(
        &repo,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            url.clone(),
            "--json".to_owned(),
            "headRefName,headRefOid,baseRefName,state,isCrossRepository".to_owned(),
        ],
    )
    .await?;
    if !ok {
        bail!("gh pr view {url} failed: {view}");
    }
    if let Err(refusal) = closable(&view, branch, &base, verified) {
        // A pull request whose head cannot be tied to what was proven is not
        // one to walk away from: the caller keeps the run resumable.
        if refusal.retry {
            bail!("left {url} open: {}", refusal.why);
        }
        return Ok(Err(format!("left {url} open: {}", refusal.why)));
    }
    let (ok, out) = gh(
        &repo,
        &[
            "pr".to_owned(),
            "close".to_owned(),
            url.clone(),
            "--comment".to_owned(),
            superseded_comment(&base, evidence),
        ],
    )
    .await?;
    if !ok {
        bail!("gh pr close {url} failed: {out}");
    }
    if let Some(p) = state.pr.as_mut().filter(|p| p.url == url) {
        p.state = "closed".to_owned();
    }
    Ok(Ok(url))
}

/// `gh pr edit <url> --title <title>`, for an adopted pull request whose title
/// differs from the one this run computed. Only the title: the body may have
/// been edited by the owner and cannot be compared.
pub async fn set_pr_title(repo: &Path, url: &str, title: &str) -> Result<()> {
    let (ok, out) = gh(
        repo,
        &[
            "pr".to_owned(),
            "edit".to_owned(),
            url.to_owned(),
            "--title".to_owned(),
            title.to_owned(),
        ],
    )
    .await?;
    if !ok {
        bail!("gh pr edit failed: {out}");
    }
    Ok(())
}

/// Ask GitHub whether this run's winning candidate branch was actually merged
/// somewhere `land::land`'s own loop never saw — the gap `magi fold
/// --merged` exists to close, minus the operator having to find the URL by
/// hand.
///
/// `Ok(None)` covers every case where nothing can be said with confidence: no
/// winner decided yet (nothing to check a branch for), no merged pull request
/// found, or [`pick_merged_pr`] found more than one candidate and would not
/// guess between them. Never wired to a weaker, URL-less signal like
/// [`branch_is_ancestor`] — a caller wanting that has to ask for it
/// separately, precisely because it cannot drive an automatic correction on
/// its own (see that function's own doc).
pub async fn find_external_merge(state: &RunState) -> Result<Option<ExternalMerge>> {
    let Some(winner) = state.winner() else {
        return Ok(None);
    };
    let branch = winner.branch.clone();
    let out = gh(
        &state.repo,
        &[
            "pr".to_owned(),
            "list".to_owned(),
            "--head".to_owned(),
            branch.clone(),
            "--state".to_owned(),
            "merged".to_owned(),
            "--json".to_owned(),
            "url,number,mergedAt,baseRefName".to_owned(),
        ],
    )
    .await?;
    if !out.0 {
        bail!("gh pr list --head {branch}: {}", out.1);
    }
    pick_merged_pr(&out.1, &state.base_branch, state.created_at)
}

/// Whether `branch` is, right now, an ancestor of `base_branch` in the local
/// git graph — the weaker, URL-less signal that a branch landed somewhere.
///
/// Deliberately never consulted by [`find_external_merge`]: a base branch
/// that has moved since the run started can make an old, abandoned branch
/// look like an ancestor of the *current* base for reasons that have nothing
/// to do with a merge (a later commit that happens to supersede it, an
/// unrelated squash), and there is no pull request URL here to confirm
/// against or to land through anyway. Its only honest use is a weaker
/// notice — "this looks merged, go check" — never an automatic rewrite of
/// `status`/`merge`.
pub async fn branch_is_ancestor(repo: &Path, branch: &str, base_branch: &str) -> Result<bool> {
    let out = tokio::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", branch, base_branch])
        .current_dir(repo)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn git merge-base --is-ancestor")?;
    Ok(out.status.success())
}

/// Parse `host/owner/repo` out of a forge URL, with no network access.
///
/// The host is part of the slug, not discarded: `owner/repo` alone would
/// treat `github.example.com/o/r` and `github.com/o/r` as the same
/// repository, which is exactly the mix-up the same-repo guard exists to
/// catch. Returns `None` for anything that doesn't have a `<host>/<path>`
/// shape at all.
fn forge_slug(url: &str) -> Option<(String, &str)> {
    let rest = url.rsplit("://").next()?;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), path))
}

/// Parse `host/owner/repo` out of a GitHub pull request URL, with no network
/// access - the first half of the same-repo guard [`correct_manual_merge`]
/// applies before it writes anything.
///
/// Returns `None` for anything that does not look like
/// `https://<host>/<owner>/<repo>/pull/<n>`, which the caller treats as
/// fail-closed: a URL this cannot make sense of refuses rather than guesses.
pub(crate) fn slug_of_pr_url(url: &str) -> Option<String> {
    let (host, path) = forge_slug(url)?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    let kind = segments.next()?;
    if owner.is_empty() || repo.is_empty() || kind != "pull" {
        return None;
    }
    Some(format!("{host}/{owner}/{repo}"))
}

/// Parse `host/owner/repo` out of a plain repository URL (no `/pull/<n>`
/// suffix), the shape `gh repo view --json url` returns - the other half of
/// the same-repo guard, matched against [`slug_of_pr_url`]'s output.
fn slug_of_repo_url(url: &str) -> Option<String> {
    let (host, path) = forge_slug(url)?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{host}/{owner}/{repo}"))
}

/// Refuse to correct a run against a pull request from a different
/// repository than the one it is recorded against.
///
/// This is the guard the shun/8c75 incident argued for: an operator ran
/// `magi fold --merged <shun PR url>` meaning to correct an old `Blocked` run
/// in a different repository, omitted the run id, and the id defaulted to
/// this machine's most recently created run - an unrelated, still-in-progress
/// run in a completely different repository - which then had its `status`
/// rewritten to `merged` from a pull request it had nothing to do with.
/// `correct_manual_merge` now requires an explicit id (see `magi fold`'s own
/// CLI help), but a mistyped or stale id could still name a run in a
/// different repository than the one the URL belongs to, so this checks that
/// independently rather than trusting the id alone.
///
/// Comparison is case-insensitive - GitHub owner/repo names are - and a
/// mismatch names both slugs rather than just refusing, so an operator whose
/// local checkout's `origin` is a fork of the repository the pull request was
/// opened against (a legitimate setup this cannot tell apart from a genuine
/// mix-up) can judge for themselves rather than being blocked with no way to
/// see why.
pub(crate) fn ensure_same_repo(run_repo_slug: &str, pr_repo_slug: &str) -> Result<()> {
    if run_repo_slug.eq_ignore_ascii_case(pr_repo_slug) {
        return Ok(());
    }
    bail!(
        "refusing to correct this run: it is recorded against {run_repo_slug}, but the pull \
         request URL belongs to {pr_repo_slug} - pass the run id whose repository the URL \
         actually belongs to (or, if `origin` is a fork opened against a different upstream, \
         verify by hand before treating this as a false positive)"
    );
}

/// Ask the forge which `host/owner/repo` a local checkout's `origin` remote
/// actually resolves to, for the same-repo guard in [`correct_manual_merge`].
///
/// Asking `gh` rather than parsing `git remote -v` locally is deliberate: it
/// normalizes case, SSH vs. HTTPS remotes, and a renamed or transferred
/// repository the same way GitHub itself would recognize it, so the
/// comparison in [`ensure_same_repo`] is against the same canonical slug on
/// both sides. Reads `url` rather than `nameWithOwner` so the host is part of
/// the answer too - `nameWithOwner` alone cannot tell a `github.com` repo from
/// a same-named one on a GitHub Enterprise host.
async fn repo_slug(repo: &Path) -> Result<String> {
    let out = gh(
        repo,
        &[
            "repo".to_owned(),
            "view".to_owned(),
            "--json".to_owned(),
            "url".to_owned(),
        ],
    )
    .await?;
    if !out.0 {
        bail!("gh repo view --json url: {}", out.1);
    }
    #[derive(Debug, Deserialize)]
    struct GhRepo {
        url: String,
    }
    let parsed: GhRepo = serde_json::from_str(&out.1)
        .with_context(|| format!("parse `gh repo view` output: {}", out.1))?;
    slug_of_repo_url(&parsed.url)
        .with_context(|| format!("could not parse a host/owner/repo out of {}", parsed.url))
}

/// Confirm `url` is actually a merged pull request, then rewrite `state`'s
/// `status` and `merge` exactly as the automatic land loop (`land::land`)
/// would have written them had magi opened and merged this pull request
/// itself.
///
/// This is `magi fold --merged`'s whole implementation, and also what the
/// web `fold-merged` route calls once it has a URL in hand — an operator
/// recovery path for a merge magi could not finish on its own: a PR title too
/// long for the GraphQL mutation, `gh pr create` unreachable, a stale token -
/// closed by hand with a pull request magi never opened and so never
/// recorded. Reusing `land::land` rather than writing `status`/`merge`
/// directly keeps this one authoritative: a merged pull request decides
/// `Step::Done { merged: true }` on the very first read, before any of
/// `land`'s own checks/fix/rebase machinery can run, which is what makes it
/// safe to call here even though this pull request was never magi's own.
///
/// [`ensure_same_repo`] is checked before anything else: a pull request from
/// a different repository than the one `state` is recorded against is
/// refused outright, regardless of its lifecycle. This is the guard for a
/// URL an *operator* hands in - the CLI or the web route - where a stale or
/// mistyped run id could otherwise get corrected from an unrelated
/// repository's pull request (see the shun/8c75 incident in `magi fold`'s own
/// CLI help). The automatic janitor sweep (`clean::reconcile_external_merges`)
/// goes through [`correct_confirmed_external_merge`] instead, which skips
/// this check: its `url` was never operator-supplied, it comes from
/// [`find_external_merge`] querying `gh` from inside `state.repo` itself, so
/// it is already guaranteed to name a pull request in that same repository -
/// re-deriving and re-checking the repository here would only be a second
/// `gh repo view` call that can fail for reasons that have nothing to do with
/// correctness (a rate limit, a network blip), turning a self-heal that would
/// otherwise have succeeded into a run left `Blocked` for another pass.
///
/// [`lifecycle`] is checked next and separately so a mistyped or still-open
/// URL fails loudly without writing anything, rather than handing an open
/// pull request to the full autonomous loop by accident.
///
/// Correcting `status` this way does not run `bump::after_merge`
/// (`src/bump.rs`): that call is made only from `graph::Runner::run_land`,
/// which this path never goes through. A release version bump the change
/// might have earned is therefore not filed automatically and has to be
/// requested by hand - recorded as an event on the run so the gap is visible
/// to whoever reads it later, not just wherever this was called from. Follow-up
/// tasks for findings the merge left open (`crate::followup`) *are* filed here.
///
/// Returns the status before and after, so every caller (CLI, janitor, web
/// route) can build its own log line or response from the same pair rather
/// than each re-deriving it.
pub async fn correct_manual_merge(
    state: &mut RunState,
    url: &str,
) -> Result<(RunStatus, RunStatus)> {
    let Some(pr_slug) = slug_of_pr_url(url) else {
        bail!(
            "could not parse an owner/repo out of {url}; refusing to guess which repository \
             this pull request belongs to"
        );
    };
    let run_slug = repo_slug(&state.repo).await?;
    ensure_same_repo(&run_slug, &pr_slug)?;
    correct_merge(state, url).await
}

/// The janitor's own entry point into the same correction
/// [`correct_manual_merge`] performs for an operator-supplied URL, minus the
/// same-repo guard - see that function's own doc for why skipping it here is
/// safe rather than a hole: [`clean::reconcile_external_merges`] only ever
/// calls this with a `url` [`find_external_merge`] already found by querying
/// `state.repo`'s own remote, so the guard could never do anything here but
/// fail on its own transient errors.
///
/// [`crate::clean`] is the only caller; `pub(crate)` rather than private only
/// because it lives in a different module.
pub(crate) async fn correct_confirmed_external_merge(
    state: &mut RunState,
    url: &str,
) -> Result<(RunStatus, RunStatus)> {
    correct_merge(state, url).await
}

/// Same pull request, same repository: the url, or the number within one repo.
fn names_same_pr(a: &RunState, url: &str, number: u64, repo: &Path) -> bool {
    let Some(pr) = a.pr.as_ref() else {
        return false;
    };
    if !url.is_empty()
        && pr
            .url
            .trim_end_matches('/')
            .eq_ignore_ascii_case(url.trim_end_matches('/'))
    {
        return true;
    }
    number > 0
        && pr.number == number
        && match (a.repo.canonicalize(), repo.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => a.repo == repo,
        }
}

/// Rewrite `pr.state` to a final state on every run record under `home` that
/// `decide` picks, and report how many were rewritten.
///
/// This is the one place a run other than the driver changes another run's
/// record, so it is deliberately narrow: only a **terminal** run (never one a
/// driver may still be writing), never one a live daemon claims, only a record
/// whose `pr.state` is still `open`, and only `merged` / `closed` ever goes in.
/// The record is read as folding reads it (no schema check: every bump so far
/// only added fields, and the whole struct round-trips) and written through
/// [`RunState::save_under`], the path every record uses, so nothing but
/// `pr.state` and an event line changes (and `updated_at`, as for any save).
/// A run that cannot be read or written is skipped with a warning.
fn rewrite_open_prs(
    home: &Path,
    decide: &mut dyn FnMut(&RunState) -> Option<PrLifecycle>,
) -> usize {
    let now = Timestamp::now();
    let mut changed = 0;
    for id in crate::run::list_ids_in(&home.join("runs")) {
        let path = home.join("runs").join(&id).join("run.json");
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut state) = serde_json::from_str::<RunState>(&body) else {
            continue;
        };
        if !state.status.done()
            || state.pr.as_ref().is_none_or(|p| p.state != "open")
            || crate::daemon::is_working_on(home, &id, now)
        {
            continue;
        }
        let Some(to @ (PrLifecycle::Merged | PrLifecycle::Closed)) = decide(&state) else {
            continue;
        };
        if let Some(pr) = state.pr.as_mut() {
            pr.state = to.as_str().to_owned();
        }
        let url = state.pr.as_ref().map(|p| p.url.clone()).unwrap_or_default();
        state.event(
            "land",
            format!("recorded {url} as {}: another run settled it", to.as_str()),
        );
        match state.save_under(home) {
            Ok(()) => changed += 1,
            Err(e) => tracing::warn!("write pr state through to run {id}: {e:#}"),
        }
    }
    changed
}

/// A run reached a final state for its pull request: tell every other terminal
/// run in the same repository that names the same pull request (handed-over
/// predecessors, blocked attempts, anything), so their records stop saying
/// `open`. Best effort; `run`'s own record is the caller's.
pub(crate) fn write_pr_state_through(run: &RunState, to: PrLifecycle) {
    if to == PrLifecycle::Open {
        return;
    }
    let Some(home) = crate::run::try_home() else {
        return;
    };
    write_pr_state_through_in(&home, run, to);
}

pub(crate) fn write_pr_state_through_in(home: &Path, run: &RunState, to: PrLifecycle) -> usize {
    let Some(pr) = run.pr.as_ref() else {
        return 0;
    };
    let (url, number) = (pr.url.clone(), pr.number);
    rewrite_open_prs(home, &mut |other| {
        (other.id != run.id && names_same_pr(other, &url, number, &run.repo)).then_some(to)
    })
}

/// Terminal runs whose record still says their pull request is open, as
/// `(run id, repo, url)`, for [`repair_stale_pr_states`].
pub(crate) fn stale_open_prs(home: &Path) -> Vec<(String, PathBuf, String)> {
    let now = Timestamp::now();
    let mut out = Vec::new();
    for id in crate::run::list_ids_in(&home.join("runs")) {
        let path = home.join("runs").join(&id).join("run.json");
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<RunState>(&body) else {
            continue;
        };
        if let Some(pr) = state.pr.as_ref()
            && state.status.done()
            && pr.state == "open"
            && !pr.url.is_empty()
            && !crate::daemon::is_working_on(home, &id, now)
        {
            out.push((id, state.repo.clone(), pr.url.clone()));
        }
    }
    out
}

/// Apply forge answers (`pr url -> state`) to every stale terminal record.
/// A url with no answer (the forge was unreadable) changes nothing.
pub(crate) fn apply_pr_states(home: &Path, known: &BTreeMap<String, PrLifecycle>) -> usize {
    rewrite_open_prs(home, &mut |s| {
        s.pr.as_ref().and_then(|p| known.get(&p.url)).copied()
    })
}

/// One-time (and idempotent) repair of records that froze an `open` pull
/// request: ask the forge about each distinct pull request that a terminal run
/// still calls open - at most `max_lookups` of them - and rewrite the merged
/// and closed ones. A genuinely open pull request is left alone, and a lookup
/// that fails is "unknown, change nothing". Returns `(records rewritten,
/// lookups that failed)`.
pub async fn repair_stale_pr_states(home: &Path, max_lookups: usize) -> (usize, usize) {
    let mut known = BTreeMap::new();
    let mut failed = 0;
    let mut seen = BTreeSet::new();
    for (_, repo, url) in stale_open_prs(home) {
        if known.len() + failed >= max_lookups || !seen.insert(url.clone()) {
            continue;
        }
        match lifecycle(&repo, &url).await {
            Ok(state) => {
                known.insert(url, state);
            }
            Err(e) => {
                tracing::warn!("repair pr state of {url}: {e:#}");
                failed += 1;
            }
        }
    }
    (apply_pr_states(home, &known), failed)
}

async fn correct_merge(state: &mut RunState, url: &str) -> Result<(RunStatus, RunStatus)> {
    match lifecycle(&state.repo, url).await? {
        PrLifecycle::Merged => {}
        other => bail!(
            "{url} is {}, not merged; refusing to record {} as merged on a guess",
            other.as_str(),
            state.id
        ),
    }
    let before = state.status;
    if let Err(e) = land(state, url).await {
        // `land` sets `status` to `Landing` and saves before its first read
        // of the pull request — see its own doc — so a failure here (a
        // transient `gh` hiccup between the two forge reads this function
        // makes) can leave the run stuck on that in-between value with
        // nothing left driving it. Land it on the same terminal shape an
        // automated `land` failure lands on instead of leaving it stuck.
        state.status = RunStatus::Blocked;
        state.event("fold", format!("manual-merge correction failed: {e:#}"));
        state.save()?;
        return Err(e).context(format!("confirming the merge of {url}"));
    }
    state.event(
        "fold",
        "operator recorded this pull request as a manual merge; this run never \
         re-entered `land`, so `bump::after_merge` did not run for it - a release \
         bump this change might warrant has to be filed by hand",
    );
    // Unlike the bump, the findings the merge left open are filed on this
    // path too: nothing else would ever carry them forward.
    if state.status == RunStatus::Merged {
        crate::followup::after_merge(state, url).await;
    }
    state.save()?;
    Ok((before, state.status))
}

/// Parse `gh api repos/{owner}/{repo}/pulls/<n>/comments` into inline review
/// comments. No I/O.
///
/// `gh pr view` does not surface inline comments, and inline is exactly where
/// both review bots put their findings - a landing loop that read only the
/// top-level thread would never see the thing it is supposed to fix.
pub fn parse_inline_comments(json: &str) -> Result<Vec<ReviewComment>> {
    let raw: Vec<GhInline> =
        serde_json::from_str(json).context("parse `gh api .../pulls/<n>/comments` output")?;
    let mut out = Vec::new();
    for c in raw {
        push_if_outstanding(
            &mut out,
            ReviewComment {
                author: c.user.login,
                path: c.path,
                line: c.line,
                body: c.body,
            },
        );
    }
    Ok(out)
}

/// Keep a comment only when it asks for something.
///
/// An inline comment always does: it names a file and a line. A top-level
/// comment is dropped when it is empty, when it is magi's own, or when it is
/// [noise](is_noise).
fn push_if_outstanding(out: &mut Vec<ReviewComment>, comment: ReviewComment) {
    if comment.body.trim().is_empty() || comment.body.contains(MARKER) {
        return;
    }
    if comment.path.is_none() && is_noise(&comment.body) {
        return;
    }
    out.push(comment);
}

/// Is this comment body machinery rather than a finding?
///
/// Two tests, both structural, because guessing from prose is how a "looks
/// good to me" turns into a fix round:
///
/// 1. The bot said so - the body carries one of the [`NOT_A_REVIEW`] markers
///    with which CodeRabbit labels its trigger notice, its walkthrough, and its
///    footer.
/// 2. It asks for nothing - once HTML comments, `<details>` blocks, headings,
///    horizontal rules, and the bot's own status banner are removed, every
///    remaining line is a task-list item. That is exactly the shape of the
///    comment the Claude review job posts while it is still working.
///
/// Anything else is input, including bot prose. A bot that writes a paragraph
/// has said something, and the fix prompt tells the fixer it may decline a
/// comment with an argument - a wasted sentence in a prompt is cheaper than a
/// missed finding.
pub fn is_noise(body: &str) -> bool {
    if NOT_A_REVIEW.iter().any(|m| body.contains(m)) {
        return true;
    }
    let mut content = false;
    for line in strip_blocks(body).lines() {
        let line = unquote(line);
        if line.is_empty() || is_checklist(line) || is_decoration(line) || is_banner(line) {
            continue;
        }
        content = true;
        break;
    }
    !content
}

/// Remove HTML comments and collapsed `<details>` blocks.
fn strip_blocks(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    loop {
        let open = ["<!--", "<details>"]
            .iter()
            .filter_map(|tag| rest.find(tag).map(|i| (i, *tag)))
            .min_by_key(|(i, _)| *i);
        let Some((at, tag)) = open else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        let after = &rest[at + tag.len()..];
        let close = if tag == "<!--" { "-->" } else { "</details>" };
        match after.find(close) {
            Some(end) => rest = &after[end + close.len()..],
            // Unterminated: the rest of the body is inside the block.
            None => return out,
        }
    }
}

/// Strip blockquote markers, which both bots wrap their callouts in.
fn unquote(line: &str) -> &str {
    let mut s = line.trim();
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start();
    }
    s.trim()
}

/// `- [ ]` / `- [x]`, in any of the bullet styles GitHub renders.
fn is_checklist(line: &str) -> bool {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .unwrap_or("");
    let rest = rest.trim_start();
    matches!(
        rest.get(..3),
        Some("[ ]") | Some("[x]") | Some("[X]") | Some("[*]")
    )
}

/// A heading, a horizontal rule, or a callout tag - shape, never content.
fn is_decoration(line: &str) -> bool {
    line.starts_with('#')
        || line.starts_with("[!")
        || (line.len() >= 3 && line.chars().all(|c| matches!(c, '-' | '=' | '*' | '_')))
}

/// A line that is nothing but emphasis and links.
///
/// Both review jobs open with a status banner
/// (`**Claude finished ... in 4m 14s** —— [View job](url)`). It reads as prose
/// to a line-based test and asks for nothing, so it is measured the same way a
/// heading is: strip the markup, and if no word survives, it was decoration.
fn is_banner(line: &str) -> bool {
    let plain = drop_spans(line, "**", "**");
    let plain = if plain.contains("](") {
        drop_spans(&plain, "[", ")")
    } else {
        plain
    };
    !plain.chars().any(char::is_alphanumeric)
}

/// Remove every `open` .. `close` span, including the delimiters. An
/// unterminated span swallows the rest of the input, which is what a reader
/// sees too.
fn drop_spans(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(open) {
        out.push_str(&rest[..at]);
        let after = &rest[at + open.len()..];
        match after.find(close) {
            Some(end) => rest = &after[end + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The lock that keeps at most one run per repository actually moving the
/// base branch at a time: a rebase push, or `gh pr merge`.
///
/// Deliberately narrow. Everything else in [`land`]'s loop - watching CI,
/// running a fix round in the winner's own worktree, waiting on the owner's
/// approval - touches nothing a *different* run in the same repository could
/// collide with, and holding a lock across any of that would serialise one
/// run's CI wait (up to [`WAIT_CEILING`]) against another run's land-approval
/// resume, which is precisely the "must not wait on another task" property
/// the daemon's slot-freeing exists to give a resume. Only the two moments
/// that actually write to the shared base branch need mutual exclusion, and
/// both are brief.
///
/// One entry per repository, each its own `tokio::sync::Mutex`, so two
/// different repositories' runs never wait on each other. The outer
/// `std::sync::Mutex` guards only the map itself, held long enough to find or
/// insert an entry and clone its `Arc`, never across an `.await`.
fn repo_merge_lock(repo: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<BTreeMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));
    LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(repo.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// `owner/repo` out of a pull request url, falling back to the checkout's
/// directory name when the url is not the usual `host/owner/repo/pull/N`.
fn repo_label(repo: &Path, pr_url: &str) -> String {
    let parts: Vec<&str> = pr_url.split('/').collect();
    if let Some(at) = parts.iter().rposition(|p| *p == "pull")
        && at >= 2
        && !parts[at - 1].is_empty()
        && !parts[at - 2].is_empty()
    {
        return format!("{}/{}", parts[at - 2], parts[at - 1]);
    }
    repo.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The operator-facing sentence for a merge that went ahead with red checks,
/// or `None` when the checks were not red. Judged on `checks`, not on
/// `failing`, which can be non-empty on a green observation.
fn red_merge_summary(repo_name: &str, pr: &PrState) -> Option<String> {
    (pr.checks == Checks::Red).then(|| {
        format!(
            "Merged {repo_name} PR #{} with red checks: {} ({})",
            pr.number,
            if pr.failing.is_empty() {
                "(none named)".to_owned()
            } else {
                pr.failing.join(", ")
            },
            pr.url
        )
    })
}

/// After a merge that succeeded: record which checks were red and tell the
/// operator. The decision to merge is already made; this only makes it audible.
/// Best-effort - a broken notifier never fails the run.
async fn announce_red_merge(state: &mut RunState, pr: &PrState) {
    let repo_name = repo_label(&state.repo, &pr.url);
    let Some(summary) = red_merge_summary(&repo_name, pr) else {
        return;
    };
    if let Some(rec) = state.pr.as_mut() {
        rec.red_at_merge = pr.failing.clone();
    }
    state.event("land", summary.clone());
    // The notice pages through `[notify]` itself, once, unless a question
    // already carries the cause.
    crate::notices::raise_with(
        crate::notices::merged_red(&state.id, &summary),
        &state.config.notify,
    );
}

/// Run the loop against a real pull request until it merges or the budget runs
/// out.
///
/// The caller decides whether landing happens at all: this is only reached when
/// `graph.land` is on. Returns the last observation, so the caller can report
/// what magi was looking at when it stopped.
pub async fn land(state: &mut RunState, pr_url: &str) -> Result<PrState> {
    land_with(state, pr_url, &GhForge).await
}

/// The forge calls whose timing the loop's decisions depend on, behind a seam
/// so a test can script what the pull request looks like from one observation
/// to the next. Comments, logs and rebases stay on `gh` and `git` directly.
trait Forge {
    async fn view(&self, repo: &Path, pr_url: &str) -> Result<Seen>;
    async fn merge(&self, repo: &Path, argv: &[String]) -> Result<(bool, String)>;
    async fn poll(&self);
    #[allow(clippy::too_many_arguments)]
    async fn fix(
        &self,
        state: &mut RunState,
        pr: &PrState,
        round: usize,
        budget: usize,
        reason: &str,
        logs: &str,
    ) -> Result<Fixed>;
}

struct GhForge;

impl Forge for GhForge {
    async fn view(&self, repo: &Path, pr_url: &str) -> Result<Seen> {
        observe(repo, pr_url).await
    }
    async fn merge(&self, repo: &Path, argv: &[String]) -> Result<(bool, String)> {
        gh(repo, argv).await
    }
    async fn poll(&self) {
        tokio::time::sleep(POLL).await;
    }
    async fn fix(
        &self,
        state: &mut RunState,
        pr: &PrState,
        round: usize,
        budget: usize,
        reason: &str,
        logs: &str,
    ) -> Result<Fixed> {
        fix_round(state, pr, round, budget, reason, logs).await
    }
}

/// Is the observation older than the commit a fix round pushed?
///
/// The forge takes a few seconds to move the pull request to a new head and
/// attach that head's check runs, and until it has, the rollup is the previous
/// head's - all green, which is exactly what a merge decision must not read.
/// An absent or different head counts as not yet: guessing "close enough"
/// would reopen the hole.
fn awaiting_new_head(awaiting: Option<&str>, observed: &str) -> bool {
    awaiting.is_some_and(|want| !observed.eq_ignore_ascii_case(want))
}

/// The commit an observation may be decided on, or `None` while it cannot be
/// trusted to describe one.
///
/// `None` when a pushed commit is awaited and the pull request is not on it,
/// when the head is unreadable, or when the rollup (`statusCheckRollup` is the
/// last commit's) is not the head's: that is the previous commit's checks. The
/// caller re-polls on `None`. A rollup that cannot be read (including one
/// longer than a page) leaves `rollup_head` empty, so the wait runs to
/// [`WAIT_CEILING`] and stops - a safe failure, never a merge.
fn bound_head<'a>(
    seen_head: &'a str,
    rollup_head: &str,
    awaiting: Option<&str>,
) -> Option<&'a str> {
    if seen_head.is_empty()
        || awaiting_new_head(awaiting, seen_head)
        || !rollup_head.eq_ignore_ascii_case(seen_head)
    {
        return None;
    }
    Some(seen_head)
}

/// What a refused `gh pr merge` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    /// The branch policy is not satisfied *yet*: go back to waiting.
    Pending,
    /// Checks have settled and the merge state still says no, seen for the
    /// first time. The forge updates `mergeStateStatus` a moment after the last
    /// check finishes, so one more look is allowed before believing it.
    Recheck,
    /// Nothing is in flight and the policy still refuses: a person has to
    /// supply what it asks for (a review, say).
    Final,
}

/// Judged from the pull request's state after the refusal, never from the
/// refusal's wording, which belongs to the forge and changes.
fn classify_refusal(after: Option<&Seen>, rechecked: bool, observed_head: &str) -> Refused {
    let Some(after) = after else {
        // Unreadable is not evidence of anything; the next loop reads again.
        return Refused::Pending;
    };
    if after.pr.state != PrLifecycle::Open {
        return Refused::Final;
    }
    // The branch moved after it was observed (the forge refuses a merge pinned
    // to the old commit): not a verdict on anything, look again.
    if !after.head.eq_ignore_ascii_case(observed_head) {
        return Refused::Pending;
    }
    // The same binding the loop applies: checks of another commit say nothing.
    if bound_head(&after.head, &after.rollup_head, None).is_none() {
        return Refused::Pending;
    }
    let state = after.merge_state.to_ascii_uppercase();
    if matches!(after.pr.checks, Checks::Pending | Checks::Unknown)
        || state.is_empty()
        || state == "UNKNOWN"
    {
        return Refused::Pending;
    }
    if rechecked {
        Refused::Final
    } else {
        Refused::Recheck
    }
}

/// Take auto-merge back from the forge and forget that it was armed.
///
/// `Err` carries the forge's message: the caller must not push or move on, as
/// an armed merge left behind could still fire for a commit nobody approved.
async fn disarm<F: Forge>(
    forge: &F,
    state: &mut RunState,
    repo: &Path,
    number: u64,
) -> std::result::Result<(), String> {
    let argv = disable_automerge_argv(number);
    let out = {
        let merge_lock = repo_merge_lock(repo);
        let _merge_slot = merge_lock.lock().await;
        forge.merge(repo, &argv).await
    };
    match out {
        Ok((true, _)) => {
            state.land_armed_head = None;
            state.event("land", "auto-merge disabled");
            state.save().map_err(|e| format!("{e:#}"))?;
            Ok(())
        }
        Ok((false, msg)) => Err(msg),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// [`stop`], first taking back an armed auto-merge so a pull request magi
/// gave up on does not merge on its own later. A failure to disarm is added
/// to the reason rather than hiding it.
async fn stop_disarmed<F: Forge>(
    forge: &F,
    state: &mut RunState,
    repo: &Path,
    pr: &PrState,
    why: &str,
) -> Result<()> {
    if state.land_armed_head.is_none() {
        return stop(state, repo, pr, why).await;
    }
    match disarm(forge, state, repo, pr.number).await {
        Ok(()) => stop(state, repo, pr, why).await,
        Err(e) => {
            let why = format!("{why} (auto-merge could not be disabled and may still fire: {e})");
            stop(state, repo, pr, &why).await
        }
    }
}

async fn land_with<F: Forge>(state: &mut RunState, pr_url: &str, forge: &F) -> Result<PrState> {
    let repo = state.repo.clone();
    let budget = state.config.graph.land_rounds;
    let mut round = 0usize;
    // Counted apart from `round`: a rebase is not a fix, and a base that
    // moved is not the change's fault.
    let mut rebases = 0usize;
    let mut waited = Duration::ZERO;
    // Comment bodies the fixer has already been shown. A comment is
    // outstanding until it has been handed over once; after that it is a
    // recorded decision, not an open question, and re-feeding it would loop the
    // budget away on a comment the fixer already declined with an argument.
    let mut shown: BTreeSet<String> = BTreeSet::new();
    // The head a fix round pushed, until the pull request is seen on it. Memory
    // only: a resume after a crash between the push and the next look can read
    // the old head's green once more, and a refused merge then waits it out.
    let mut awaiting_head: Option<String> = None;
    // Whether a settled-checks refusal has already been given its one re-look.
    let mut rechecked = false;

    // Marks the run resumable through exactly this function, not through a
    // fresh competition: `RunStatus::resumable` excludes only `Merged`,
    // `Ready` and `Failed`, and `merge`'s own re-entry guard looks for this
    // status specifically to know a resumed run belongs back in `land`
    // rather than at a second `gh pr create`. Set on every entry - fresh or
    // resumed - because a resume that parked here again must keep reading
    // `Landing`, not whatever a first pass through `merge` left behind.
    state.status = RunStatus::Landing;
    state.event("land", format!("watching {pr_url}"));
    state.save()?;

    // An armed merge recorded by an earlier pass may or may not have reached
    // the forge (the record is written before the call). It is taken back on
    // the first observation, where a pull request is known to stop with.
    let mut resumed_armed = state.land_armed_head.is_some();

    loop {
        let seen = forge.view(&repo, pr_url).await?;
        let mut pr = seen.pr.clone();
        pr.review_comments.retain(|c| !shown.contains(&c.body));
        state.pr = Some(crate::run::PrRecord {
            url: pr.url.clone(),
            number: pr.number,
            state: pr.state.as_str().to_owned(),
            checks: pr.checks.as_str().to_owned(),
            round,
            rounds: budget,
            red_at_merge: Vec::new(),
        });
        state.save()?;

        if std::mem::take(&mut resumed_armed) && pr.state == PrLifecycle::Open {
            // The normal path re-arms, and an approval already given for the
            // same head is reused, so nothing is asked twice. A failed disable
            // stops the run with the record kept: pushing on could change the
            // head under an arm that is still live.
            if let Err(e) = disarm(forge, state, &repo, pr.number).await {
                let why = format!(
                    "a previous pass may have armed auto-merge and it could not be disabled \
                     on resume: {e}"
                );
                stop(state, &repo, &pr, &why).await?;
                return Ok(pr);
            }
        }

        // The head moved away from the one auto-merge was armed on, by
        // someone other than this loop. The arm is pinned and would refuse to
        // merge the new commit, but the approval was for the old one: take it
        // back and go through the normal path again.
        if pr.state == PrLifecycle::Open
            && !seen.head.is_empty()
            && state
                .land_armed_head
                .as_deref()
                .is_some_and(|armed| !armed.eq_ignore_ascii_case(&seen.head))
        {
            if let Err(e) = disarm(forge, state, &repo, pr.number).await {
                let why = format!(
                    "the head moved while auto-merge was armed and it could not be disabled: {e}"
                );
                stop(state, &repo, &pr, &why).await?;
                return Ok(pr);
            }
        }

        if pr.state == PrLifecycle::Open {
            if bound_head(&seen.head, &seen.rollup_head, awaiting_head.as_deref()).is_none() {
                if waited >= WAIT_CEILING {
                    let want = awaiting_head.as_deref().unwrap_or_default();
                    let why = format!(
                        "the pull request's checks were still not about one readable head after \
                         {} minutes (expected {}, pull request points at {}, checks are for {}); \
                         someone may have pushed over it",
                        WAIT_CEILING.as_secs() / 60,
                        if want.is_empty() { "any" } else { want },
                        if seen.head.is_empty() {
                            "nothing readable"
                        } else {
                            &seen.head
                        },
                        if seen.rollup_head.is_empty() {
                            "nothing readable"
                        } else {
                            &seen.rollup_head
                        },
                    );
                    stop_disarmed(forge, state, &repo, &pr, &why).await?;
                    return Ok(pr);
                }
                waited += POLL;
                forge.poll().await;
                continue;
            }
            // Reset once, when the awaited head first shows up; resetting on
            // every re-observation would let a standing refusal wait forever.
            if awaiting_head.take().is_some() {
                // The checks now being read belong to the new head; give them
                // the same grace a fresh pull request gets.
                waited = Duration::ZERO;
            }
        }

        let step = decide(&pr, round, budget, waited);
        // Armed on exactly this head: the forge is doing the waiting. A
        // decision to merge (or keep waiting) is only watched, never re-armed
        // and never re-approved; anything else - a red check, a comment, a
        // conflict - falls through to its own arm, which disarms first.
        let armed_here = state
            .land_armed_head
            .as_deref()
            .is_some_and(|armed| armed.eq_ignore_ascii_case(&seen.head));
        if armed_here && matches!(step, Step::Merge | Step::Wait) {
            if waited >= WAIT_CEILING {
                let why = format!(
                    "auto-merge was armed on {} but the pull request did not merge within {} \
                     minutes ({})",
                    seen.head,
                    WAIT_CEILING.as_secs() / 60,
                    waiting_on(&seen.merge_state, &seen.contexts)
                );
                stop_disarmed(forge, state, &repo, &pr, &why).await?;
                return Ok(pr);
            }
            waited += POLL;
            forge.poll().await;
            continue;
        }
        if armed_here && !matches!(step, Step::Done { .. }) {
            // Not merging or waiting any more: whatever is next may change the
            // head or give up, and neither may leave the arm standing.
            if let Err(e) = disarm(forge, state, &repo, pr.number).await {
                let why = format!("auto-merge could not be disabled: {e}");
                stop(state, &repo, &pr, &why).await?;
                return Ok(pr);
            }
        }
        match step {
            Step::Wait => {
                if waited >= WAIT_CEILING {
                    let why = format!(
                        "checks were still running after {} minutes",
                        WAIT_CEILING.as_secs() / 60
                    );
                    stop(state, &repo, &pr, &why).await?;
                    return Ok(pr);
                }
                waited += POLL;
                forge.poll().await;
            }
            Step::Done { merged } => {
                // Auto-merge is pinned to the approved head, but the forge's
                // own handling of a later push is not something magi can
                // see: if the pull request merged on another commit, say so
                // loudly rather than record a clean landing.
                if let Some(armed) = state.land_armed_head.take() {
                    if merged && !seen.head.is_empty() && !armed.eq_ignore_ascii_case(&seen.head) {
                        let msg = format!(
                            "{} merged on {} but the owner approved {armed}; review what landed",
                            pr.url, seen.head
                        );
                        tracing::warn!("{msg}");
                        state.event("land", msg);
                    }
                }
                state.status = if merged {
                    RunStatus::Merged
                } else {
                    RunStatus::Ready
                };
                let detail = if merged {
                    format!("{} was merged", pr.url)
                } else {
                    format!("{} was closed without merging", pr.url)
                };
                state.merge = Some(MergeOutcome {
                    mode: MergeMode::Pr,
                    ok: merged,
                    detail: detail.clone(),
                    empty: false,
                });
                state.event("land", detail);
                state.save()?;
                write_pr_state_through(state, pr.state);
                return Ok(pr);
            }
            Step::Merge => {
                let subject = merge_subject(
                    crate::graph::landing_title(state, &seen.title),
                    &crate::graph::landing_subject_source(state),
                );
                // The owner sees the panel before the one irreversible step,
                // and an unanswered question is a hold: silence never merges.
                //
                // `land_approval` asks about every merge. With it off, a
                // review hand-off the panel contested (a blocking finding
                // open and a reject vote) is asked about all the same.
                let contested = contested_to_ask(state);
                if state.config.graph.land_approval || contested.is_some() {
                    match approval_gate(state, &pr, &subject, contested.as_ref(), &seen.head)
                        .await?
                    {
                        ApprovalGate::Approved => {}
                        ApprovalGate::Held => {
                            stop(
                                state,
                                &repo,
                                &pr,
                                "the owner did not approve the merge (held or unanswered)",
                            )
                            .await?;
                            return Ok(pr);
                        }
                        // Filed (or still standing from an earlier visit) and
                        // not yet answered. Park here rather than wait: the
                        // question survives on disk, the daemon hands this
                        // run's slot to something else, and a later resume
                        // re-enters `land`, finds the same question, and
                        // either merges or stops depending on what it says
                        // by then.
                        ApprovalGate::Pending => {
                            state.parked = true;
                            state.event(
                                "land",
                                "parked awaiting merge approval - resumes once answered",
                            );
                            state.save()?;
                            return Ok(pr);
                        }
                    }
                }
                // Open and past the gate above, so `seen.head` is the commit
                // the checks were bound to and the owner approved.
                //
                // Arm auto-merge on that commit rather than merging now: the
                // forge waits for the required checks against its own view and
                // merges only while the head is still this one, so an
                // observation that is a moment stale cannot become a merge.
                // The intent is saved first, so a crash around the call is
                // still visible to a resume.
                let observed_head = seen.head.clone();
                // `gh` merges at once instead of arming when the pull request
                // is mergeable by the time it looks, and the state seen here
                // can be a moment older than that - BLOCKED now, CLEAN when
                // `gh` asks. So every arm gets the same fresh, head-bound read
                // the direct fallback does, whatever the merge state was.
                {
                    let fresh = forge.view(&repo, pr_url).await.ok();
                    if !direct_merge_is_safe(
                        fresh.as_ref(),
                        &observed_head,
                        &shown,
                        round,
                        budget,
                        waited,
                    ) {
                        if waited >= WAIT_CEILING {
                            let why = "the pull request did not settle on the approved head \
                                       before it could be merged";
                            stop(state, &repo, &pr, why).await?;
                            return Ok(pr);
                        }
                        state.event(
                            "land",
                            "the pull request changed before merging; looking again",
                        );
                        waited += POLL;
                        forge.poll().await;
                        continue;
                    }
                }
                let arm_argv = automerge_argv_at(pr.number, &subject, &observed_head);
                state.land_armed_head = Some(observed_head.clone());
                state.save()?;
                let arm_out = {
                    let merge_lock = repo_merge_lock(&repo);
                    let _merge_slot = merge_lock.lock().await;
                    forge.merge(&repo, &arm_argv).await?
                };
                if arm_out.0 {
                    state.event(
                        "land",
                        format!("auto-merge armed on {observed_head} for {}", pr.url),
                    );
                    state.save()?;
                    // The ceiling counts from here: what is awaited now is
                    // the forge's merge, not the checks magi was waiting on.
                    waited = Duration::ZERO;
                    forge.poll().await;
                    continue;
                }
                state.land_armed_head = None;
                state.save()?;

                let unavailable = automerge_unavailable(&arm_out.1);
                let arm_msg = arm_out.1.clone();
                let (argv, out, forced_verdict) = if unavailable {
                    // Nothing to wait on or no way to wait: merge directly, but
                    // only on the approved head and only after reading the
                    // pull request again and finding that head, its checks
                    // and the policy all still say merge.
                    let fresh = forge.view(&repo, pr_url).await.ok();
                    if direct_merge_is_safe(
                        fresh.as_ref(),
                        &observed_head,
                        &shown,
                        round,
                        budget,
                        waited,
                    ) {
                        let argv = merge_argv_at(pr.number, &subject, &observed_head);
                        let out = {
                            let merge_lock = repo_merge_lock(&repo);
                            let _merge_slot = merge_lock.lock().await;
                            forge.merge(&repo, &argv).await?
                        };
                        (argv, out, None)
                    } else {
                        state.event(
                            "land",
                            "auto-merge is unavailable and the pull request changed before a \
                             direct merge; looking again",
                        );
                        (arm_argv, (false, arm_msg.clone()), Some(Refused::Pending))
                    }
                } else {
                    (arm_argv, arm_out, None)
                };
                if out.0 {
                    pr.state = PrLifecycle::Merged;
                    state.status = RunStatus::Merged;
                    state.merge = Some(MergeOutcome {
                        mode: MergeMode::Pr,
                        ok: true,
                        detail: format!("gh {}", argv.join(" ")),
                        empty: false,
                    });
                    // The last `state.pr` snapshot is whatever the poll before
                    // this merge observed - still `open` - and nothing below
                    // refreshes it from GitHub again, so the UI's round rail
                    // would otherwise keep animating a merged run forever.
                    if let Some(pr_record) = state.pr.as_mut() {
                        pr_record.state = pr.state.as_str().to_owned();
                    }
                    state.event("land", format!("merged {} as `{subject}`", pr.url));
                    announce_red_merge(state, &pr).await;
                    state.save()?;
                    write_pr_state_through(state, pr.state);
                    return Ok(pr);
                }
                let after_seen = forge.view(&repo, pr_url).await.ok();
                let after = after_seen.as_ref().map(|s| s.pr.state);
                if let Some(outcome) = merged_after_all(&argv, &out.1, after) {
                    pr.state = PrLifecycle::Merged;
                    state.status = RunStatus::Merged;
                    state.merge = Some(outcome);
                    if let Some(pr_record) = state.pr.as_mut() {
                        pr_record.state = pr.state.as_str().to_owned();
                    }
                    state.event("land", format!("merged {} as `{subject}`", pr.url));
                    announce_red_merge(state, &pr).await;
                    state.save()?;
                    write_pr_state_through(state, pr.state);
                    return Ok(pr);
                }
                let verdict = forced_verdict.unwrap_or_else(|| {
                    classify_refusal(after_seen.as_ref(), rechecked, &observed_head)
                });
                match verdict {
                    Refused::Final => {
                        let merge_state = after_seen
                            .as_ref()
                            .map(|s| s.merge_state.as_str())
                            .filter(|m| !m.is_empty())
                            .unwrap_or("unknown");
                        // Which refusal this was decides what a person has to
                        // do about it, so the two are worded apart; both carry
                        // the forge's own message.
                        let why = if unavailable {
                            format!(
                                "auto-merge is not available for this pull request ({arm_msg}) \
                                 and the direct merge was refused: {} (merge state: {merge_state})",
                                out.1
                            )
                        } else {
                            format!(
                                "auto-merge could not be armed because the requirements are not \
                                 met: {} (merge state: {merge_state})",
                                out.1
                            )
                        };
                        stop(state, &repo, &pr, &why).await?;
                        return Ok(pr);
                    }
                    verdict => {
                        // Back to the top, which re-decides and passes the
                        // approval gate again, a poll later. `waited` is not
                        // reset here: only a pushed fix restarts it, or a
                        // standing refusal would wait forever.
                        if waited >= WAIT_CEILING {
                            let why = format!(
                                "the merge was still refused after {} minutes: {}",
                                WAIT_CEILING.as_secs() / 60,
                                out.1
                            );
                            stop(state, &repo, &pr, &why).await?;
                            return Ok(pr);
                        }
                        if verdict == Refused::Recheck {
                            rechecked = true;
                        }
                        state.event(
                            "land",
                            "merge refused while the branch policy is not satisfied yet; waiting",
                        );
                        state.save()?;
                        waited += POLL;
                        forge.poll().await;
                    }
                }
            }
            Step::Rebase => {
                // Bounded by the same budget as a fix, because a rebase that
                // keeps being needed means the base moves faster than this
                // run can land and a person should decide what to do. It
                // spends none of that budget: the change is not what is
                // wrong.
                if rebases >= budget {
                    let why = format!(
                        "the base moved under this branch {budget} time(s) and it still does \
                         not merge; rebasing again would only race it"
                    );
                    stop(state, &repo, &pr, &why).await?;
                    return Ok(pr);
                }
                rebases += 1;
                let Some(branch) = state.winner().map(|w| w.branch.clone()) else {
                    stop(
                        state,
                        &repo,
                        &pr,
                        "the pull request conflicts and this run has no winning branch to rebase",
                    )
                    .await?;
                    return Ok(pr);
                };
                let base = state.base_branch.clone();
                state.event(
                    "land",
                    format!("{} no longer merges; rebasing onto {base}", pr.url),
                );
                state.save()?;

                // Onto the base as the *remote* has it: the local ref may be
                // behind, and rebasing onto a stale base produces a branch
                // that conflicts all over again.
                git::fetch(&repo, "origin", &base).await.ok();
                let scratch = state.dir().join("rebase");
                let onto = format!("origin/{base}");
                let rebased =
                    match crate::rebase::rebase_with_fixer(state, &scratch, &branch, &onto).await {
                        Ok(crate::rebase::Rebased::Applied) => Ok(None),
                        Ok(crate::rebase::Rebased::Stopped(why)) => Ok(Some(why)),
                        Err(e) => Err(e),
                    };
                match rebased {
                    Ok(None) => {
                        let pushed = {
                            let merge_lock = repo_merge_lock(&repo);
                            let _merge_slot = merge_lock.lock().await;
                            git::push_rewritten(&repo, "origin", &branch).await?
                        };
                        if !pushed.ok() {
                            let why = format!(
                                "rebased {branch} but could not push it: {}",
                                pushed.stderr.trim()
                            );
                            stop(state, &repo, &pr, &why).await?;
                            return Ok(pr);
                        }
                        // Bind to what was pushed: until the pull request is
                        // seen on it, the rollup is the old head's.
                        let head =
                            match git::rev_parse(&repo, &format!("refs/heads/{branch}")).await {
                                Ok(head) => head,
                                Err(e) => {
                                    let why = format!(
                                        "rebased and pushed {branch} but could not read the pushed \
                                     commit: {e:#}"
                                    );
                                    stop(state, &repo, &pr, &why).await?;
                                    return Ok(pr);
                                }
                            };
                        awaiting_head = Some(head);
                        rechecked = false;
                        state.event("land", format!("rebased {branch} onto {base}"));
                        state.save()?;
                        // The forge has to re-run its checks against the
                        // rebased head before anything else can be decided.
                        waited = Duration::ZERO;
                        tokio::time::sleep(POLL).await;
                    }
                    // The fixer's rounds are spent (or it could not finish):
                    // that is a decision for a person.
                    Ok(Some(conflict)) => {
                        let why = format!(
                            "{} conflicts with {base} and the rebase did not apply: {}",
                            pr.url,
                            conflict.chars().take(600).collect::<String>()
                        );
                        stop(state, &repo, &pr, &why).await?;
                        return Ok(pr);
                    }
                    Err(e) => {
                        let why = format!("could not rebase {branch} onto {base}: {e:#}");
                        stop(state, &repo, &pr, &why).await?;
                        return Ok(pr);
                    }
                }
            }
            Step::GiveUp { reason } => {
                stop(state, &repo, &pr, &reason).await?;
                return Ok(pr);
            }
            Step::Fix { reason } => {
                round += 1;
                waited = Duration::ZERO;
                for c in &pr.review_comments {
                    shown.insert(c.body.clone());
                }
                state.event("land", format!("round {round}: {reason}"));
                state.save()?;

                let logs = failing_logs(&repo, &seen.failing_urls).await;
                let was_red = pr.checks == Checks::Red;
                match forge.fix(state, &pr, round, budget, &reason, &logs).await? {
                    Fixed::Committed { head } => {
                        // Whatever the next look shows may still be the
                        // previous head; see `awaiting_new_head`.
                        awaiting_head = Some(head);
                        rechecked = false;
                        waited = Duration::ZERO;
                        forge.poll().await;
                    }
                    Fixed::Declined if was_red => {
                        let why = format!(
                            "the fixer produced no commit while {} check(s) were failing \
                             ({}); stopping instead of looping on an unchanged tree",
                            pr.failing.len(),
                            pr.failing.join(", ")
                        );
                        stop(state, &repo, &pr, &why).await?;
                        return Ok(pr);
                    }
                    // Comment-driven round with no commit: the fixer read the
                    // comments and changed nothing, which is a decision it is
                    // allowed to make. The comments are recorded as shown, so
                    // the next observation sees a clean pull request.
                    Fixed::Declined => state.event(
                        "land",
                        format!("round {round}: fixer declined the comments, nothing committed"),
                    ),
                    Fixed::Failed(why) => {
                        stop(state, &repo, &pr, &format!("the fix round failed: {why}")).await?;
                        return Ok(pr);
                    }
                }
                state.save()?;
            }
        }
    }
}

/// One observation, plus the two things [`PrState`] deliberately does not carry:
/// the title (needed for the squash subject) and where the failing checks'
/// logs live.
#[derive(Clone)]
struct Seen {
    pr: PrState,
    title: String,
    failing_urls: Vec<(String, String)>,
    /// `headRefOid`: the commit this observation, checks included, is about.
    head: String,
    /// The commit the checks belong to, read from the same GraphQL node as
    /// the checks themselves. Empty when unreadable.
    rollup_head: String,
    /// `mergeStateStatus` as the forge spelled it. [`Blocking`] folds BLOCKED,
    /// BEHIND and DRAFT together, and a stop reason has to say which.
    merge_state: String,
    /// Every check of the rollup, for naming what an armed merge waits on.
    contexts: Vec<CheckInfo>,
}

/// One check of the rollup as far as a stop reason needs it.
#[derive(Clone)]
struct CheckInfo {
    label: String,
    verdict: Verdict,
    required: Option<bool>,
}

/// Read the pull request: `gh pr view` for the top-level thread and merge
/// state, `gh api graphql` for the last commit and its checks, `gh api` for the
/// inline review comments `gh pr view` does not report.
async fn observe(repo: &Path, pr_url: &str) -> Result<Seen> {
    let view = gh(
        repo,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            pr_url.to_owned(),
            "--json".to_owned(),
            "url,number,state,title,reviews,comments,mergeStateStatus,headRefOid".to_owned(),
        ],
    )
    .await?;
    if !view.0 {
        bail!("gh pr view {pr_url}: {}", view.1);
    }
    let number = parse_pr(&view.1)?.number;
    let node = last_commit_node(repo, number).await;
    let mut seen = seen_from(&view.1, node.as_deref())?;

    let inline = gh(
        repo,
        &[
            "api".to_owned(),
            format!("repos/{{owner}}/{{repo}}/pulls/{}/comments", seen.pr.number),
        ],
    )
    .await?;
    if inline.0 {
        match parse_inline_comments(&inline.1) {
            Ok(mut comments) => seen.pr.review_comments.append(&mut comments),
            // An unreadable inline thread must not end a landing: the rollup
            // and the top-level thread are still real signal.
            Err(e) => tracing::warn!("inline review comments unreadable: {e}"),
        }
    } else {
        tracing::warn!("gh api pulls/{}/comments: {}", seen.pr.number, inline.1);
    }
    Ok(seen)
}

/// Build a [`Seen`] from the `gh pr view` json and the last-commit node
/// response. No I/O.
///
/// The commit oid and the checks are read from the *same* node, so the rollup
/// is bound to the commit it belongs to by construction; two reads that agree
/// before and after another response prove nothing about what that response
/// held. The view's own rollup is never used. A node that is missing,
/// unreadable, carries GraphQL errors, or whose checks run past the page
/// leaves `rollup_head` empty and the checks `Unknown`, which the loop treats
/// as "look again" and never as a reason to merge.
fn seen_from(view_json: &str, node_json: Option<&str>) -> Result<Seen> {
    let mut pr = parse_pr(view_json)?;
    let raw: GhPr = serde_json::from_str(view_json).context("re-read pull request json")?;

    let mut rollup_head = String::new();
    let mut failing_urls = Vec::new();
    let mut contexts = Vec::new();
    let mut checks = Checks::Unknown;
    let mut failing = Vec::new();
    if let Some((oid, rollup)) = node_json.and_then(parse_last_commit_node) {
        (checks, failing) = rollup_verdict(&rollup);
        failing_urls = rollup
            .iter()
            .filter(|c| c.verdict() == Verdict::Fail)
            .filter_map(|c| c.url().map(|u| (c.label(), u.to_owned())))
            .collect();
        contexts = rollup
            .iter()
            .map(|c| CheckInfo {
                label: c.label(),
                verdict: c.verdict(),
                required: c.is_required,
            })
            .collect();
        rollup_head = oid;
    }
    pr.checks = checks;
    pr.failing = failing;

    Ok(Seen {
        pr,
        title: raw.title,
        failing_urls,
        head: raw.head_ref_oid,
        rollup_head,
        merge_state: raw.merge_state_status,
        contexts,
    })
}

/// The last commit's oid and its checks from one GraphQL response, `None`
/// when anything about it cannot be trusted.
fn parse_last_commit_node(json: &str) -> Option<(String, Vec<GhCheck>)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v.get("errors").is_some_and(|e| !e.is_null()) {
        return None;
    }
    let commit = v.pointer("/data/repository/pullRequest/commits/nodes/0/commit")?;
    let oid = commit.get("oid")?.as_str().filter(|o| !o.is_empty())?;
    let contexts = commit.pointer("/statusCheckRollup/contexts");
    let Some(contexts) = contexts.filter(|c| !c.is_null()) else {
        // No rollup at all: the commit has no checks.
        return Some((oid.to_owned(), Vec::new()));
    };
    if contexts.pointer("/pageInfo/hasNextPage")?.as_bool()? {
        return None;
    }
    let nodes = contexts.get("nodes")?.as_array()?;
    let rollup = nodes
        .iter()
        .map(|n| serde_json::from_value::<GhCheck>(n.clone()))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some((oid.to_owned(), rollup))
}

/// The pull request's last commit and its checks, in one response. `None`
/// when the forge could not be asked.
async fn last_commit_node(repo: &Path, number: u64) -> Option<String> {
    let out = gh(
        repo,
        &[
            "api".to_owned(),
            "graphql".to_owned(),
            "-F".to_owned(),
            "owner={owner}".to_owned(),
            "-F".to_owned(),
            "repo={repo}".to_owned(),
            "-F".to_owned(),
            format!("number={number}"),
            "-f".to_owned(),
            "query=query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,\
             name:$repo){pullRequest(number:$number){commits(last:1){nodes{commit{oid \
             statusCheckRollup{contexts(first:100){pageInfo{hasNextPage} nodes{\
             ... on CheckRun{name status conclusion detailsUrl \
             isRequired(pullRequestNumber:$number)} \
             ... on StatusContext{context state targetUrl \
             isRequired(pullRequestNumber:$number)}}}}}}}}}}"
                .to_owned(),
        ],
    )
    .await
    .ok()?;
    out.0.then_some(out.1)
}

/// What a fix round did.
#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub enum Fixed {
    /// The fixer committed something, and this is the head that was pushed.
    Committed {
        /// The commit now at the tip of the pushed branch.
        head: String,
    },
    /// The fixer ran and chose to change nothing.
    Declined,
    /// The fixer could not run, or said nothing usable.
    Failed(String),
}

/// Hand the failures and the comments to the fixer, then commit and push.
///
/// The fixer works in the winner's own worktree so its commits land on the
/// branch the pull request is built from, and it runs with `allow_write` for
/// the same reason.
#[doc(hidden)]
pub async fn fix_round(
    state: &mut RunState,
    pr: &PrState,
    round: usize,
    budget: usize,
    reason: &str,
    logs: &str,
) -> Result<Fixed> {
    let winner = state
        .winner()
        .cloned()
        .context("landing needs a winning candidate; none is recorded on this run")?;
    let roles = state
        .config
        .resolve_roles()
        .context("resolve the roster for the fix round")?;
    // Same rule as the review loop: an explicitly configured fixer, otherwise
    // the winner's own author continuing its own conversation - the competition
    // is over, so its context is pure benefit.
    let (spec, seat_key): (AgentSpec, String) = match &roles.fixer {
        Some(f) if f.id != winner.agent => (f.clone(), "fix".to_owned()),
        _ => (
            state
                .config
                .agent(&winner.agent)
                .cloned()
                .unwrap_or_else(|_| roles.implementers[winner.index].clone()),
            format!("impl-{}", winner.label),
        ),
    };

    let prompt = fix_prompt(state, pr, round, budget, reason, logs);
    let mut seat = seat_of(state, &seat_key, &spec.id);
    let artifacts = agent::artifacts_dir(&state.dir());
    let prompt = if state.config.cache_dir().is_some() {
        format!("{prompt}\n\n{}", prompt::build_cache_note("fix", true))
    } else {
        prompt
    };
    // Read before the fixer runs: it normally commits for itself, so HEAD has
    // already moved by the time it returns and a later read would see no
    // progress (run 20261004-041622-5769 stopped on a fix that had landed).
    let before = git::rev_parse(&winner.worktree, "HEAD").await?;
    let out = agent::invoke(
        &spec,
        &mut seat,
        &Invocation {
            cwd: &winner.worktree,
            prompt: &prompt,
            timeout: Duration::from_secs(state.config.graph.timeout_fix),
            allow_write: true,
            sessions: state.config.graph.sessions,
            artifacts: &artifacts,
            stem: &format!("land-{round}"),
            run: &state.id,
            node: "land",
            cache_dir: state.config.cache_dir().as_deref(),
            attachments: &[],
            writable: &[],
        },
    )
    .await;
    state.seats.insert(seat.key.clone(), seat);

    match out {
        Ok(o) if o.quota_exhausted() => {
            return Ok(Fixed::Failed(
                "rate limited (quota); the fixer could not run".to_owned(),
            ));
        }
        Ok(o) if !o.usable() => {
            return Ok(Fixed::Failed(format!(
                "the fixer produced nothing usable (exit {:?}, timed out: {})",
                o.exit_code, o.timed_out
            )));
        }
        Ok(_) => {}
        Err(e) => return Ok(Fixed::Failed(format!("{e:#}"))),
    }

    // An agent that edited files but never committed would otherwise push
    // nothing and look like a refusal.
    if let Ok(r) = git::rescue_commit(
        &winner.worktree,
        &format!("magi: land round {round} fixes (uncommitted work)"),
    )
    .await
    {
        state.note_withheld("land", &r.withheld);
    }
    let after = git::rev_parse(&winner.worktree, "HEAD").await?;
    if after == before {
        return Ok(Fixed::Declined);
    }

    let remote = state.config.merge.remote.clone();
    let push = git::push(&winner.worktree, &remote, &winner.branch).await?;
    if !push.ok() {
        return Ok(Fixed::Failed(format!(
            "pushing {} to {remote} failed: {}",
            winner.branch, push.stderr
        )));
    }
    state.event(
        "land",
        format!("round {round}: pushed a fix to {}", winner.branch),
    );
    Ok(Fixed::Committed { head: after })
}

/// Fetch or create a seat, keeping its conversation across nodes.
pub(crate) fn seat_of(state: &mut RunState, key: &str, agent: &str) -> SeatState {
    if let Some(existing) = state.seats.get(key)
        && existing.agent == agent
    {
        return existing.clone();
    }
    let fresh = SeatState::new(key, agent, state.seed);
    state.seats.insert(key.to_owned(), fresh.clone());
    fresh
}

/// What the fixer is told.
fn fix_prompt(
    state: &RunState,
    pr: &PrState,
    round: usize,
    budget: usize,
    reason: &str,
    logs: &str,
) -> String {
    let mut s = format!(
        "Your patch is open as a pull request and it is not landing. Land round \
         {round} of {budget}.\n\n\
         Pull request: {}\n\n\
         What is holding it: {reason}\n\n\
         # The task\n\n{}\n",
        pr.url, state.instruction
    );

    if pr.failing.is_empty() {
        s.push_str("\n# Failing checks\n\n(none)\n");
    } else {
        let _ = write!(s, "\n# Failing checks\n\n- {}\n", pr.failing.join("\n- "));
        if logs.trim().is_empty() {
            s.push_str("\nNo log could be read; reproduce the failure locally.\n");
        } else {
            let _ = write!(s, "\n## Failing log tails\n\n{logs}\n");
        }
    }

    if pr.review_comments.is_empty() {
        s.push_str("\n# Review comments\n\n(none)\n");
    } else {
        s.push_str("\n# Review comments\n");
        for c in &pr.review_comments {
            let where_ = match (&c.path, c.line) {
                (Some(p), Some(l)) => format!(" ({p}:{l})"),
                (Some(p), None) => format!(" ({p})"),
                _ => String::new(),
            };
            let _ = write!(s, "\n## {}{where_}\n\n{}\n", c.author, c.body.trim());
        }
    }

    s.push_str(
        "\n# Rules\n\n\
         1. Fix the cause, never the symptom. Do not delete, skip, or weaken a \
            failing test; do not silence a lint with an allow attribute; do not \
            stretch a timeout to hide a race. If the check is right, the code is \
            wrong.\n\
         2. Change nothing the checks and the comments did not raise. A \
            drive-by refactor turns a one-line fix into a pull request that \
            needs reviewing again.\n\
         3. If a comment is wrong, say so with a checkable argument and change \
            nothing for it. A declined comment with a reason is a correct \
            outcome; a change made to appease a reviewer is not.\n\
         4. Commit in this worktree. magi pushes to the pull request's branch \
            for you; do not push, merge, or close anything yourself.\n\
         5. Never name yourself, your vendor, or your model, anywhere.\n\n\
         # Output\n\n\
         Say what you changed and why, and what you declined and why.",
    );

    let language = &state.config.graph.language;
    if !(language.trim().is_empty() || language.eq_ignore_ascii_case("en")) {
        let _ = write!(s, "\n\nWrite all prose in {language}.");
    }
    // After the language line, so the exception is the last word on it.
    s.push_str(&crate::prompt::github_english(language));
    if let Some(overlay) = state.config.prompts.overlay("fix") {
        let _ = write!(s, "\n\n{overlay}");
    }
    s
}

/// Failing log tails, the way the operator collects them by hand:
/// `gh run view --log-failed`.
async fn failing_logs(repo: &Path, failing: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, url) in failing.iter().take(MAX_LOGS) {
        let args = match (job_of(url), run_of(url)) {
            (Some(job), _) => vec![
                "run".to_owned(),
                "view".to_owned(),
                "--log-failed".to_owned(),
                "--job".to_owned(),
                job,
            ],
            (None, Some(run)) => vec![
                "run".to_owned(),
                "view".to_owned(),
                run,
                "--log-failed".to_owned(),
            ],
            // Not a GitHub Actions check - an external status has no log here.
            (None, None) => continue,
        };
        let (ok, body) = match gh(repo, &args).await {
            Ok(v) => v,
            Err(e) => (false, format!("{e:#}")),
        };
        if !ok && body.trim().is_empty() {
            continue;
        }
        let _ = write!(out, "### {name}\n\n```\n{}\n```\n\n", tail(&body, LOG_TAIL));
    }
    out
}

/// Job id out of a check's `detailsUrl`
/// (`https://github.com/o/r/actions/runs/<run>/job/<job>`).
fn job_of(details_url: &str) -> Option<String> {
    let after = details_url.split("/job/").nth(1)?;
    let id: String = after.chars().take_while(char::is_ascii_digit).collect();
    (!id.is_empty()).then_some(id)
}

/// Workflow run id out of a check's `detailsUrl`.
fn run_of(details_url: &str) -> Option<String> {
    let after = details_url.split("/actions/runs/").nth(1)?;
    let id: String = after.chars().take_while(char::is_ascii_digit).collect();
    (!id.is_empty()).then_some(id)
}

/// The comment `stop` posts. Fixed English, whatever `[graph] language` says:
/// it lands on GitHub, not in front of the operator. Pure so a test can hold
/// it to that.
fn stop_comment(run_id: &str, why: &str) -> String {
    format!(
        "{MARKER}\nmagi stopped landing this pull request: {why}\n\n\
         The branch is untouched and the run is `{run_id}`. Nothing was merged."
    )
}

/// Leave the pull request open, say why on it, and mark the run blocked.
///
/// The comment is what makes an unattended stop actionable: the operator wakes
/// up to a pull request that explains itself rather than to a silent queue.
async fn stop(state: &mut RunState, repo: &Path, pr: &PrState, why: &str) -> Result<()> {
    let body = stop_comment(&state.id, why);
    let posted = gh(
        repo,
        &[
            "pr".to_owned(),
            "comment".to_owned(),
            pr.number.to_string(),
            "--body".to_owned(),
            body,
        ],
    )
    .await;
    match posted {
        Ok((true, _)) => {}
        Ok((false, out)) => tracing::warn!("could not comment on {}: {out}", pr.url),
        Err(e) => tracing::warn!("could not comment on {}: {e:#}", pr.url),
    }
    state.status = RunStatus::Blocked;
    state.merge = Some(MergeOutcome {
        mode: MergeMode::Pr,
        ok: false,
        detail: why.to_owned(),
        empty: false,
    });
    state.event("land", format!("stopped: {why}"));
    state.save()?;
    Ok(())
}

/// Run `gh` in `repo`, returning success and the combined output.
///
/// Combined because `gh` reports a refused merge on stderr and the pull request
/// json on stdout, and both are evidence.
///
/// `GH_REPO` is stripped from the child's environment: every call site here
/// passes an explicit `cwd` (or a full pull request URL) meaning to operate
/// on *that* checkout's own remote, and `gh` prefers `GH_REPO` over the
/// checkout it is sitting in when no `--repo` flag is given. Left unset, a
/// `GH_REPO` the operator happens to have exported for an unrelated script
/// would silently redirect [`repo_slug`] (and every other cwd-scoped call
/// below) to a different repository than the one actually on disk - which
/// for the same-repo guard in [`correct_manual_merge`] would mean the check
/// could be made to agree with whatever repository a forged `--merged` URL
/// claims, defeating it entirely.
async fn gh(cwd: &Path, args: &[String]) -> Result<(bool, String)> {
    let out = tokio::process::Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .env_remove("GH_REPO")
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("spawn gh {}", args.join(" ")))?;
    let mut body = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if body.trim().is_empty() {
        body = err.into_owned();
    } else if !err.trim().is_empty() {
        body.push_str(&err);
    }
    Ok((out.status.success(), body.trim().to_owned()))
}

/// Verdict of one entry in the status rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    Pending,
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    #[serde(default)]
    url: String,
    #[serde(default)]
    number: u64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status_check_rollup: Vec<GhCheck>,
    /// GitHub's own verdict on whether the pull request can be merged.
    ///
    /// Worth asking for because it is the only place the *required* check set
    /// is applied: the rollup lists every check equally, so a repository that
    /// deliberately does not require `coverage` still looks red here. See
    /// [`Blocking`].
    #[serde(default)]
    merge_state_status: String,
    /// The commit the pull request currently points at. Compared with the
    /// commit a fix round pushed, it is how the loop knows the forge has moved
    /// on and the rollup belongs to the new head.
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    reviews: Vec<GhReview>,
    #[serde(default)]
    comments: Vec<GhComment>,
}

/// One rollup entry. `gh` mixes two GraphQL types in this array: a `CheckRun`
/// has `name`/`status`/`conclusion`, while a `StatusContext` - the old commit
/// status API, which is how CodeRabbit reports - has `context`/`state` and no
/// conclusion at all.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhCheck {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    details_url: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
    /// Whether the base branch requires this check. Only the GraphQL node
    /// carries it; `None` is "not read", never "not required".
    #[serde(default)]
    is_required: Option<bool>,
}

impl GhCheck {
    /// Name to show a human and hand to the fixer.
    fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.context.clone())
            .unwrap_or_else(|| "(unnamed check)".to_owned())
    }

    /// Where this check's logs live, when it has any.
    fn url(&self) -> Option<&str> {
        self.details_url
            .as_deref()
            .or(self.target_url.as_deref())
            .filter(|u| !u.is_empty())
    }

    /// Did it pass?
    ///
    /// `SKIPPED` and `NEUTRAL` count as passed: the Claude review workflow
    /// skips release and bot pull requests by design, and a skip that blocked
    /// landing would block exactly the pull requests that need no review.
    /// `CANCELLED` counts as failed - a cancelled check did not pass, and
    /// merging over one is merging over a check that never ran.
    fn verdict(&self) -> Verdict {
        if let Some(status) = self.status.as_deref() {
            if !status.eq_ignore_ascii_case("COMPLETED") {
                return Verdict::Pending;
            }
        }
        let outcome = self
            .conclusion
            .as_deref()
            .or(self.state.as_deref())
            .unwrap_or("");
        match outcome.to_ascii_uppercase().as_str() {
            "SUCCESS" | "SKIPPED" | "NEUTRAL" => Verdict::Pass,
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "STARTUP_FAILURE"
            | "ACTION_REQUIRED" => Verdict::Fail,
            "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" => {
                Verdict::Pending
            }
            _ => Verdict::Unknown,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GhAuthor {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhReview {
    #[serde(default)]
    author: GhAuthor,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct GhComment {
    #[serde(default)]
    author: GhAuthor,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct GhUser {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhInline {
    #[serde(default)]
    user: GhUser,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    body: String,
}

impl Default for GhAuthor {
    fn default() -> Self {
        Self {
            login: "(unknown)".to_owned(),
        }
    }
}

impl Default for GhUser {
    fn default() -> Self {
        Self {
            login: "(unknown)".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Candidate, ReviewRecord, ReviewRound, Tally};

    fn head_json(head: &str, base: &str, state: &str, cross: bool) -> String {
        format!(
            r#"{{"headRefName":"{head}","headRefOid":"aaa","baseRefName":"{base}","state":"{state}","isCrossRepository":{cross}}}"#
        )
    }

    #[test]
    fn a_pull_request_is_closed_only_when_its_head_is_exactly_the_runs_branch() {
        let ok = head_json("magi/27b2/A", "main", "OPEN", false);
        assert_eq!(
            closable(&ok, "magi/27b2/A", "main", &["aaa".to_owned()]),
            Ok(())
        );
        for (json, why) in [
            (head_json("magi/27b2/B", "main", "OPEN", false), "head"),
            (head_json("magi/27b2/A-2", "main", "OPEN", false), "head"),
            (head_json("magi/27b2/A", "main", "OPEN", true), "fork"),
            (head_json("magi/27b2/A", "dev", "OPEN", false), "targets"),
            (head_json("magi/27b2/A", "main", "MERGED", false), "already"),
            (head_json("magi/27b2/A", "main", "CLOSED", false), "already"),
        ] {
            let err = closable(&json, "magi/27b2/A", "main", &["aaa".to_owned()])
                .unwrap_err()
                .why;
            assert!(err.contains(why), "{json}: {err}");
        }
        // A head that moved on past what was verified is left alone.
        let moved = head_json("magi/27b2/A", "main", "OPEN", false);
        let err = closable(&moved, "magi/27b2/A", "main", &["bbb".to_owned()]).unwrap_err();
        assert!(err.retry && err.why.contains("not a commit"), "{err:?}");
        assert!(
            !closable(
                &head_json("x", "main", "OPEN", false),
                "magi/27b2/A",
                "main",
                &[]
            )
            .unwrap_err()
            .retry
        );
        assert!(is_forge_url("https://github.com/o/r.git"));
        assert!(is_forge_url("git@github.com:o/r.git"));
        assert!(!is_forge_url("/tmp/origin.git"));
        assert!(!is_forge_url("C:\\work\\origin.git"));
        assert!(!is_forge_url("file:///tmp/origin.git"));
        assert!(forge_unavailable(
            "gh pr list failed: none of the git remotes configured for this repository point to a known GitHub host."
        ));
        assert!(!forge_unavailable(
            "gh pr list failed: error connecting to api.github.com"
        ));
        assert!(closable("not json", "magi/27b2/A", "main", &[]).is_err());
        // A record that does not say whether it is a fork is not trusted.
        assert!(
            closable(
                r#"{"headRefName":"b","headRefOid":"aaa","baseRefName":"main","state":"OPEN"}"#,
                "b",
                "main",
                &["aaa".to_owned()]
            )
            .is_err()
        );
    }

    #[test]
    fn the_close_comment_names_the_commit_on_the_base() {
        let e = crate::already::Evidence {
            proof: crate::already::Proof::PatchId,
            tip: "1234567890".to_owned(),
            commits: vec!["0e368de0000".to_owned()],
        };
        let c = superseded_comment("main", &e);
        assert!(c.contains("0e368de") && c.contains("`main`"), "{c}");
    }

    /// Real `gh pr view` output for the open pull request #10 (Renovate's apm bump), trimmed to four checks and its one comment. Every check passed or was skipped by the review workflow, and the only comment is CodeRabbit's trigger notice.
    const GREEN_OPEN: &str = r####"{
  "url": "https://github.com/yukimemi/magi/pull/10",
  "number": 10,
  "state": "OPEN",
  "mergeStateStatus": "CLEAN",
  "statusCheckRollup": [
    {
      "__typename": "CheckRun",
      "conclusion": "SKIPPED",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33356278334/job/99378963755",
      "name": "review",
      "status": "COMPLETED",
      "workflowName": "claude-review"
    },
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33356278338/job/99378963144",
      "name": "check (ubuntu-latest)",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33356278338/job/99378963095",
      "name": "rustfmt",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "StatusContext",
      "context": "CodeRabbit",
      "state": "SUCCESS",
      "targetUrl": ""
    }
  ],
  "reviews": [],
  "comments": [
    {
      "author": {
        "login": "coderabbitai"
      },
      "authorAssociation": "NONE",
      "body": "<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- This is an auto-generated comment: skip review by coderabbit.ai -->\n\n> [!IMPORTANT]\n> - [ ] <!-- {\"checkboxId\":\"e9bb8d72-00e8-4f67-9cb2-caf3b22574fe\"} --> 🔍 Trigger review\n> \n> This repository does not receive automatic reviews because it has fewer than 10 stars.\n> \n> <details>\n> <summary>⚙️ Run configuration</summary>\n> \n> **Configuration used**: defaults\n> \n> **Review profile**: CHILL\n> \n> **Plan**: Pro Plus\n> \n> **Run ID**: `78e70bf3-c5a0-4269-a96c-2afb2dba7eff`\n> \n> </details>\n\n<!-- end of auto-generated comment: skip review by coderabbit.ai -->\n\n<!-- tips_start -->\n\n---\n\nThanks for using [CodeRabbit](https://coderabbit.ai?utm_source=oss&utm_medium=github&utm_campaign=yukimemi/magi&utm_content=10)! It's free for OSS, and your support helps us grow. If you like it, consider giving us a shout-out.\n\n<details>\n<summary>❤️ Share</summary>\n\n- [X](https://twitter.com/intent/tweet?text=I%20just%20used%20%40coderabbitai%20for%20my%20code%20review%2C%20and%20it%27s%20fantastic%21%20It%27s%20free%20for%20OSS%20and%2"
    }
  ]
}"####;

    /// Real output for the open pull request #9 (the daily kata-apply), whose `editorconfig` check failed while everything else passed.
    const RED_OPEN: &str = r####"{
  "url": "https://github.com/yukimemi/magi/pull/9",
  "number": 9,
  "state": "OPEN",
  "mergeStateStatus": "UNSTABLE",
  "statusCheckRollup": [
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33587406996/job/100114323744",
      "name": "check (ubuntu-latest)",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33587406996/job/100114323811",
      "name": "rustfmt",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "CheckRun",
      "conclusion": "FAILURE",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33587406996/job/100114323572",
      "name": "editorconfig",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "StatusContext",
      "context": "CodeRabbit",
      "state": "SUCCESS",
      "targetUrl": ""
    }
  ],
  "reviews": [],
  "comments": [
    {
      "author": {
        "login": "coderabbitai"
      },
      "authorAssociation": "NONE",
      "body": "<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- This is an auto-generated comment: skip review by coderabbit.ai -->\n\n> [!IMPORTANT]\n> - [ ] <!-- {\"checkboxId\":\"e9bb8d72-00e8-4f67-9cb2-caf3b22574fe\"} --> 🔍 Trigger review\n> \n> This repository does not receive automatic reviews because it has fewer than 10 stars.\n> \n> <details>\n> <summary>⚙️ Run configuration</summary>\n> \n> **Configuration used**: defaults\n> \n> **Review profile**: CHILL\n> \n> **Plan**: Team\n> \n> **Run ID**: `91e0dc24-6040-4c3d-92c6-f7d2b542523d`\n> \n> </details>\n\n<!-- end of auto-generated comment: skip review by coderabbit.ai -->\n\n<!-- tips_start -->\n\n---\n\nThanks for using [CodeRabbit](https://coderab"
    }
  ]
}"####;

    /// Pull request #9's real payload with its `editorconfig` check rewound to the `IN_PROGRESS` / `conclusion: null` pair `gh` reports while a job is still in flight.
    const PENDING_OPEN: &str = r####"{
  "url": "https://github.com/yukimemi/magi/pull/9",
  "number": 9,
  "state": "OPEN",
  "statusCheckRollup": [
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33587406996/job/100114323744",
      "name": "check (ubuntu-latest)",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33587406996/job/100114323811",
      "name": "rustfmt",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "CheckRun",
      "conclusion": null,
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33587406996/job/100114323572",
      "name": "editorconfig",
      "status": "IN_PROGRESS",
      "workflowName": "CI"
    },
    {
      "__typename": "StatusContext",
      "context": "CodeRabbit",
      "state": "SUCCESS",
      "targetUrl": ""
    }
  ],
  "reviews": [],
  "comments": []
}"####;

    /// Real output for pull request #16 after it was merged - the shape landing sees when a person merged underneath it.
    const MERGED: &str = r####"{
  "url": "https://github.com/yukimemi/magi/pull/16",
  "number": 16,
  "state": "MERGED",
  "statusCheckRollup": [
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33636587933/job/100268878095",
      "name": "check (ubuntu-latest)",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33636587918/job/100268876427",
      "name": "review",
      "status": "COMPLETED",
      "workflowName": "claude-review"
    }
  ],
  "reviews": [],
  "comments": []
}"####;

    /// Pull request #12's real payload - a green pull request carrying CodeRabbit's walkthrough and a Claude review that found a real bug - rewound to the `OPEN` state it was in when that review was posted.
    const REVIEWED_OPEN: &str = r####"{
  "url": "https://github.com/yukimemi/magi/pull/12",
  "number": 12,
  "state": "OPEN",
  "statusCheckRollup": [
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33571212506/job/100065355258",
      "name": "check (ubuntu-latest)",
      "status": "COMPLETED",
      "workflowName": "CI"
    },
    {
      "__typename": "CheckRun",
      "conclusion": "SUCCESS",
      "detailsUrl": "https://github.com/yukimemi/magi/actions/runs/33571212566/job/100065355810",
      "name": "review",
      "status": "COMPLETED",
      "workflowName": "claude-review"
    }
  ],
  "reviews": [
    {
      "author": {
        "login": "claude"
      },
      "state": "COMMENTED",
      "body": ""
    }
  ],
  "comments": [
    {
      "author": {
        "login": "coderabbitai"
      },
      "authorAssociation": "NONE",
      "body": "<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- This is an auto-generated comment: skip review by coderabbit.ai -->\n\n> [!IMPORTANT]\n> - [ ] <!-- {\"checkboxId\":\"e9bb8d72-00e8-4f67-9cb2-caf3b22574fe\"} --> 🔍 Trigger review\n> \n> This repository does not receive automatic reviews because it has fewer than 10 stars.\n> \n> <details>\n> <summary>⚙️ Run configuration</summary>\n> \n> **Configuration used**: defaults\n> \n> **Review profile**: CHILL\n> \n> **Plan**: Team\n> \n> **Run ID**: `72058bf3-b7df-41d9-8e4d-a06a31be4a26`\n> \n> </details>\n\n<!-- end of auto-generated comment: skip review by coderabbit.ai -->\n\n<!-- tips_start -->\n\n---\n\nThanks for using [CodeRabbit](https://coderabbit.ai?utm_source=oss&utm_medium=github&utm_campaign=yukimemi/magi&utm_content=12)! It's free for OSS, and your support helps us grow. If you like it, consider giving us a shout-out.\n\n<details>\n<summa"
    },
    {
      "author": {
        "login": "claude"
      },
      "authorAssociation": "NONE",
      "body": "**Claude finished @yukimemi's task in 3m 52s** —— [View job](https://github.com/yukimemi/magi/actions/runs/33571212566)\n\n---\n### Review: `magi review <branch>` — cheap-half-only graph\n\nRead through `src/graph.rs`, `src/main.rs`, `src/prompt.rs`, and the new/edited tests, and traced the claimed degeneration (`prep` short-circuits on a non-empty candidate list, `implement` skips because `commits != 0`, `judge`/`vote` skip on `viable().len() == 1`, `tally` skips because it's pre-set, `fold_losers` has no losers) against the actual code — it holds up. CI (`cargo make check`) is green on this PR.\n\n**Correctness**\n\n- One real bug, flagged inline on `src/graph.rs:1255`: the fixer-agent fallback (`self.roles.implementers[winner.index].clone()`) is unreachable in the normal graph (a real candidate's `winner.agent` always resolves via `config.agent(...)`), but a review-only run's `winner.agent` is always the `\"(existing branch)\"` sentinel, so this fallback now runs on *every* review-only fix that has no dedicated `[roles] fixer`. `graph.candidates` has no lower-bound validation, so a `magi.toml` tuned for review-only use (`candidates = 0`, plausible given this PR's own cost rationale) would panic with an out-of-bounds index the first time a"
    }
  ]
}"####;

    /// Real `gh api repos/{owner}/{repo}/pulls/12/comments` output: one inline finding with its file and line.
    const INLINE: &str = r####"[
  {
    "user": {
      "login": "claude[bot]"
    },
    "path": "src/graph.rs",
    "line": 231,
    "body": "Minor edge case: unlike `implement()` (which sets `c.empty = commits == 0 || patch.trim().is_empty()`, `src/graph.rs:472`), the seeded review-only candidate always sets `empty: false` once `commits > 0` is confirmed, without checking whether the diff itself is actually empty (e.g. a commit immediately followed by a revert nets zero file changes). Such a branch would pass `Runner::review`'s validation and proceed into a review round with an empty patch, where `implement()`'s equivalent path would"
  }
]"####;

    /// CodeRabbit's real trigger notice: a checkbox, a `<details>` block, and its own "skip review" marker.
    const CODERABBIT_TRIGGER: &str = r####"<!-- This is an auto-generated comment: summarize by coderabbit.ai -->
<!-- This is an auto-generated comment: skip review by coderabbit.ai -->

> [!IMPORTANT]
> - [ ] <!-- {"checkboxId":"e9bb8d72-00e8-4f67-9cb2-caf3b22574fe"} --> 🔍 Trigger review
> 
> This repository does not receive automatic reviews because it has fewer than 10 stars.
> 
> <details>
> <summary>⚙️ Run configuration</summary>
> 
> **Configuration used**: defaults
> 
> **Review profile**: CHILL
> 
> **Plan**: Team
> 
> **Run ID**: `c1e2a68f-87fc-4b35-9ec4-e75c7854966a`
> 
> </details>

<!-- end of auto-generated comment: skip review by coderabbit.ai -->

<!-- tips_start -->

---

Thanks for using [CodeRabbit](https://coderabbit.ai?utm_source=oss&utm_medium=github&utm_campaign=yukimemi/magi&utm_content=16)! It's free for OSS, and your support helps us grow. If you like it, consider giving us a shout-out.

<details>
<summary>❤️ Share</summary>

- [X](https://twitter.com/intent/tweet?text=I%20just%20used%20%40coderabbitai%20for%20my%20code%20review%2C%20and%20it%27s%20fantastic%21%20It%27s%20free%20for%20OSS%20and%20off"####;

    /// The Claude review job's real comment while it is still working: a heading and a task list, and nothing that asks for a change.
    const CLAUDE_CHECKLIST: &str = r####"**Claude finished @yukimemi's task in 4m 14s** —— [View job](https://github.com/yukimemi/magi/actions/runs/33636587918)

---
### Reviewing PR #16

- [x] Read AGENTS.md conventions
- [x] Review `src/daemon.rs` changes
- [x] Review `src/main.rs` changes (new `doctor` reporting)
- [x] Review `src/web.rs` changes (reuse of unreadable-run count)
- [x] Check test coverage for new behavior
- [x] Run verification commands (blocked — see note)
- [x] Post findings"####;

    /// The same job's real comment on pull request #12 once it had something to say.
    const CLAUDE_FINDING: &str = r####"**Claude finished @yukimemi's task in 3m 52s** —— [View job](https://github.com/yukimemi/magi/actions/runs/33571212566)

---
### Review: `magi review <branch>` — cheap-half-only graph

Read through `src/graph.rs`, `src/main.rs`, `src/prompt.rs`, and the new/edited tests, and traced the claimed degeneration (`prep` short-circuits on a non-empty candidate list, `implement` skips because `commits != 0`, `judge`/`vote` skip on `viable().len() == 1`, `tally` skips because it's pre-set, `fold_losers` has no losers) against the actual code — it holds up. CI (`cargo make check`) is green on this PR.

**Correctness**

- One real bug, flagged inline on `src/graph.rs:1255`: the fixer-agent fallback (`self.roles.implementers[winner.index].clone()`) is unreachable in the normal graph (a real candidate's `winner.agent` always resolves via `config.agent(...)`), but a review-only run's `winner.agent` is always the `"(existing branch)"` sentinel, so this fallback now runs on *every* review-only fix that has no dedicated `[roles] fixer`. `graph.candidates` has no lower-bound validation, so a `magi.toml` tuned for review-only use (`candidates = 0`, plausible given this PR's own cost rationale) would panic with an out-of-bounds index the first time a"####;

    fn pr(checks: Checks, failing: &[&str], comments: usize) -> PrState {
        PrState {
            url: "https://github.com/yukimemi/magi/pull/16".to_owned(),
            number: 16,
            state: PrLifecycle::Open,
            checks,
            // These tests are about red-means-fix, so a red here is one the
            // forge gates on. Without saying so they would assert the new
            // "merge past a check nobody requires" path by accident.
            blocking: if matches!(checks, Checks::Red) {
                Blocking::Yes
            } else {
                Blocking::No
            },
            failing: failing.iter().map(|s| (*s).to_owned()).collect(),
            review_comments: (0..comments)
                .map(|i| ReviewComment {
                    author: "coderabbitai".to_owned(),
                    path: Some("src/graph.rs".to_owned()),
                    line: Some(231),
                    body: format!("finding {i}"),
                })
                .collect(),
        }
    }

    #[test]
    fn a_green_pull_request_with_nothing_outstanding_parses_as_ready_to_merge() {
        let state = parse_pr(GREEN_OPEN).expect("green fixture parses");
        assert_eq!(state.number, 10);
        assert_eq!(state.state, PrLifecycle::Open);
        assert_eq!(state.checks, Checks::Green);
        assert!(state.failing.is_empty());
        assert!(
            state.review_comments.is_empty(),
            "the only comment is CodeRabbit's trigger notice: {:?}",
            state.review_comments
        );
        assert_eq!(decide(&state, 0, 4, Duration::ZERO), Step::Merge);
    }

    #[test]
    fn a_failing_check_parses_as_red_and_is_named() {
        let state = parse_pr(RED_OPEN).expect("red fixture parses");
        assert_eq!(state.checks, Checks::Red);
        assert_eq!(state.failing, vec!["editorconfig".to_owned()]);
        // The captured payload says `UNSTABLE` - mergeable, with a check
        // nobody requires red - which is exactly the shape that had to be
        // merged by hand. Asserted separately, in
        // `a_red_check_nobody_requires_does_not_buy_a_fix_round`. What this
        // test is about is that a red check is *named*, so the reason a fixer
        // is handed says which one; so it asks the blocking question here.
        let mut blocking = state.clone();
        blocking.blocking = Blocking::Yes;
        match decide(&blocking, 0, 4, Duration::ZERO) {
            Step::Fix { reason } => {
                assert!(reason.contains("editorconfig"), "reason: {reason}");
                assert!(reason.contains("failing"), "reason: {reason}");
            }
            other => panic!("expected a fix round, got {other:?}"),
        }
    }

    #[test]
    fn a_check_still_running_parses_as_pending_and_is_waited_for() {
        let state = parse_pr(PENDING_OPEN).expect("pending fixture parses");
        assert_eq!(state.checks, Checks::Pending);
        assert_eq!(decide(&state, 0, 4, Duration::ZERO), Step::Wait);
    }

    #[test]
    fn a_pull_request_merged_underneath_us_is_done_rather_than_a_failure() {
        let state = parse_pr(MERGED).expect("merged fixture parses");
        assert_eq!(state.state, PrLifecycle::Merged);
        assert_eq!(
            decide(&state, 0, 4, Duration::ZERO),
            Step::Done { merged: true }
        );
    }

    #[test]
    fn a_review_that_found_something_is_outstanding_and_holds_the_merge() {
        let state = parse_pr(REVIEWED_OPEN).expect("reviewed fixture parses");
        assert_eq!(state.checks, Checks::Green);
        let authors: Vec<&str> = state
            .review_comments
            .iter()
            .map(|c| c.author.as_str())
            .collect();
        assert_eq!(
            authors,
            vec!["claude"],
            "CodeRabbit's walkthrough is machinery; Claude's review is a finding"
        );
        match decide(&state, 0, 4, Duration::ZERO) {
            Step::Fix { reason } => assert!(reason.contains("unresolved"), "reason: {reason}"),
            other => panic!("expected a fix round, got {other:?}"),
        }
    }

    #[test]
    fn inline_review_comments_keep_their_file_and_line() {
        let comments = parse_inline_comments(INLINE).expect("inline fixture parses");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "claude[bot]");
        assert_eq!(comments[0].path.as_deref(), Some("src/graph.rs"));
        assert_eq!(comments[0].line, Some(231));
        assert!(comments[0].body.contains("empty"), "{}", comments[0].body);
    }

    #[test]
    fn a_status_only_bot_comment_does_not_trigger_a_fix_round() {
        assert!(
            is_noise(CODERABBIT_TRIGGER),
            "CodeRabbit's trigger notice declares itself not a review"
        );
        assert!(
            is_noise(CLAUDE_CHECKLIST),
            "a progress checklist asks for nothing"
        );
        assert!(
            !is_noise(CLAUDE_FINDING),
            "a review that names a bug is input, not noise"
        );

        let mut clean = pr(Checks::Green, &[], 0);
        clean.review_comments.push(ReviewComment {
            author: "coderabbitai".to_owned(),
            path: None,
            line: None,
            body: CODERABBIT_TRIGGER.to_owned(),
        });
        clean.review_comments.retain(|c| !is_noise(&c.body));
        assert_eq!(decide(&clean, 0, 4, Duration::ZERO), Step::Merge);

        let mut found = pr(Checks::Green, &[], 0);
        found.review_comments.push(ReviewComment {
            author: "claude".to_owned(),
            path: None,
            line: None,
            body: CLAUDE_FINDING.to_owned(),
        });
        found.review_comments.retain(|c| !is_noise(&c.body));
        assert!(matches!(
            decide(&found, 0, 4, Duration::ZERO),
            Step::Fix { .. }
        ));
    }

    #[test]
    fn the_policy_table_holds_for_every_combination_that_matters() {
        let cases: Vec<(&str, PrState, usize, usize, Duration, Step)> = vec![
            (
                "pending checks are waited for, even on the last round",
                pr(Checks::Pending, &[], 0),
                4,
                4,
                Duration::ZERO,
                Step::Wait,
            ),
            (
                "red checks are fixed",
                pr(Checks::Red, &["editorconfig"], 0),
                0,
                4,
                Duration::ZERO,
                Step::Fix {
                    reason: "1 check(s) failing: editorconfig".to_owned(),
                },
            ),
            (
                "green with comments is fixed, not merged",
                pr(Checks::Green, &[], 2),
                1,
                4,
                Duration::ZERO,
                Step::Fix {
                    reason: "checks are green but 2 review comment(s) are unresolved: coderabbitai"
                        .to_owned(),
                },
            ),
            (
                "green and clean merges",
                pr(Checks::Green, &[], 0),
                3,
                4,
                Duration::ZERO,
                Step::Merge,
            ),
            (
                "an unreadable rollup is waited on while the grace lasts",
                pr(Checks::Unknown, &[], 0),
                0,
                4,
                Duration::ZERO,
                Step::Wait,
            ),
            (
                "an unreadable rollup is never merged once the grace is spent",
                pr(Checks::Unknown, &[], 0),
                0,
                4,
                CHECKS_GRACE,
                Step::GiveUp {
                    reason: "no check status is readable on the pull request after 3 minute(s); \
                             refusing to merge on a guess"
                        .to_owned(),
                },
            ),
        ];
        for (what, state, round, budget, waited, want) in cases {
            assert_eq!(decide(&state, round, budget, waited), want, "{what}");
        }
    }

    #[test]
    fn the_forge_verdict_survives_the_round_trip_from_gh() {
        // Read off `gh pr view --json ...,mergeStateStatus`, because a field
        // requested but never parsed is the kind of thing that looks wired up
        // and answers `Unsaid` forever.
        let green = parse_pr(GREEN_OPEN).expect("parse");
        assert_eq!(green.blocking, Blocking::No);
        let red = parse_pr(RED_OPEN).expect("parse");
        assert_eq!(
            red.blocking,
            Blocking::No,
            "`UNSTABLE` is mergeable: the red check is one nobody requires"
        );
        assert_eq!(red.checks, Checks::Red, "and it is still reported as red");
        // A payload from an older `gh` has no such field at all.
        let quiet =
            parse_pr(&GREEN_OPEN.replace("\"mergeStateStatus\": \"CLEAN\",", "")).expect("parse");
        assert_eq!(quiet.blocking, Blocking::Unsaid);
    }

    #[test]
    fn a_red_check_nobody_requires_does_not_buy_a_fix_round() {
        // Pull request 37's only red check was `editorconfig`, failing
        // because the action could not fetch its own binary after
        // editorconfig-checker v4 renamed its release assets. The repository
        // does not require it. magi answered by asking a fixer to repair a
        // change that was fine, and the pull request had to be merged by hand.
        let mut nonblocking = pr(Checks::Red, &["editorconfig", "coverage"], 0);
        nonblocking.blocking = Blocking::No;
        assert_eq!(
            decide(&nonblocking, 0, 4, Duration::ZERO),
            Step::Merge,
            "the forge says nothing is in the way, so nothing is"
        );

        // The same red, gated on: that is a fix round, as before.
        let mut blocking = pr(Checks::Red, &["test (ubuntu-latest)"], 0);
        blocking.blocking = Blocking::Yes;
        assert!(matches!(
            decide(&blocking, 0, 4, Duration::ZERO),
            Step::Fix { .. }
        ));

        // A review comment still outranks green-enough: a non-required red
        // must not become a way to merge past an unanswered reviewer.
        let mut commented = pr(Checks::Red, &["coverage"], 1);
        commented.blocking = Blocking::No;
        assert!(matches!(
            decide(&commented, 0, 4, Duration::ZERO),
            Step::Fix { .. }
        ));

        // And silence from the forge is not consent.
        let mut unsaid = pr(Checks::Red, &["coverage"], 0);
        unsaid.blocking = Blocking::Unsaid;
        assert!(matches!(
            decide(&unsaid, 0, 4, Duration::ZERO),
            Step::Fix { .. }
        ));
    }

    #[test]
    fn a_red_merge_is_announced_with_every_failing_check_and_a_green_one_is_not() {
        let mut red = pr(Checks::Red, &["test (windows-latest)", "coverage"], 0);
        red.blocking = Blocking::No;
        assert_eq!(
            decide(&red, 0, 4, Duration::ZERO),
            Step::Merge,
            "announcing must not change the decision"
        );
        let said = red_merge_summary("yukimemi/magi", &red).expect("red merge is announced");
        assert!(said.contains("yukimemi/magi"), "{said}");
        assert!(said.contains("#16"), "{said}");
        assert!(
            said.contains("https://github.com/yukimemi/magi/pull/16"),
            "{said}"
        );
        assert!(
            said.contains("test (windows-latest)") && said.contains("coverage"),
            "{said}"
        );

        // `failing` can be left over on a green observation; only `checks` counts.
        let green = pr(Checks::Green, &["stale"], 0);
        assert_eq!(red_merge_summary("yukimemi/magi", &green), None);
    }

    #[test]
    fn the_repo_label_comes_from_the_pull_request_url() {
        let p = Path::new("/tmp/checkout");
        assert_eq!(
            repo_label(p, "https://github.com/yukimemi/magi/pull/16"),
            "yukimemi/magi"
        );
        assert_eq!(repo_label(p, "not a url"), "checkout");
    }

    #[test]
    fn a_branch_the_base_moved_under_is_rebased_not_fixed() {
        // Pull requests 35 and 37 were both rebased by hand: a competition
        // that runs for two hours against a repository merging pull requests
        // all day conflicts on the way in, and that is arithmetic rather
        // than a defect in the change.
        let mut conflicted = pr(Checks::Green, &[], 0);
        conflicted.blocking = Blocking::Conflict;
        assert_eq!(decide(&conflicted, 0, 4, Duration::ZERO), Step::Rebase);

        // Decided before the checks, and even with the rounds spent: every
        // check on a branch that cannot land is an answer about a state that
        // cannot land, and a conflict is not the change's fault.
        let mut red = pr(Checks::Red, &["test (ubuntu-latest)"], 2);
        red.blocking = Blocking::Conflict;
        assert_eq!(decide(&red, 4, 4, Duration::ZERO), Step::Rebase);

        // The lifecycle still wins over everything, conflict included.
        let mut merged = pr(Checks::Red, &[], 0);
        merged.blocking = Blocking::Conflict;
        merged.state = PrLifecycle::Merged;
        assert_eq!(
            decide(&merged, 0, 4, Duration::ZERO),
            Step::Done { merged: true }
        );
    }

    #[test]
    fn the_forge_verdict_is_read_off_merge_state_status() {
        // The spellings that mean "mergeable". `UNSTABLE` is the one that
        // matters: mergeable, with a non-required check red or still running.
        for ok in ["CLEAN", "UNSTABLE", "unstable", "HAS_HOOKS"] {
            assert_eq!(Blocking::of(ok), Blocking::No, "{ok}");
            assert!(!Blocking::of(ok).stops_a_merge(), "{ok}");
        }
        assert_eq!(Blocking::of("DIRTY"), Blocking::Conflict);
        assert_eq!(Blocking::of("BLOCKED"), Blocking::Yes);
        assert_eq!(Blocking::of("BEHIND"), Blocking::Yes);
        // An older `gh`, or a token without the scope, says nothing - and
        // refusing to guess is the rule everywhere else in this module.
        for quiet in ["", "UNKNOWN"] {
            assert_eq!(Blocking::of(quiet), Blocking::Unsaid);
            assert!(Blocking::of(quiet).stops_a_merge());
        }
    }

    #[test]
    fn a_merge_command_that_failed_after_merging_is_still_a_merge() {
        let argv = merge_argv(28, "fix: retry uploads on transient network errors");
        // The exact stderr from run ec12, in a jj-colocated repository.
        let jj = "could not determine current branch: failed to run git: not on any branch";

        let landed = merged_after_all(&argv, jj, Some(PrLifecycle::Merged))
            .expect("the forge says merged, so it merged");
        assert!(landed.ok);
        assert!(
            landed.detail.contains("but the pull request is merged"),
            "the record must not read as a clean success: {}",
            landed.detail
        );
        assert!(
            landed.detail.contains("not on any branch"),
            "and it must keep what the command actually said: {}",
            landed.detail
        );

        // A pull request still open means the merge really failed.
        assert!(merged_after_all(&argv, jj, Some(PrLifecycle::Open)).is_none());
        assert!(merged_after_all(&argv, jj, Some(PrLifecycle::Closed)).is_none());
        // And an unreadable answer is not evidence of success.
        assert!(merged_after_all(&argv, jj, None).is_none());
    }

    #[test]
    fn a_pull_request_closed_underneath_us_is_done_and_not_merged() {
        let mut state = pr(Checks::Red, &["editorconfig"], 3);
        state.state = PrLifecycle::Closed;
        assert_eq!(
            decide(&state, 0, 4, Duration::ZERO),
            Step::Done { merged: false },
            "a human closing the pull request ends the loop, whatever CI says"
        );
    }

    #[test]
    fn the_last_round_gives_up_with_a_reason_naming_what_is_still_failing() {
        let red = decide(
            &pr(Checks::Red, &["editorconfig", "test (macos)"], 0),
            4,
            4,
            Duration::ZERO,
        );
        match red {
            Step::GiveUp { reason } => {
                assert!(reason.contains("editorconfig"), "reason: {reason}");
                assert!(reason.contains("test (macos)"), "reason: {reason}");
                assert!(reason.contains("4 fix round(s)"), "reason: {reason}");
            }
            other => panic!("expected a give-up, got {other:?}"),
        }

        let commented = decide(&pr(Checks::Green, &[], 1), 2, 2, Duration::ZERO);
        match commented {
            Step::GiveUp { reason } => {
                assert!(reason.contains("unresolved"), "reason: {reason}");
                assert!(reason.contains("2 fix round(s)"), "reason: {reason}");
            }
            other => panic!("expected a give-up, got {other:?}"),
        }
    }

    #[test]
    fn the_merge_command_squashes_deletes_the_branch_and_sets_its_own_subject() {
        let candidate_commit = "magi: candidate A (uncommitted work)";
        let subject = merge_subject(candidate_commit, "add retries to the uploader");
        let argv = merge_argv(16, &subject);

        assert!(argv.contains(&"--squash".to_owned()));
        assert!(argv.contains(&"--delete-branch".to_owned()));
        assert!(argv.contains(&"--subject".to_owned()));
        assert_eq!(
            argv.last().map(String::as_str),
            Some("add retries to the uploader"),
            "the subject must not be the candidate commit message"
        );
        assert_ne!(subject, candidate_commit);
    }

    #[test]
    fn a_real_pull_request_title_is_used_as_the_squash_subject_verbatim() {
        assert_eq!(
            merge_subject("feat: a queue, an unattended loop, and a phone UI", "task"),
            "feat: a queue, an unattended loop, and a phone UI"
        );
        assert_eq!(
            merge_subject("", "# port the retry logic\n\ndetails"),
            "port the retry logic",
            "an empty title falls back to the task's first line, heading marks stripped"
        );
    }

    #[test]
    fn a_failing_checks_details_url_yields_the_job_to_read_logs_from() {
        let url = "https://github.com/yukimemi/magi/actions/runs/33587406996/job/100114323572";
        assert_eq!(job_of(url).as_deref(), Some("100114323572"));
        assert_eq!(run_of(url).as_deref(), Some("33587406996"));
        assert_eq!(job_of("https://coderabbit.ai/status"), None);
        assert_eq!(run_of(""), None);
    }

    #[test]
    fn magis_own_stop_comment_is_never_read_back_as_a_finding() {
        let mut out = Vec::new();
        push_if_outstanding(
            &mut out,
            ReviewComment {
                author: "yukimemi".to_owned(),
                path: None,
                line: None,
                body: format!("{MARKER}\nmagi stopped landing this pull request: 1 check failing"),
            },
        );
        assert!(out.is_empty());
    }

    /// A run with no tally, so [`RunState::winner`] is `None` and the panel
    /// falls back to the repository - which keeps these tests free of a
    /// worktree, a `git` invocation and a network.
    fn run_state() -> RunState {
        RunState::new(
            std::path::PathBuf::from("/repo/magi"),
            "main".to_owned(),
            "abcdef1234".to_owned(),
            "add retries to the uploader".to_owned(),
            crate::config::Config::default(),
        )
    }

    fn green_pr() -> PrState {
        PrState {
            url: "https://github.com/yukimemi/magi/pull/42".to_owned(),
            number: 42,
            state: PrLifecycle::Open,
            checks: Checks::Green,
            // The forge sees nothing in the way unless a test says otherwise.
            blocking: Blocking::No,
            failing: Vec::new(),
            review_comments: vec![ReviewComment {
                author: "coderabbitai".to_owned(),
                path: Some("src/land.rs".to_owned()),
                line: Some(212),
                body: "this branch never checks the exit code".to_owned(),
            }],
        }
    }

    #[test]
    fn github_facing_land_text_is_english_whatever_the_language() {
        let mut state = run_state();
        state.config.graph.language = "ja".to_owned();
        let comment = stop_comment(&state.id, "checks are still red");
        assert!(comment.is_ascii(), "{comment}");
        assert!(comment.starts_with(MARKER));

        let p = fix_prompt(&state, &green_pr(), 1, 2, "red", "");
        let ja_at = p.find("Write all prose in ja").unwrap();
        let rule_at = p.find(crate::prompt::GITHUB_ENGLISH_HEADING).unwrap();
        assert!(ja_at < rule_at, "{p}");
        assert!(p.contains("stays in Japanese"), "{p}");

        state.config.graph.language = "en".to_owned();
        let p = fix_prompt(&state, &green_pr(), 1, 2, "red", "");
        assert!(p.contains(crate::prompt::GITHUB_ENGLISH_HEADING), "{p}");
        assert!(!p.contains("does not apply"), "{p}");
    }

    const NUMSTAT: &str = "12\t3\tsrc/land.rs\n40\t1\tsrc/web.rs\n-\t-\tassets/logo.png";

    fn panel() -> String {
        approval_panel(
            &run_state(),
            &green_pr(),
            NUMSTAT,
            "diff --git a/src/land.rs b/src/land.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context",
            &[
                "land: ask before merging".to_owned(),
                "land: colour the diff".to_owned(),
            ],
            "feat: merge approval from the phone",
        )
    }

    #[test]
    fn the_approval_panel_carries_the_whole_case_for_the_merge() {
        let html = panel();
        for needle in [
            "42",
            "main",
            "src/land.rs",
            "src/web.rs",
            "assets/logo.png",
            "feat: merge approval from the phone",
            "land: ask before merging",
            "land: colour the diff",
            "coderabbitai",
            "this branch never checks the exit code",
            "green",
        ] {
            assert!(html.contains(needle), "the panel must state `{needle}`");
        }
    }

    /// A candidate whose label is `A` and has won, so [`RunState::winner`]
    /// resolves to it.
    fn winning_candidate(summary: &str) -> Candidate {
        Candidate {
            index: 0,
            label: 'A',
            agent: "opus".to_owned(),
            branch: "magi/x/A".to_owned(),
            worktree: PathBuf::from("/wt/A"),
            summary: summary.to_owned(),
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

    fn uncontested_tally() -> Tally {
        Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 1,
            present: 1,
            quorum: 1,
            met_quorum: true,
            uncontested: None,
        }
    }

    fn review_record(reviewer: usize, agent: &str, summary: &str) -> ReviewRecord {
        ReviewRecord {
            attempts: 0,
            reviewer,
            agent: agent.to_owned(),
            summary: summary.to_owned(),
            findings: Vec::new(),
            vote: None,
            failed: None,
            duration_ms: 0,
        }
    }

    fn review_round(round: usize, reviews: Vec<ReviewRecord>) -> ReviewRound {
        let answered = reviews.len();
        ReviewRound {
            round,
            head: "abc1234".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews,
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered,
            expected: answered,
            clean: true,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }
    }

    #[test]
    fn the_approval_panel_states_the_task_verbatim_in_either_language() {
        let en = panel();
        assert!(en.contains("Task"), "{en}");
        assert!(en.contains("add retries to the uploader"), "{en}");

        let mut state = run_state();
        state.config.graph.language = "ja".to_owned();
        let ja = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(ja.contains("タスク"), "{ja}");
        assert!(
            ja.contains("add retries to the uploader"),
            "the task itself is not translated: {ja}"
        );
    }

    #[test]
    fn the_approval_panel_omits_what_changed_and_review_verdict_with_no_data() {
        // `run_state()` has no candidates, no tally and no reviews - exactly
        // the shape a run has before anything has judged or reviewed it, and
        // the panel must not print an empty box for either.
        let html = panel();
        assert!(!html.contains("What changed"), "{html}");
        assert!(!html.contains("Review verdict"), "{html}");
    }

    #[test]
    fn the_approval_panel_omits_what_changed_when_the_winners_summary_is_empty() {
        let mut state = run_state();
        state.candidates = vec![winning_candidate("")];
        state.tally = Some(uncontested_tally());
        let html = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(
            !html.contains("What changed"),
            "an empty summary must not render an empty box: {html}"
        );
    }

    #[test]
    fn the_approval_panel_shows_the_winners_own_account_in_either_language() {
        let mut state = run_state();
        state.candidates = vec![winning_candidate(
            "Added a retry loop around the uploader PUT call.",
        )];
        state.tally = Some(uncontested_tally());
        let en = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(en.contains("What changed"), "{en}");
        assert!(
            en.contains("Added a retry loop around the uploader PUT call."),
            "{en}"
        );

        state.config.graph.language = "ja".to_owned();
        let ja = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(ja.contains("変更内容"), "{ja}");
        assert!(
            ja.contains("Added a retry loop around the uploader PUT call."),
            "{ja}"
        );
    }

    #[test]
    fn the_approval_panel_shows_only_the_last_review_rounds_verdict() {
        let mut state = run_state();
        state.reviews = vec![
            review_round(
                1,
                vec![review_record(1, "alpha", "found a race, sent back")],
            ),
            review_round(2, vec![review_record(1, "alpha", "race is fixed, clean")]),
        ];
        let en = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(en.contains("Review verdict"), "{en}");
        assert!(en.contains("race is fixed, clean"), "{en}");
        assert!(
            !en.contains("found a race, sent back"),
            "only the round that actually cleared the merge should show: {en}"
        );

        state.config.graph.language = "ja".to_owned();
        let ja = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(ja.contains("レビューの結論"), "{ja}");
        assert!(ja.contains("レビュアー"), "{ja}");
        assert!(ja.contains("race is fixed, clean"), "{ja}");
    }

    /// The `incomplete_review = "warn"` policy (see
    /// `graph::Runner::review_loop`) can push a `clean` round to
    /// `state.reviews` while one seat's own record still has `failed: Some`
    /// and an empty `summary` - a seat that never answered, not one that
    /// answered with nothing to say.
    fn unanswered_review_record(reviewer: usize, agent: &str, reason: &str) -> ReviewRecord {
        ReviewRecord {
            attempts: 0,
            reviewer,
            agent: agent.to_owned(),
            summary: String::new(),
            findings: Vec::new(),
            vote: None,
            failed: Some(reason.to_owned()),
            duration_ms: 0,
        }
    }

    #[test]
    fn the_approval_panel_never_shows_an_unanswered_seat_as_a_blank_verdict() {
        let mut state = run_state();
        state.reviews = vec![review_round(
            1,
            vec![
                review_record(1, "alpha", "clean, nothing to add"),
                unanswered_review_record(2, "beta", "timed out"),
            ],
        )];
        let en = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(en.contains("clean, nothing to add"), "{en}");
        assert!(
            en.contains("produced no answer: timed out"),
            "a seat that never answered must say so, not render a blank box: {en}"
        );
        assert!(
            !en.contains("<div style=\"white-space:pre-wrap;font-size:13px\"></div>"),
            "no reviewer box may be left empty: {en}"
        );

        state.config.graph.language = "ja".to_owned();
        let ja = approval_panel(&state, &green_pr(), NUMSTAT, "", &[], "feat: x");
        assert!(ja.contains("回答なし: timed out"), "{ja}");
    }

    #[test]
    fn the_approval_panel_contains_nothing_the_frames_policy_would_block() {
        let html = panel();
        assert!(!html.contains("<script"), "no script survives the csp");
        assert!(!html.contains("<form"), "form-action is 'none'");
        let pr = green_pr();
        assert_eq!(
            html.matches("http").count(),
            html.matches(pr.url.as_str()).count(),
            "the only http url in the panel is the pull request's own link"
        );
    }

    #[test]
    fn added_and_removed_diff_lines_are_distinguishable_without_colour() {
        let html = panel();
        assert!(
            html.contains(">+</span>"),
            "an added line carries a `+` in the gutter, not only a background"
        );
        assert!(
            html.contains(">-</span>"),
            "a removed line carries a `-` in the gutter, not only a background"
        );
        assert!(
            html.contains(">new line</span>"),
            "the marker is moved to the gutter, so the body is printed once without it"
        );
    }

    #[test]
    fn a_diff_past_the_threshold_is_cut_with_an_honest_count() {
        let total = DIFF_MAX_LINES + 100;
        let diff: String = (0..total).map(|i| format!("+line {i}\n")).collect();
        let html = approval_panel(
            &run_state(),
            &green_pr(),
            NUMSTAT,
            &diff,
            &[],
            "feat: something long",
        );
        assert!(
            html.contains(&format!("100 of {total} diff lines omitted")),
            "the note must say exactly how much was cut"
        );
        assert!(html.contains(&format!("line {}", DIFF_MAX_LINES - 1)));
        assert!(
            !html.contains(&format!("line {DIFF_MAX_LINES}")),
            "nothing past the threshold is rendered"
        );
        assert!(
            html.contains("/repo/magi"),
            "the note says where the rest is"
        );
    }

    #[test]
    fn a_path_with_html_metacharacters_is_escaped_rather_than_rendered() {
        let html = approval_panel(
            &run_state(),
            &green_pr(),
            "1\t2\tsrc/<b>&\"x\"'.rs",
            "",
            &[],
            "subject",
        );
        assert!(html.contains("src/&lt;b&gt;&amp;&quot;x&quot;&#39;.rs"));
        assert!(
            !html.contains("<b>"),
            "an agent-influenced path must never become markup"
        );
    }

    #[tokio::test]
    async fn the_merge_lock_serialises_one_repository_but_never_a_different_one() {
        let a = std::path::PathBuf::from("/repo/a");
        let b = std::path::PathBuf::from("/repo/b");

        let held = repo_merge_lock(&a).lock_owned().await;

        // A second, concurrent land run against the *same* repository must
        // wait - `try_lock` fails while `held` is alive.
        assert!(
            repo_merge_lock(&a).try_lock().is_err(),
            "a second merge into the same repository must not proceed concurrently"
        );

        // A run against a *different* repository must not be blocked by it -
        // this is what keeps a slow rebase or `gh pr merge` in one
        // repository from also stalling a land-approval resume in another.
        assert!(
            repo_merge_lock(&b).try_lock().is_ok(),
            "a different repository's merge lock must be independent"
        );

        drop(held);
        assert!(
            repo_merge_lock(&a).try_lock().is_ok(),
            "the lock is released once the holder is done"
        );
    }

    #[test]
    fn only_the_merge_choice_merges_and_silence_holds() {
        let table = [
            (None, Approval::Hold),
            (Some("merge"), Approval::Merge),
            (Some(" merge\n"), Approval::Merge),
            (Some("hold"), Approval::Hold),
            (Some(""), Approval::Hold),
            (Some("yes"), Approval::Hold),
        ];
        for (answer, want) in table {
            assert_eq!(
                approval(answer),
                want,
                "answer {answer:?} must resolve to {want:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_first_visit_to_the_merge_gate_files_a_question_and_returns_pending_at_once() {
        crate::run::set_home(std::env::temp_dir().join("magi-land-approval-test-home"));
        let mut state = run_state();
        state.config.graph.land_approval = true;
        let pr = green_pr();

        let gate = approval_gate(&mut state, &pr, "feat: x", None, "abc")
            .await
            .unwrap();
        assert_eq!(gate, ApprovalGate::Pending, "nobody has answered yet");
        assert!(
            !state.parked,
            "approval_gate itself never sets `parked`; only its caller does"
        );

        let store = ask::Questions::open();
        let filed: Vec<_> = store
            .list()
            .into_iter()
            .filter(|q| q.run == state.id)
            .collect();
        assert_eq!(filed.len(), 1, "exactly one question is filed");
        assert_eq!(filed[0].node, APPROVAL_NODE);
        assert_eq!(filed[0].choices, vec![APPROVE.to_owned(), HOLD.to_owned()]);
        assert!(filed[0].status.open());

        // A second visit - standing in for a resumed run whose slot the
        // daemon handed to something else while nobody had answered - must
        // find the same question rather than filing a second one.
        let again = approval_gate(&mut state, &pr, "feat: x", None, "abc")
            .await
            .unwrap();
        assert_eq!(again, ApprovalGate::Pending);
        let still_one = store
            .list()
            .into_iter()
            .filter(|q| q.run == state.id)
            .count();
        assert_eq!(
            still_one, 1,
            "asking twice must not double-file the question"
        );
    }

    #[tokio::test]
    async fn approving_the_existing_question_is_read_back_as_approved() {
        crate::run::set_home(std::env::temp_dir().join("magi-land-approval-test-home"));
        let mut state = run_state();
        state.config.graph.land_approval = true;
        let pr = green_pr();
        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", None, "abc")
                .await
                .unwrap(),
            ApprovalGate::Pending
        );

        let store = ask::Questions::open();
        let mut q = store
            .list()
            .into_iter()
            .find(|q| q.run == state.id)
            .expect("filed above");
        q.answer(ask::Answer::Choice(APPROVE.to_owned())).unwrap();
        store.put(&mut q).unwrap();

        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", None, "abc")
                .await
                .unwrap(),
            ApprovalGate::Approved
        );
    }

    #[tokio::test]
    async fn holding_or_abandoning_the_existing_question_is_read_back_as_held() {
        crate::run::set_home(std::env::temp_dir().join("magi-land-approval-test-home"));
        let store = ask::Questions::open();

        let mut held_state = run_state();
        held_state.config.graph.land_approval = true;
        let pr = green_pr();
        approval_gate(&mut held_state, &pr, "feat: x", None, "abc")
            .await
            .unwrap();
        let mut q = store
            .list()
            .into_iter()
            .find(|q| q.run == held_state.id)
            .expect("filed above");
        q.answer(ask::Answer::Choice(HOLD.to_owned())).unwrap();
        store.put(&mut q).unwrap();
        assert_eq!(
            approval_gate(&mut held_state, &pr, "feat: x", None, "abc")
                .await
                .unwrap(),
            ApprovalGate::Held
        );

        let mut abandoned_state = run_state();
        abandoned_state.config.graph.land_approval = true;
        approval_gate(&mut abandoned_state, &pr, "feat: x", None, "abc")
            .await
            .unwrap();
        let mut q = store
            .list()
            .into_iter()
            .find(|q| q.run == abandoned_state.id)
            .expect("filed above");
        q.abandon("no answer within the timeout");
        store.put(&mut q).unwrap();
        assert_eq!(
            approval_gate(&mut abandoned_state, &pr, "feat: x", None, "abc")
                .await
                .unwrap(),
            ApprovalGate::Held,
            "silence must never merge"
        );
    }

    fn contested() -> ContestedHandoff {
        let finding = |id: &str, n: u32| crate::verdict::Finding {
            id: id.to_owned(),
            severity: crate::verdict::Severity::Major,
            file: Some("src/a.rs".to_owned()),
            line: Some(n),
            title: format!("problem {id}"),
            detail: String::new(),
        };
        ContestedHandoff {
            findings: (1..=7).map(|n| finding(&format!("R3-1-{n}"), n)).collect(),
            rejecters: vec![(1, "alpha".to_owned())],
        }
    }

    #[test]
    fn the_contested_record_is_asked_about_unless_the_switch_is_off() {
        let mut state = run_state();
        assert!(contested_to_ask(&state).is_none(), "nothing recorded");
        state.contested_handoff = Some(contested());
        assert!(contested_to_ask(&state).is_some());
        state.config.graph.hold_contested_merge = false;
        assert!(
            contested_to_ask(&state).is_none(),
            "the switch restores today"
        );
    }

    #[test]
    fn the_deputy_brief_carries_the_pr_the_findings_and_names_what_is_missing() {
        let q = ask::Question::new(
            "run-1".to_owned(),
            APPROVAL_NODE.to_owned(),
            "land".to_owned(),
            "Merge?".to_owned(),
            String::new(),
            vec![APPROVE.to_owned(), HOLD.to_owned()],
        );
        let none = deputy_brief(&q, None);
        assert!(none.contains("could not be read"), "{none}");
        assert!(none.contains("Silence is a hold"), "{none}");

        let mut state = run_state();
        state.pr = Some(crate::run::PrRecord {
            url: "https://example.test/pull/7".to_owned(),
            number: 7,
            state: "open".to_owned(),
            checks: "green".to_owned(),
            round: 0,
            rounds: 3,
            red_at_merge: Vec::new(),
        });
        state.contested_handoff = Some(contested());
        let b = deputy_brief(&q, Some(&state));
        assert!(b.contains("https://example.test/pull/7"), "{b}");
        assert!(b.contains("R3-1-1") && b.contains("src/a.rs:1"), "{b}");
        assert!(b.contains("#1"), "the rejecting seat: {b}");
        state.contested_handoff = None;
        assert!(deputy_brief(&q, Some(&state)).contains("not recorded as contested"));
    }

    #[test]
    fn the_contested_question_names_the_pr_the_findings_and_the_rejecter() {
        for lang in ["en", "ja"] {
            let mut cfg = crate::config::Config::default();
            cfg.graph.language = lang.to_owned();
            let w = words(&cfg.graph.language);
            let text = w.approval_detail(
                "https://github.com/yukimemi/magi/pull/42",
                "main",
                "feat: x",
                Some(&contested()),
            );
            assert!(text.contains("pull/42"), "{text}");
            assert!(
                text.contains("R3-1-1 Major src/a.rs:1: problem R3-1-1"),
                "{text}"
            );
            assert!(text.contains("R3-1-5"), "{text}");
            assert!(!text.contains("R3-1-6"), "the list is capped: {text}");
            assert!(text.contains("2"), "the rest are counted: {text}");
            assert!(text.contains("#1 (alpha)"), "{text}");
        }
        let plain = words("en").approval_detail("u", "main", "s", None);
        assert!(!plain.contains("reject"), "{plain}");
    }

    #[tokio::test]
    async fn a_contested_question_is_filed_once_and_a_resume_finds_the_same_one() {
        crate::run::set_home(std::env::temp_dir().join("magi-land-approval-test-home"));
        let mut state = run_state();
        state.config.graph.land_approval = false;
        state.contested_handoff = Some(contested());
        let pr = green_pr();
        let c = contested_to_ask(&state);
        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", c.as_ref(), "abc")
                .await
                .unwrap(),
            ApprovalGate::Pending,
            "silence is a hold"
        );
        let store = ask::Questions::open();
        let filed: Vec<_> = store
            .list()
            .into_iter()
            .filter(|q| q.run == state.id)
            .collect();
        assert_eq!(filed.len(), 1);
        assert!(filed[0].detail.contains("R3-1-1"), "{}", filed[0].detail);

        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", c.as_ref(), "abc")
                .await
                .unwrap(),
            ApprovalGate::Pending
        );
        let mut q = store
            .list()
            .into_iter()
            .find(|q| q.run == state.id)
            .unwrap();
        assert_eq!(q.id, filed[0].id, "the same question after a resume");
        q.answer(ask::Answer::Choice(APPROVE.to_owned())).unwrap();
        store.put(&mut q).unwrap();
        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", c.as_ref(), "abc")
                .await
                .unwrap(),
            ApprovalGate::Approved
        );
    }

    #[test]
    fn the_diffstat_table_is_ordered_by_churn_with_binaries_last() {
        let rows = parse_numstat(NUMSTAT);
        assert_eq!(
            rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            ["src/web.rs", "src/land.rs", "assets/logo.png"]
        );
        assert_eq!(rows[2].added, None, "a binary file has no line counts");
    }
    #[test]
    fn the_approval_speaks_the_language_the_repository_is_configured_for() {
        // Reported from a real run: the merge question arrived in English on a
        // repository with `language = "ja"`. magi's own strings have to follow
        // that setting too - "it is a literal in Rust" is not an answer.
        let mut state = run_state();
        state.config.graph.language = "ja".to_owned();
        let pr = green_pr();
        let commits = ["c1".to_owned()];

        let ja = approval_panel(&state, &pr, "3\t1\tsrc/a.rs", "+ x", &commits, "feat: x");
        assert!(ja.contains("lang=\"ja\""), "the document must declare it");
        assert!(ja.contains("squash されるコミット"), "{ja}");
        assert!(ja.contains("レビューコメント"), "{ja}");
        assert!(ja.contains("差分"), "{ja}");
        assert!(
            !ja.contains("Commits being squashed"),
            "no English left over"
        );

        let w = words("ja");
        assert!(w.approval_summary(17, "feat: x").contains("マージ"));
        assert!(
            w.approval_detail("http://x/1", "main", "feat: x", None)
                .contains("パネル")
        );

        // The evidence itself is language-neutral and must survive either way.
        assert!(ja.contains("src/a.rs"), "the diffstat is not prose");
        assert!(ja.contains("feat: x"), "nor is the merge subject");

        // English stays the default, and a language magi cannot check falls
        // back to it rather than shipping a guess.
        state.config.graph.language = "en".to_owned();
        let en = approval_panel(&state, &pr, "3\t1\tsrc/a.rs", "+ x", &commits, "feat: x");
        assert!(en.contains("Commits being squashed"), "{en}");
        assert_eq!(words("Klingon").html_lang, "en");
    }

    /// A `gh pr list` result naming exactly one pull request whose base and
    /// merge time both fit the run is exactly the case
    /// [`find_external_merge`] exists to act on.
    #[test]
    fn pick_open_pr_classifies_by_count_and_base() {
        let one = r#"[{"number":58,"url":"https://x/pull/58","title":"t","baseRefName":"main"}]"#;
        assert_eq!(
            pick_open_pr(one, "main").unwrap(),
            OpenPr::One {
                url: "https://x/pull/58".into(),
                title: "t".into()
            }
        );
        assert_eq!(pick_open_pr("[]", "main").unwrap(), OpenPr::None);
        assert_eq!(pick_open_pr(one, "dev").unwrap(), OpenPr::None);
        let two = r#"[{"number":1,"url":"u1","title":"","baseRefName":"main"},
                     {"number":2,"url":"u2","title":"","baseRefName":"main"}]"#;
        assert_eq!(
            pick_open_pr(two, "main").unwrap(),
            OpenPr::Many(vec!["u1".into(), "u2".into()])
        );
        assert!(pick_open_pr("not json", "main").is_err());
        // An incomplete record is an error, never "nothing open".
        assert!(pick_open_pr(r#"[{"url":"u","title":"t"}]"#, "main").is_err());
        assert!(pick_open_pr(r#"[{"title":"t","baseRefName":"main"}]"#, "main").is_err());
    }

    #[test]
    fn pick_merged_pr_picks_the_unique_match() {
        let json = r#"[
            {"url": "https://github.com/o/r/pull/42", "number": 42,
             "mergedAt": "2026-09-20T10:00:00Z", "baseRefName": "main"}
        ]"#;
        let created_at: Timestamp = "2026-09-19T00:00:00Z".parse().unwrap();
        let found = pick_merged_pr(json, "main", created_at)
            .expect("valid json")
            .expect("one unambiguous match");
        assert_eq!(found.url, "https://github.com/o/r/pull/42");
        assert_eq!(found.number, 42);
    }

    /// Two candidates surviving the filter is exactly as uninformative as
    /// zero — a branch name can be reused across runs — so neither is
    /// preferred over the other and nothing is recorded automatically.
    #[test]
    fn pick_merged_pr_refuses_when_more_than_one_candidate_survives() {
        let json = r#"[
            {"url": "https://github.com/o/r/pull/42", "number": 42,
             "mergedAt": "2026-09-20T10:00:00Z", "baseRefName": "main"},
            {"url": "https://github.com/o/r/pull/43", "number": 43,
             "mergedAt": "2026-09-21T10:00:00Z", "baseRefName": "main"}
        ]"#;
        let created_at: Timestamp = "2026-09-19T00:00:00Z".parse().unwrap();
        assert_eq!(pick_merged_pr(json, "main", created_at).unwrap(), None);
    }

    /// A pull request that targets a different base branch cannot be this
    /// run's, whatever its head branch is named — a reused branch name from
    /// an unrelated task must not be recorded as this run's merge.
    #[test]
    fn pick_merged_pr_ignores_a_different_base_branch() {
        let json = r#"[
            {"url": "https://github.com/o/r/pull/42", "number": 42,
             "mergedAt": "2026-09-20T10:00:00Z", "baseRefName": "release"}
        ]"#;
        let created_at: Timestamp = "2026-09-19T00:00:00Z".parse().unwrap();
        assert_eq!(pick_merged_pr(json, "main", created_at).unwrap(), None);
    }

    /// A pull request merged before this run was even created cannot be this
    /// run's winner, no matter how its head branch is spelled.
    #[test]
    fn pick_merged_pr_ignores_a_merge_that_predates_the_run() {
        let json = r#"[
            {"url": "https://github.com/o/r/pull/42", "number": 42,
             "mergedAt": "2026-09-18T10:00:00Z", "baseRefName": "main"}
        ]"#;
        let created_at: Timestamp = "2026-09-19T00:00:00Z".parse().unwrap();
        assert_eq!(pick_merged_pr(json, "main", created_at).unwrap(), None);
    }

    #[test]
    fn slug_of_pr_url_reads_host_owner_and_repo() {
        assert_eq!(
            slug_of_pr_url("https://github.com/yukimemi/shun/pull/272").as_deref(),
            Some("github.com/yukimemi/shun")
        );
    }

    #[test]
    fn slug_of_pr_url_refuses_a_url_with_no_pull_segment() {
        assert_eq!(slug_of_pr_url("https://github.com/yukimemi/shun"), None);
        assert_eq!(slug_of_pr_url("not a url at all"), None);
        assert_eq!(slug_of_pr_url("https://github.com"), None);
    }

    #[test]
    fn slug_of_repo_url_reads_host_owner_and_repo() {
        assert_eq!(
            slug_of_repo_url("https://github.com/yukimemi/magi").as_deref(),
            Some("github.com/yukimemi/magi")
        );
        assert_eq!(slug_of_repo_url("https://github.com"), None);
    }

    #[test]
    fn ensure_same_repo_accepts_a_matching_slug_regardless_of_case() {
        ensure_same_repo("github.com/yukimemi/magi", "GitHub.Com/YukiMemi/Magi")
            .expect("same repo, different case");
    }

    /// The shun/8c75 incident: an id-less `--merged` picked this repository's
    /// own in-progress run and rewrote its status from a pull request in a
    /// completely different repository. This is the guard that must catch
    /// that even when an explicit (but wrong) id is given.
    #[test]
    fn ensure_same_repo_refuses_a_different_repo() {
        let err =
            ensure_same_repo("github.com/yukimemi/magi", "github.com/yukimemi/shun").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("github.com/yukimemi/magi"), "{msg}");
        assert!(msg.contains("github.com/yukimemi/shun"), "{msg}");
    }

    /// Same owner/repo on two different forge hosts (a GitHub Enterprise
    /// instance mirroring a `github.com` repository's name, say) must not be
    /// treated as the same repository just because the trailing path
    /// matches.
    #[test]
    fn ensure_same_repo_refuses_the_same_slug_on_a_different_host() {
        let err = ensure_same_repo(
            "github.com/yukimemi/magi",
            "github.example.com/yukimemi/magi",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("github.com/yukimemi/magi"), "{msg}");
        assert!(msg.contains("github.example.com/yukimemi/magi"), "{msg}");
    }

    /// No winner decided yet means there is no branch to ask GitHub about at
    /// all — `find_external_merge` must return `None` without ever spawning
    /// `gh`, which this proves by never providing a real repository to spawn
    /// it in.
    #[tokio::test]
    async fn find_external_merge_returns_none_without_a_winner() {
        let state = RunState::new(
            PathBuf::from("/no/such/repo"),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            "irrelevant".to_owned(),
            crate::config::Config::default(),
        );
        assert_eq!(find_external_merge(&state).await.unwrap(), None);
    }

    fn pr_run(home: &Path, id: &str, repo: &str, status: RunStatus, url: &str, state: &str) {
        let mut run = RunState::new(
            PathBuf::from(repo),
            "main".to_owned(),
            "abcdef1234".to_owned(),
            "x".to_owned(),
            crate::config::Config::default(),
        );
        run.id = id.to_owned();
        run.status = status;
        run.pr = Some(crate::run::PrRecord {
            number: url.rsplit('/').next().unwrap().parse().unwrap(),
            url: url.to_owned(),
            state: state.to_owned(),
            checks: "red".to_owned(),
            round: 0,
            rounds: 2,
            red_at_merge: Vec::new(),
        });
        run.save_under(home).unwrap();
    }

    fn recorded(home: &Path, id: &str) -> String {
        let body = std::fs::read_to_string(home.join("runs").join(id).join("run.json")).unwrap();
        serde_json::from_str::<RunState>(&body)
            .unwrap()
            .pr
            .unwrap()
            .state
    }

    const PR: &str = "https://github.com/o/r/pull/7";

    #[test]
    fn write_through_updates_predecessors_and_siblings_only() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        pr_run(
            h,
            "20261004-100000-aaaa",
            "/repo/r",
            RunStatus::Superseded,
            PR,
            "open",
        );
        pr_run(
            h,
            "20261004-100100-bbbb",
            "/repo/r",
            RunStatus::Blocked,
            PR,
            "open",
        );
        // Not terminal: a driver may be writing it.
        pr_run(
            h,
            "20261004-100200-cccc",
            "/repo/r",
            RunStatus::Landing,
            PR,
            "open",
        );
        // Another repository's pull request with the same number.
        pr_run(
            h,
            "20261004-100300-dddd",
            "/repo/other",
            RunStatus::Blocked,
            "https://github.com/o/other/pull/7",
            "open",
        );
        // A different pull request of the same repository.
        pr_run(
            h,
            "20261004-100400-eeee",
            "/repo/r",
            RunStatus::Blocked,
            "https://github.com/o/r/pull/8",
            "open",
        );
        pr_run(
            h,
            "20261004-100500-ffff",
            "/repo/r",
            RunStatus::Merged,
            PR,
            "open",
        );
        let source = RunState::load_under("20261004-100500-ffff", h).unwrap();

        assert_eq!(
            write_pr_state_through_in(h, &source, PrLifecycle::Merged),
            2
        );
        assert_eq!(recorded(h, "20261004-100000-aaaa"), "merged");
        assert_eq!(recorded(h, "20261004-100100-bbbb"), "merged");
        assert_eq!(recorded(h, "20261004-100200-cccc"), "open");
        assert_eq!(recorded(h, "20261004-100300-dddd"), "open");
        assert_eq!(recorded(h, "20261004-100400-eeee"), "open");
        // The source's own record is the caller's to write.
        assert_eq!(recorded(h, "20261004-100500-ffff"), "open");
        // Idempotent.
        assert_eq!(
            write_pr_state_through_in(h, &source, PrLifecycle::Merged),
            0
        );
        let hit = RunState::load_under("20261004-100000-aaaa", h).unwrap();
        assert!(hit.events.iter().any(|e| e.message.contains("merged")));
    }

    #[test]
    fn repair_rewrites_merged_and_closed_and_leaves_open_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        let url = |n: u32| format!("https://github.com/o/r/pull/{n}");
        pr_run(
            h,
            "20261004-100000-aaaa",
            "/repo/r",
            RunStatus::Superseded,
            &url(1),
            "open",
        );
        pr_run(
            h,
            "20261004-100100-bbbb",
            "/repo/r",
            RunStatus::Blocked,
            &url(2),
            "open",
        );
        pr_run(
            h,
            "20261004-100200-cccc",
            "/repo/r",
            RunStatus::Ready,
            &url(3),
            "open",
        );
        pr_run(
            h,
            "20261004-100300-dddd",
            "/repo/r",
            RunStatus::Ready,
            &url(4),
            "open",
        );
        pr_run(
            h,
            "20261004-100400-eeee",
            "/repo/r",
            RunStatus::Implementing,
            &url(1),
            "open",
        );
        assert_eq!(stale_open_prs(h).len(), 4);

        let mut known = BTreeMap::new();
        known.insert(url(1), PrLifecycle::Merged);
        known.insert(url(2), PrLifecycle::Closed);
        known.insert(url(3), PrLifecycle::Open);
        // #4: the forge could not be read, so it has no answer.
        assert_eq!(apply_pr_states(h, &known), 2);
        assert_eq!(recorded(h, "20261004-100000-aaaa"), "merged");
        assert_eq!(recorded(h, "20261004-100100-bbbb"), "closed");
        assert_eq!(recorded(h, "20261004-100200-cccc"), "open");
        assert_eq!(recorded(h, "20261004-100300-dddd"), "open");
        assert_eq!(recorded(h, "20261004-100400-eeee"), "open");
        assert_eq!(apply_pr_states(h, &known), 0);
    }

    // --- the land loop against a scripted forge -------------------------

    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Answers views from a script (the last one repeats), merges from a
    /// queue, and records every call so a test can assert the order.
    struct Scripted {
        views: Mutex<VecDeque<Seen>>,
        merges: Mutex<VecDeque<(bool, String)>>,
        fix: Mutex<Option<Fixed>>,
        log: Mutex<Vec<&'static str>>,
        argvs: Mutex<Vec<Vec<String>>>,
    }

    impl Scripted {
        fn new(views: Vec<Seen>, merges: Vec<(bool, &str)>) -> Self {
            Self {
                views: Mutex::new(views.into()),
                merges: Mutex::new(
                    merges
                        .into_iter()
                        .map(|(ok, m)| (ok, m.to_owned()))
                        .collect(),
                ),
                fix: Mutex::new(None),
                log: Mutex::new(Vec::new()),
                argvs: Mutex::new(Vec::new()),
            }
        }
        fn argvs(&self) -> Vec<Vec<String>> {
            self.argvs.lock().unwrap().clone()
        }
        fn calls(&self) -> Vec<&'static str> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Forge for Scripted {
        async fn view(&self, _repo: &Path, _url: &str) -> Result<Seen> {
            self.log.lock().unwrap().push("view");
            let mut v = self.views.lock().unwrap();
            Ok(if v.len() > 1 {
                v.pop_front().unwrap()
            } else {
                v[0].clone()
            })
        }
        async fn merge(&self, _repo: &Path, argv: &[String]) -> Result<(bool, String)> {
            self.log.lock().unwrap().push("merge");
            self.argvs.lock().unwrap().push(argv.to_vec());
            Ok(self
                .merges
                .lock()
                .unwrap()
                .pop_front()
                .expect("unscripted merge"))
        }
        async fn poll(&self) {
            self.log.lock().unwrap().push("poll");
        }
        async fn fix(
            &self,
            _state: &mut RunState,
            _pr: &PrState,
            _round: usize,
            _budget: usize,
            _reason: &str,
            _logs: &str,
        ) -> Result<Fixed> {
            self.log.lock().unwrap().push("fix");
            Ok(self.fix.lock().unwrap().take().expect("unscripted fix"))
        }
    }

    const CLEAN_REFUSAL: &str =
        "GraphQL: Pull request is in clean status (enablePullRequestAutoMerge)";
    const UNAVAILABLE: &str =
        "GraphQL: Auto merge is not allowed for this repository (enablePullRequestAutoMerge)";
    const REFUSED: &str =
        "X Pull request #42 is not mergeable: the base branch policy prohibits the merge.";

    fn seen(head: &str, checks: Checks, merge_state: &str, comments: bool) -> Seen {
        let mut pr = green_pr();
        pr.checks = checks;
        pr.blocking = Blocking::of(merge_state);
        if !comments {
            pr.review_comments.clear();
        }
        Seen {
            pr,
            title: "feat: x".to_owned(),
            failing_urls: Vec::new(),
            head: head.to_owned(),
            rollup_head: head.to_owned(),
            merge_state: merge_state.to_owned(),
            contexts: Vec::new(),
        }
    }

    fn landing_state() -> RunState {
        crate::run::set_home(std::env::temp_dir().join("magi-land-approval-test-home"));
        let mut state = run_state();
        state.config.graph.land_approval = false;
        // Tests run in parallel and `run_state` ids come from the clock, so two
        // of them would otherwise share one run directory.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        state.id = format!(
            "20261004-000000-{:04x}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        state
    }

    #[test]
    fn a_pushed_head_is_awaited_case_insensitively_and_an_unreadable_one_is_not_a_match() {
        assert!(!awaiting_new_head(None, "aaa"));
        assert!(!awaiting_new_head(Some("abc123"), "ABC123"));
        assert!(awaiting_new_head(Some("abc123"), "def456"));
        assert!(awaiting_new_head(Some("abc123"), ""));
    }

    #[test]
    fn a_refusal_is_judged_by_the_pull_requests_state_not_by_its_wording() {
        let open = |c, m: &str| seen("a", c, m, false);
        let table = [
            (None, false, Refused::Pending),
            (
                Some(open(Checks::Pending, "BLOCKED")),
                false,
                Refused::Pending,
            ),
            (
                Some(open(Checks::Unknown, "BLOCKED")),
                false,
                Refused::Pending,
            ),
            (
                Some(open(Checks::Green, "UNKNOWN")),
                false,
                Refused::Pending,
            ),
            (Some(open(Checks::Green, "")), false, Refused::Pending),
            (
                Some(open(Checks::Green, "BLOCKED")),
                false,
                Refused::Recheck,
            ),
            (Some(open(Checks::Green, "BLOCKED")), true, Refused::Final),
        ];
        for (after, rechecked, want) in table {
            assert_eq!(classify_refusal(after.as_ref(), rechecked, "a"), want);
        }
        let mut closed = open(Checks::Green, "CLEAN");
        closed.pr.state = PrLifecycle::Closed;
        assert_eq!(classify_refusal(Some(&closed), false, "a"), Refused::Final);
    }

    #[tokio::test]
    async fn a_normal_landing_merges_on_the_first_look() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![seen("a", Checks::Green, "CLEAN", false)],
            vec![(false, CLEAN_REFUSAL), (true, "")],
        );
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        // Already clean: auto-merge has nothing to wait for and is refused, so
        // the guarded direct merge goes ahead after a fresh read.
        assert_eq!(forge.calls(), ["view", "view", "merge", "view", "merge"]);
        assert_eq!(state.status, RunStatus::Merged);
    }

    #[tokio::test]
    async fn after_a_pushed_fix_no_merge_is_tried_until_the_head_matches() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("old", Checks::Green, "CLEAN", true),
                // The forge has not moved to the new head yet: still green.
                seen("old", Checks::Green, "CLEAN", true),
                seen("new", Checks::Pending, "BLOCKED", true),
                seen("new", Checks::Green, "CLEAN", true),
            ],
            vec![(false, CLEAN_REFUSAL), (true, "")],
        );
        *forge.fix.lock().unwrap() = Some(Fixed::Committed {
            head: "NEW".to_owned(),
        });
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        assert_eq!(
            forge.calls(),
            [
                "view", "fix", "poll", "view", "poll", "view", "poll", "view", "view", "merge",
                "view", "merge"
            ]
        );
        assert_eq!(state.status, RunStatus::Merged);
    }

    #[tokio::test]
    async fn a_head_that_never_arrives_stops_naming_both_commits() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("old", Checks::Green, "CLEAN", true),
                seen("someone-elses", Checks::Green, "CLEAN", true),
            ],
            vec![],
        );
        *forge.fix.lock().unwrap() = Some(Fixed::Committed {
            head: "mine".to_owned(),
        });
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        assert!(!forge.calls().contains(&"merge"));
        let why = state.merge.as_ref().unwrap().detail.clone();
        assert!(
            why.contains("mine") && why.contains("someone-elses"),
            "{why}"
        );
        assert_eq!(state.status, RunStatus::Blocked);
    }

    #[tokio::test]
    async fn a_policy_refusal_while_checks_run_waits_and_then_merges() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Pending, "BLOCKED", false),
                seen("a", Checks::Pending, "BLOCKED", false),
                seen("a", Checks::Green, "CLEAN", false),
            ],
            vec![(false, REFUSED), (false, CLEAN_REFUSAL), (true, "")],
        );
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        assert_eq!(
            forge.calls(),
            [
                "view", "view", "merge", "view", "poll", "view", "poll", "view", "view", "merge",
                "view", "merge"
            ]
        );
        assert_eq!(state.status, RunStatus::Merged);
    }

    #[tokio::test]
    async fn a_refusal_that_outlives_settled_checks_stops_with_the_merge_state() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "BLOCKED", false),
            ],
            vec![(false, REFUSED), (false, REFUSED)],
        );
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        // One re-look is allowed for the forge's own lag, then it is final.
        assert_eq!(forge.calls().iter().filter(|c| **c == "merge").count(), 2);
        let why = state.merge.as_ref().unwrap().detail.clone();
        assert!(
            why.contains("policy prohibits")
                && why.contains("BLOCKED")
                && why.contains("requirements are not met")
                && !why.contains("not available"),
            "{why}"
        );
        assert_eq!(state.status, RunStatus::Blocked);
    }

    #[test]
    fn a_decision_is_bound_to_a_head_only_when_every_signal_agrees() {
        assert_eq!(bound_head("abc", "abc", None), Some("abc"));
        assert_eq!(bound_head("abc", "ABC", Some("abc")), Some("abc"));
        // The pull request is still on the commit before the push.
        assert_eq!(bound_head("old", "old", Some("new")), None);
        // The checks are the previous commit's.
        assert_eq!(bound_head("new", "old", Some("new")), None);
        assert_eq!(bound_head("new", "old", None), None);
        // Nothing readable.
        assert_eq!(bound_head("", "", None), None);
        assert_eq!(bound_head("", "", Some("new")), None);
        assert_eq!(bound_head("abc", "", None), None);
    }

    fn view_json(head: &str) -> String {
        format!(
            r#"{{"url":"https://github.com/o/r/pull/42","number":42,"state":"OPEN",
            "title":"t","headRefOid":"{head}","mergeStateStatus":"CLEAN",
            "reviews":[],"comments":[]}}"#
        )
    }

    fn node_json(oid: &str, check: &str, has_next: bool) -> String {
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"commits":{{"nodes":[{{"commit":
            {{"oid":"{oid}","statusCheckRollup":{{"contexts":{{"pageInfo":{{"hasNextPage":{has_next}}},
            "nodes":[{{"__typename":"CheckRun","name":"ci","status":"COMPLETED",
            "conclusion":"{check}","detailsUrl":"https://example.test/1"}}]}}}}}}}}]}}}}}}}}}}"#
        )
    }

    #[test]
    fn rollup_is_bound_to_the_commit_in_the_same_node() {
        // The view already points at the new head, but the node still answers
        // for the old commit with its red check: the head stays unbound.
        let s = seen_from(&view_json("new"), Some(&node_json("old", "FAILURE", false))).unwrap();
        assert_eq!(s.rollup_head, "old");
        assert_eq!(s.pr.checks, Checks::Red);
        assert_eq!(bound_head(&s.head, &s.rollup_head, Some("new")), None);
        assert_eq!(s.failing_urls.len(), 1);
    }

    #[test]
    fn checks_come_from_the_node_not_the_view() {
        let view = view_json("new").replace(
            r#""reviews""#,
            r#""statusCheckRollup":[{"name":"ci","status":"COMPLETED","conclusion":"FAILURE"}],"reviews""#,
        );
        let s = seen_from(&view, Some(&node_json("new", "SUCCESS", false))).unwrap();
        assert_eq!(s.pr.checks, Checks::Green);
        assert!(s.pr.failing.is_empty());
        assert_eq!(
            bound_head(&s.head, &s.rollup_head, Some("new")),
            Some("new")
        );
    }

    #[test]
    fn an_unreadable_or_paged_node_leaves_the_head_unbound() {
        for node in [
            None,
            Some("not json".to_owned()),
            Some(r#"{"errors":[{"message":"x"}]}"#.to_owned()),
            Some(node_json("new", "SUCCESS", true)),
        ] {
            let s = seen_from(&view_json("new"), node.as_deref()).unwrap();
            assert!(s.rollup_head.is_empty());
            assert_eq!(s.pr.checks, Checks::Unknown);
            assert_eq!(bound_head(&s.head, &s.rollup_head, None), None);
        }
    }

    #[test]
    fn the_merge_command_is_pinned_to_the_observed_head() {
        let argv = merge_argv_at(7, "feat: x", "deadbeef");
        let at = argv
            .iter()
            .position(|a| a == "--match-head-commit")
            .unwrap();
        assert_eq!(argv[at + 1], "deadbeef");
    }

    #[tokio::test]
    async fn stale_checks_after_a_fix_push_never_reach_a_merge() {
        let mut state = landing_state();
        // The pull request is on the pushed head but the rollup is still the
        // previous commit's red, non-required result.
        let mut stale = seen("new", Checks::Red, "CLEAN", false);
        stale.rollup_head = "old".to_owned();
        let forge = Scripted::new(
            vec![seen("old", Checks::Green, "CLEAN", true), stale],
            vec![],
        );
        *forge.fix.lock().unwrap() = Some(Fixed::Committed {
            head: "new".to_owned(),
        });
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        assert!(!forge.calls().contains(&"merge"));
        assert_eq!(state.status, RunStatus::Blocked);
        let why = state.merge.as_ref().unwrap().detail.clone();
        assert!(why.contains("new") && why.contains("old"), "{why}");
    }

    #[test]
    fn a_refusal_read_against_another_commits_checks_is_pending() {
        let mut after = seen("a", Checks::Green, "BLOCKED", false);
        after.rollup_head = "old".to_owned();
        assert_eq!(classify_refusal(Some(&after), true, "a"), Refused::Pending);
    }

    #[tokio::test]
    async fn a_matching_head_with_red_non_required_checks_still_merges() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![seen("a", Checks::Red, "CLEAN", false)],
            vec![(false, CLEAN_REFUSAL), (true, "")],
        );
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        assert_eq!(forge.calls(), ["view", "view", "merge", "view", "merge"]);
        assert_eq!(state.status, RunStatus::Merged);
    }

    #[tokio::test]
    async fn a_merge_refused_because_the_head_moved_looks_again_instead_of_failing() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "CLEAN", false),
                // Re-viewed after the refusal: someone pushed.
                seen("b", Checks::Green, "BLOCKED", false),
                seen("b", Checks::Green, "CLEAN", false),
            ],
            vec![(false, REFUSED), (false, CLEAN_REFUSAL), (true, "")],
        );
        land_with(&mut state, "https://github.com/o/r/pull/42", &forge)
            .await
            .unwrap();
        assert_eq!(
            forge.calls(),
            [
                "view", "view", "merge", "view", "poll", "view", "view", "merge", "view", "merge"
            ]
        );
        assert_eq!(state.status, RunStatus::Merged);
    }

    fn merged_view(head: &str) -> Seen {
        let mut m = seen(head, Checks::Green, "CLEAN", false);
        m.pr.state = PrLifecycle::Merged;
        m
    }

    fn has(argv: &[String], flag: &str) -> bool {
        argv.iter().any(|a| a == flag)
    }

    fn value_of<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        let at = argv.iter().position(|a| a == flag)?;
        argv.get(at + 1).map(String::as_str)
    }

    const URL: &str = "https://github.com/o/r/pull/42";

    #[tokio::test]
    async fn the_merge_step_arms_auto_merge_on_the_observed_head_and_keeps_watching() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("abc", Checks::Green, "CLEAN", false),
                seen("abc", Checks::Green, "CLEAN", false),
                seen("abc", Checks::Pending, "BLOCKED", false),
                merged_view("abc"),
            ],
            vec![(true, "")],
        );
        land_with(&mut state, URL, &forge).await.unwrap();
        assert_eq!(
            forge.calls(),
            ["view", "view", "merge", "poll", "view", "poll", "view"]
        );
        let argv = &forge.argvs()[0];
        assert!(has(argv, "--squash") && has(argv, "--auto") && has(argv, "--subject"));
        assert!(!has(argv, "--admin"));
        assert_eq!(value_of(argv, "--match-head-commit"), Some("abc"));
        assert_eq!(state.status, RunStatus::Merged);
        assert!(state.land_armed_head.is_none());
        assert!(
            state
                .events
                .iter()
                .any(|e| e.message.contains("auto-merge armed on abc")),
            "the arm is recorded with its head"
        );
    }

    #[tokio::test]
    async fn a_fix_round_after_arming_disables_auto_merge_before_it_runs() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "CLEAN", false),
                // A review comment arrives while armed.
                seen("a", Checks::Green, "CLEAN", true),
                seen("b", Checks::Green, "CLEAN", false),
                seen("b", Checks::Green, "CLEAN", false),
                merged_view("b"),
            ],
            vec![(true, ""), (true, ""), (true, "")],
        );
        *forge.fix.lock().unwrap() = Some(Fixed::Committed {
            head: "b".to_owned(),
        });
        land_with(&mut state, URL, &forge).await.unwrap();
        assert_eq!(
            forge.calls(),
            [
                "view", "view", "merge", "poll", "view", "merge", "fix", "poll", "view", "view",
                "merge", "poll", "view"
            ]
        );
        let argvs = forge.argvs();
        assert!(has(&argvs[1], "--disable-auto"));
        // Re-armed afterwards, on the new head only.
        assert!(has(&argvs[2], "--auto"));
        assert_eq!(value_of(&argvs[2], "--match-head-commit"), Some("b"));
        assert_eq!(state.status, RunStatus::Merged);
    }

    #[tokio::test]
    async fn a_failed_disable_stops_before_the_fix_pushes_anything() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "CLEAN", true),
            ],
            vec![(true, ""), (false, "disable exploded")],
        );
        land_with(&mut state, URL, &forge).await.unwrap();
        assert!(!forge.calls().contains(&"fix"));
        assert_eq!(state.status, RunStatus::Blocked);
        let why = state.merge.as_ref().unwrap().detail.clone();
        assert!(why.contains("disable exploded"), "{why}");
    }

    #[tokio::test]
    async fn an_approval_never_carries_over_to_a_new_head() {
        crate::run::set_home(std::env::temp_dir().join("magi-land-approval-test-home"));
        let mut state = run_state();
        state.config.graph.land_approval = true;
        let pr = green_pr();
        let store = ask::Questions::open();

        approval_gate(&mut state, &pr, "feat: x", None, "aaa")
            .await
            .unwrap();
        let mut q = store
            .list()
            .into_iter()
            .find(|q| q.run == state.id)
            .unwrap();
        q.answer(ask::Answer::Choice(APPROVE.to_owned())).unwrap();
        store.put(&mut q).unwrap();
        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", None, "AAA")
                .await
                .unwrap(),
            ApprovalGate::Approved,
            "the same head keeps its approval"
        );

        // A new head is a new question, not the old word.
        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", None, "bbb")
                .await
                .unwrap(),
            ApprovalGate::Pending
        );
        let all: Vec<_> = store
            .list()
            .into_iter()
            .filter(|q| q.run == state.id)
            .collect();
        assert_eq!(all.len(), 2);

        // A question recorded before heads were tracked is not reused either.
        state.land_approval = None;
        assert_eq!(
            approval_gate(&mut state, &pr, "feat: x", None, "bbb")
                .await
                .unwrap(),
            ApprovalGate::Pending
        );
        let open = store
            .list()
            .into_iter()
            .filter(|q| q.run == state.id && q.status.open())
            .count();
        assert_eq!(open, 1, "the superseded question was retired");
    }

    #[tokio::test]
    async fn the_fallback_merges_only_on_the_approved_head() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "CLEAN", false),
                // The fresh read before the direct merge: someone pushed.
                seen("b", Checks::Green, "CLEAN", false),
            ],
            vec![(false, UNAVAILABLE), (false, UNAVAILABLE), (true, "")],
        );
        land_with(&mut state, URL, &forge).await.unwrap();
        let direct: Vec<_> = forge
            .argvs()
            .into_iter()
            .filter(|a| !has(a, "--auto"))
            .collect();
        assert_eq!(direct.len(), 1, "no merge was tried on the moved head");
        assert_eq!(value_of(&direct[0], "--match-head-commit"), Some("b"));
        assert_eq!(state.status, RunStatus::Merged);
    }

    #[tokio::test]
    async fn an_unavailable_auto_merge_and_unmet_requirements_stop_differently() {
        let mut state = landing_state();
        let forge = Scripted::new(
            vec![seen("a", Checks::Green, "CLEAN", false)],
            vec![
                (false, UNAVAILABLE),
                (false, REFUSED),
                (false, UNAVAILABLE),
                (false, REFUSED),
            ],
        );
        land_with(&mut state, URL, &forge).await.unwrap();
        let why = state.merge.as_ref().unwrap().detail.clone();
        assert!(
            why.contains("not available")
                && why.contains("Auto merge is not allowed")
                && why.contains("policy prohibits")
                && !why.contains("requirements are not met"),
            "{why}"
        );
    }

    #[tokio::test]
    async fn an_armed_merge_that_never_happens_stops_naming_what_github_waits_for() {
        let mut state = landing_state();
        let mut waiting = seen("a", Checks::Pending, "BLOCKED", false);
        waiting.contexts = vec![
            CheckInfo {
                label: "build".to_owned(),
                verdict: Verdict::Pending,
                required: Some(true),
            },
            CheckInfo {
                label: "lint".to_owned(),
                verdict: Verdict::Fail,
                required: None,
            },
            CheckInfo {
                label: "docs".to_owned(),
                verdict: Verdict::Pending,
                required: Some(false),
            },
        ];
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("a", Checks::Green, "CLEAN", false),
                waiting,
            ],
            vec![(true, ""), (true, "")],
        );
        land_with(&mut state, URL, &forge).await.unwrap();
        assert_eq!(state.status, RunStatus::Blocked);
        let why = state.merge.as_ref().unwrap().detail.clone();
        assert!(
            why.contains("BLOCKED")
                && why.contains("build (pending)")
                && why.contains("could not be read")
                && why.contains("lint (failed)")
                && !why.contains("docs"),
            "{why}"
        );
        assert!(has(forge.argvs().last().unwrap(), "--disable-auto"));
        assert!(state.land_armed_head.is_none());
    }

    #[test]
    fn only_known_wordings_mean_auto_merge_is_unavailable() {
        assert!(automerge_unavailable(UNAVAILABLE));
        assert!(automerge_unavailable(CLEAN_REFUSAL));
        assert!(!automerge_unavailable(REFUSED));
        assert!(!automerge_unavailable("Head branch was modified"));
        assert!(!automerge_unavailable(""));
    }

    #[test]
    fn the_direct_merge_guard_needs_the_approved_head_bound_to_its_checks() {
        let shown = BTreeSet::new();
        let ok = seen("a", Checks::Green, "CLEAN", false);
        let guard = |s: Option<&Seen>| direct_merge_is_safe(s, "A", &shown, 0, 4, Duration::ZERO);
        assert!(guard(Some(&ok)));
        assert!(!guard(None));
        assert!(!guard(Some(&seen("b", Checks::Green, "CLEAN", false))));
        let mut stale = ok.clone();
        stale.rollup_head = "old".to_owned();
        assert!(!guard(Some(&stale)));
        assert!(!guard(Some(&seen("a", Checks::Pending, "BLOCKED", false))));
        assert!(!guard(Some(&merged_view("a"))));
    }

    #[test]
    fn the_rollup_node_carries_whether_each_check_is_required() {
        let node = node_json("new", "SUCCESS", false)
            .replace(r#""name":"ci","#, r#""name":"ci","isRequired":true,"#);
        let s = seen_from(&view_json("new"), Some(&node)).unwrap();
        assert_eq!(s.contexts.len(), 1);
        assert_eq!(s.contexts[0].required, Some(true));
        let s = seen_from(&view_json("new"), Some(&node_json("new", "SUCCESS", false))).unwrap();
        assert_eq!(s.contexts[0].required, None);
    }

    #[tokio::test]
    async fn a_resume_that_cannot_disable_a_recorded_arm_stops_and_keeps_the_record() {
        let mut state = landing_state();
        state.land_armed_head = Some("a".to_owned());
        let forge = Scripted::new(
            vec![seen("a", Checks::Green, "CLEAN", true)],
            vec![(false, "disable exploded")],
        );
        land_with(&mut state, URL, &forge).await.unwrap();
        assert!(!forge.calls().contains(&"fix"));
        assert_eq!(state.status, RunStatus::Blocked);
        assert_eq!(state.land_armed_head.as_deref(), Some("a"));
        let why = state.merge.as_ref().unwrap().detail.clone();
        assert!(why.contains("disable exploded"), "{why}");
    }

    #[tokio::test]
    async fn an_immediately_mergeable_pull_request_is_not_armed_from_a_moved_head() {
        let mut state = landing_state();
        // The pre-arm read finds a different head: nothing is sent to the forge
        // for the stale one.
        let forge = Scripted::new(
            vec![
                seen("a", Checks::Green, "CLEAN", false),
                seen("b", Checks::Pending, "BLOCKED", false),
                merged_view("b"),
            ],
            vec![],
        );
        land_with(&mut state, URL, &forge).await.unwrap();
        assert!(forge.argvs().is_empty());
        assert_eq!(state.status, RunStatus::Merged);
    }
}
