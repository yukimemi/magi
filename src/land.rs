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

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::agent::{self, Invocation, SeatState};
use crate::ask;
use crate::config::{AgentSpec, MergeMode};
use crate::git;
use crate::run::{MergeOutcome, RunState, RunStatus, tail};

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
}

/// What the loop decided to do next. Pure, so the policy is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Checks are still running; re-read the pull request after [`POLL`].
    Wait,
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
fn merged_after_all(
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
    fn approval_detail(&self, url: &str, base: &str, subject: &str) -> String {
        if self.html_lang == "ja" {
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
        }
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
    let l = language.trim();
    if l.eq_ignore_ascii_case("ja")
        || l.eq_ignore_ascii_case("jp")
        || l.eq_ignore_ascii_case("japanese")
        || l.eq_ignore_ascii_case("日本語")
    {
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

/// Ask the owner before merging, with the whole case attached as a panel.
///
/// The evidence is gathered from the winner's own worktree with the `git` CLI,
/// never from the network, so a phone on a slow link gets the diff magi is
/// looking at rather than a link it has to go and open.
async fn request_approval(state: &mut RunState, pr: &PrState, subject: &str) -> Result<Approval> {
    let (worktree, head) = match state.winner() {
        Some(w) => (w.worktree.clone(), w.branch.clone()),
        None => (state.repo.clone(), "HEAD".to_owned()),
    };
    let base = state.base_branch.clone();
    let range = format!("{base}...{head}");
    // A failed `git` must not decide the merge: the panel degrades to less
    // evidence and the owner still chooses. Merging because the diff could not
    // be read would be the worst of both.
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
    let store = ask::Questions::open();
    let mut q = ask::Question::new(
        state.id.clone(),
        APPROVAL_NODE.to_owned(),
        "land".to_owned(),
        w.approval_summary(pr.number, subject),
        w.approval_detail(&pr.url, &base, subject),
        vec![APPROVE.to_owned(), HOLD.to_owned()],
    );
    store
        .put_panel(&mut q, &html, &[])
        .context("write the merge approval panel")?;
    state.event("land", format!("asking for merge approval ({})", q.short()));
    state.save()?;

    let timeout = Duration::from_secs(state.config.graph.answer_timeout);
    let said = ask::ask_and_wait(&mut q, &store, &state.config.notify, timeout).await?;
    Ok(approval(said.as_deref()))
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

    let mut failing = Vec::new();
    let mut pending = false;
    let mut unknown = false;
    for check in &raw.status_check_rollup {
        match check.verdict() {
            Verdict::Pass => {}
            Verdict::Pending => pending = true,
            Verdict::Fail => failing.push(check.label()),
            Verdict::Unknown => unknown = true,
        }
    }
    let checks = if raw.status_check_rollup.is_empty() {
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
    })
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

/// Run the loop against a real pull request until it merges or the budget runs
/// out.
///
/// The caller decides whether landing happens at all: this is only reached when
/// `graph.land` is on. Returns the last observation, so the caller can report
/// what magi was looking at when it stopped.
pub async fn land(state: &mut RunState, pr_url: &str) -> Result<PrState> {
    let repo = state.repo.clone();
    let budget = state.config.graph.land_rounds;
    let mut round = 0usize;
    let mut waited = Duration::ZERO;
    // Comment bodies the fixer has already been shown. A comment is
    // outstanding until it has been handed over once; after that it is a
    // recorded decision, not an open question, and re-feeding it would loop the
    // budget away on a comment the fixer already declined with an argument.
    let mut shown: BTreeSet<String> = BTreeSet::new();

    state.event("land", format!("watching {pr_url}"));
    state.save()?;

    loop {
        let seen = observe(&repo, pr_url).await?;
        let mut pr = seen.pr;
        pr.review_comments.retain(|c| !shown.contains(&c.body));
        state.pr = Some(crate::run::PrRecord {
            url: pr.url.clone(),
            number: pr.number,
            state: pr.state.as_str().to_owned(),
            checks: pr.checks.as_str().to_owned(),
            round,
            rounds: budget,
        });
        state.save()?;

        match decide(&pr, round, budget, waited) {
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
                tokio::time::sleep(POLL).await;
            }
            Step::Done { merged } => {
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
                });
                state.event("land", detail);
                state.save()?;
                return Ok(pr);
            }
            Step::Merge => {
                let subject = merge_subject(&seen.title, &state.instruction);
                // The owner sees the panel before the one irreversible step,
                // and an unanswered question is a hold: silence never merges.
                if state.config.graph.land_approval
                    && request_approval(state, &pr, &subject).await? == Approval::Hold
                {
                    stop(
                        state,
                        &repo,
                        &pr,
                        "the owner did not approve the merge (held or unanswered)",
                    )
                    .await?;
                    return Ok(pr);
                }
                let argv = merge_argv(pr.number, &subject);
                let out = gh(&repo, &argv).await?;
                if out.0 {
                    state.status = RunStatus::Merged;
                    state.merge = Some(MergeOutcome {
                        mode: MergeMode::Pr,
                        ok: true,
                        detail: format!("gh {}", argv.join(" ")),
                    });
                    state.event("land", format!("merged {} as `{subject}`", pr.url));
                    state.save()?;
                    pr.state = PrLifecycle::Merged;
                    return Ok(pr);
                }
                let after = observe(&repo, pr_url).await.ok().map(|s| s.pr.state);
                if let Some(outcome) = merged_after_all(&argv, &out.1, after) {
                    state.status = RunStatus::Merged;
                    state.merge = Some(outcome);
                    state.event("land", format!("merged {} as `{subject}`", pr.url));
                    state.save()?;
                    pr.state = PrLifecycle::Merged;
                    return Ok(pr);
                }
                stop(
                    state,
                    &repo,
                    &pr,
                    &format!("`gh pr merge` failed: {}", out.1),
                )
                .await?;
                return Ok(pr);
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
                match fix_round(state, &pr, round, budget, &reason, &logs).await? {
                    Fixed::Committed => {}
                    Fixed::Declined if was_red => {
                        let why = format!(
                            "the fixer produced no commit while {} check(s) were failing; \
                             stopping instead of looping on an unchanged tree",
                            pr.failing.len()
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
struct Seen {
    pr: PrState,
    title: String,
    failing_urls: Vec<(String, String)>,
}

/// Read the pull request: `gh pr view` for the rollup and the top-level thread,
/// `gh api` for the inline review comments `gh pr view` does not report.
async fn observe(repo: &Path, pr_url: &str) -> Result<Seen> {
    let view = gh(
        repo,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            pr_url.to_owned(),
            "--json".to_owned(),
            "url,number,state,title,statusCheckRollup,reviews,comments".to_owned(),
        ],
    )
    .await?;
    if !view.0 {
        bail!("gh pr view {pr_url}: {}", view.1);
    }
    let mut pr = parse_pr(&view.1)?;
    let raw: GhPr = serde_json::from_str(&view.1).context("re-read pull request json")?;

    let inline = gh(
        repo,
        &[
            "api".to_owned(),
            format!("repos/{{owner}}/{{repo}}/pulls/{}/comments", pr.number),
        ],
    )
    .await?;
    if inline.0 {
        match parse_inline_comments(&inline.1) {
            Ok(mut comments) => pr.review_comments.append(&mut comments),
            // An unreadable inline thread must not end a landing: the rollup
            // and the top-level thread are still real signal.
            Err(e) => tracing::warn!("inline review comments unreadable: {e}"),
        }
    } else {
        tracing::warn!("gh api pulls/{}/comments: {}", pr.number, inline.1);
    }

    let failing_urls = raw
        .status_check_rollup
        .iter()
        .filter(|c| c.verdict() == Verdict::Fail)
        .filter_map(|c| c.url().map(|u| (c.label(), u.to_owned())))
        .collect();

    Ok(Seen {
        pr,
        title: raw.title,
        failing_urls,
    })
}

/// What a fix round did.
enum Fixed {
    /// The fixer committed something.
    Committed,
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
async fn fix_round(
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

    let before = git::rev_parse(&winner.worktree, "HEAD").await?;
    // An agent that edited files but never committed would otherwise push
    // nothing and look like a refusal.
    git::commit_all(
        &winner.worktree,
        &format!("magi: land round {round} fixes (uncommitted work)"),
    )
    .await
    .ok();
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
    Ok(Fixed::Committed)
}

/// Fetch or create a seat, keeping its conversation across nodes.
fn seat_of(state: &mut RunState, key: &str, agent: &str) -> SeatState {
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

/// Leave the pull request open, say why on it, and mark the run blocked.
///
/// The comment is what makes an unattended stop actionable: the operator wakes
/// up to a pull request that explains itself rather than to a silent queue.
async fn stop(state: &mut RunState, repo: &Path, pr: &PrState, why: &str) -> Result<()> {
    let body = format!(
        "{MARKER}\nmagi stopped landing this pull request: {why}\n\n\
         The branch is untouched and the run is `{}`. Nothing was merged.",
        state.id
    );
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
    });
    state.event("land", format!("stopped: {why}"));
    state.save()?;
    Ok(())
}

/// Run `gh` in `repo`, returning success and the combined output.
///
/// Combined because `gh` reports a refused merge on stderr and the pull request
/// json on stdout, and both are evidence.
async fn gh(cwd: &Path, args: &[String]) -> Result<(bool, String)> {
    let out = tokio::process::Command::new("gh")
        .args(args)
        .current_dir(cwd)
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

    /// Real `gh pr view` output for the open pull request #10 (Renovate's apm bump), trimmed to four checks and its one comment. Every check passed or was skipped by the review workflow, and the only comment is CodeRabbit's trigger notice.
    const GREEN_OPEN: &str = r####"{
  "url": "https://github.com/yukimemi/magi/pull/10",
  "number": 10,
  "state": "OPEN",
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
        match decide(&state, 0, 4, Duration::ZERO) {
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
    fn a_merge_command_that_failed_after_merging_is_still_a_merge() {
        let argv = merge_argv(28, "Merge magi run ec12 (candidate B)");
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
            failing: Vec::new(),
            review_comments: vec![ReviewComment {
                author: "coderabbitai".to_owned(),
                path: Some("src/land.rs".to_owned()),
                line: Some(212),
                body: "this branch never checks the exit code".to_owned(),
            }],
        }
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
            w.approval_detail("http://x/1", "main", "feat: x")
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
}
