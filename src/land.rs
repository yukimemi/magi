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
pub fn decide(pr: &PrState, round: usize, budget: usize) -> Step {
    match pr.state {
        PrLifecycle::Merged => return Step::Done { merged: true },
        PrLifecycle::Closed => return Step::Done { merged: false },
        PrLifecycle::Open => {}
    }

    let spent = round >= budget;
    match pr.checks {
        Checks::Pending => Step::Wait,
        Checks::Unknown => Step::GiveUp {
            reason: "no check status is readable on the pull request; refusing to merge on a guess"
                .to_owned(),
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

        match decide(&pr, round, budget) {
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
        assert_eq!(decide(&state, 0, 4), Step::Merge);
    }

    #[test]
    fn a_failing_check_parses_as_red_and_is_named() {
        let state = parse_pr(RED_OPEN).expect("red fixture parses");
        assert_eq!(state.checks, Checks::Red);
        assert_eq!(state.failing, vec!["editorconfig".to_owned()]);
        match decide(&state, 0, 4) {
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
        assert_eq!(decide(&state, 0, 4), Step::Wait);
    }

    #[test]
    fn a_pull_request_merged_underneath_us_is_done_rather_than_a_failure() {
        let state = parse_pr(MERGED).expect("merged fixture parses");
        assert_eq!(state.state, PrLifecycle::Merged);
        assert_eq!(decide(&state, 0, 4), Step::Done { merged: true });
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
        match decide(&state, 0, 4) {
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
        assert_eq!(decide(&clean, 0, 4), Step::Merge);

        let mut found = pr(Checks::Green, &[], 0);
        found.review_comments.push(ReviewComment {
            author: "claude".to_owned(),
            path: None,
            line: None,
            body: CLAUDE_FINDING.to_owned(),
        });
        found.review_comments.retain(|c| !is_noise(&c.body));
        assert!(matches!(decide(&found, 0, 4), Step::Fix { .. }));
    }

    #[test]
    fn the_policy_table_holds_for_every_combination_that_matters() {
        let cases: Vec<(&str, PrState, usize, usize, Step)> = vec![
            (
                "pending checks are waited for, even on the last round",
                pr(Checks::Pending, &[], 0),
                4,
                4,
                Step::Wait,
            ),
            (
                "red checks are fixed",
                pr(Checks::Red, &["editorconfig"], 0),
                0,
                4,
                Step::Fix {
                    reason: "1 check(s) failing: editorconfig".to_owned(),
                },
            ),
            (
                "green with comments is fixed, not merged",
                pr(Checks::Green, &[], 2),
                1,
                4,
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
                Step::Merge,
            ),
            (
                "an unreadable rollup is never merged",
                pr(Checks::Unknown, &[], 0),
                0,
                4,
                Step::GiveUp {
                    reason: "no check status is readable on the pull request; refusing to merge \
                             on a guess"
                        .to_owned(),
                },
            ),
        ];
        for (what, state, round, budget, want) in cases {
            assert_eq!(decide(&state, round, budget), want, "{what}");
        }
    }

    #[test]
    fn a_pull_request_closed_underneath_us_is_done_and_not_merged() {
        let mut state = pr(Checks::Red, &["editorconfig"], 3);
        state.state = PrLifecycle::Closed;
        assert_eq!(
            decide(&state, 0, 4),
            Step::Done { merged: false },
            "a human closing the pull request ends the loop, whatever CI says"
        );
    }

    #[test]
    fn the_last_round_gives_up_with_a_reason_naming_what_is_still_failing() {
        let red = decide(&pr(Checks::Red, &["editorconfig", "test (macos)"], 0), 4, 4);
        match red {
            Step::GiveUp { reason } => {
                assert!(reason.contains("editorconfig"), "reason: {reason}");
                assert!(reason.contains("test (macos)"), "reason: {reason}");
                assert!(reason.contains("4 fix round(s)"), "reason: {reason}");
            }
            other => panic!("expected a give-up, got {other:?}"),
        }

        let commented = decide(&pr(Checks::Green, &[], 1), 2, 2);
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
}
