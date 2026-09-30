//! Rebasing the winning branch onto a moved base, with a fixer for conflicts.
//!
//! Both `graph::Runner::sync_to_base` (before review) and `land::Step::Rebase`
//! (after the pull request exists) used to stop on a conflict and leave it to
//! a person. A conflict is often a chore - two `pub mod` lines on the same
//! spot - and what follows it (a rule the base gained meanwhile) is exactly
//! what the gate-fix round handles, so both callers now come through
//! [`rebase_with_fixer`], which hands the standing conflict to the fixer seat.
//!
//! The rules, none optional:
//!
//! - **magi resolves nothing itself.** No taking one side, no line merging:
//!   the agent decides. Everything here is scaffolding around that call.
//! - **The round is judged by what git says**, never by the fixer's report:
//!   the rebase is no longer in progress, nothing is unmerged, no conflict
//!   marker is left in a path that conflicted, and the base is an ancestor of
//!   the result (an agent that ran `git rebase --abort` leaves a tidy tree
//!   that contains no base at all). Whether the tree *builds* is left to the
//!   review and gate that follow, which already know how to fix a breakage.
//! - **The budget is `graph.review_rounds`, counted in
//!   `RunState::rebase_fixes`** and saved before the fixer is called, shared
//!   by both callers, so a park or a crash cannot hand a round back. It
//!   touches neither the task's attempts nor land's own rebase budget.
//! - **A failure restores the branch.** Whatever the fixer left, the branch
//!   ref goes back to where it was and the throwaway worktree is removed, so
//!   the fallback is exactly the old "conflict, a person decides".

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use jiff::Timestamp;

use crate::agent::{self, Invocation};
use crate::git;
use crate::land::seat_of;
use crate::prompt;
use crate::run::{QuotaLoss, RebaseFixRecord, RunState};

/// How [`rebase_with_fixer`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebased {
    /// The branch now sits on top of the base (rebased cleanly or resolved by
    /// the fixer); the throwaway worktree is gone.
    Applied,
    /// It did not apply. The branch is untouched, the worktree is gone, and
    /// the string says what was tried: rounds spent, paths still conflicted,
    /// and what git said.
    Stopped(String),
}

/// Longest conflict excerpt shown to the fixer, per file and in total.
const HUNK_PER_FILE: usize = 4_000;
const HUNK_TOTAL: usize = 16_000;
/// Commit subjects listed per side.
const SUBJECTS: usize = 20;
/// Paths named in a failure reason before "and N more".
const PATHS_IN_REASON: usize = 8;

/// Rebase `branch` onto `onto` in `scratch`, giving a conflict to the fixer.
///
/// Re-entrant: when `scratch` already holds a rebase in progress (a run that
/// died in the middle of a round) it carries on from there instead of
/// starting over, and the rounds already recorded still count.
pub async fn rebase_with_fixer(
    state: &mut RunState,
    scratch: &Path,
    branch: &str,
    onto: &str,
) -> Result<Rebased> {
    let repo = state.repo.clone();
    let cap = state.config.graph.review_rounds;
    let orig = git::rev_parse(&repo, &format!("refs/heads/{branch}")).await?;

    let mut said = String::new();
    if !git::rebase_in_progress(scratch).await {
        match git::rebase_start(&repo, scratch, branch, onto).await? {
            git::RebaseStart::Applied => return Ok(Rebased::Applied),
            git::RebaseStart::Failed(why) => return Ok(Rebased::Stopped(why)),
            git::RebaseStart::Conflicted(why) => said = why,
        }
    }
    let onto_sha = git::rev_parse(&repo, onto).await?;
    // Every path that was ever unmerged: the marker check on the finished
    // tree looks at exactly these.
    let mut touched: Vec<String> = Vec::new();

    loop {
        if !git::rebase_in_progress(scratch).await {
            return finish(state, scratch, branch, &orig, &onto_sha, &touched, &said).await;
        }
        let paths = git::unmerged_paths(scratch).await.unwrap_or_default();
        for p in &paths {
            if !touched.contains(p) {
                touched.push(p.clone());
            }
        }
        let spent = state.rebase_fixes.len();
        if spent >= cap {
            let why = reason(spent, cap, &paths, &said, "the rounds are spent");
            abandon(&repo, scratch, branch, &orig).await;
            return Ok(Rebased::Stopped(why));
        }

        let winner = state
            .winner()
            .cloned()
            .context("resolving a rebase conflict needs a winning candidate")?;
        let roles = state
            .config
            .resolve_roles()
            .context("resolve the roster for the rebase fix")?;
        let (spec, seat_key) = match &roles.fixer {
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

        let round = spent + 1;
        let branch_subjects = subjects(scratch, &format!("{onto}..{branch}")).await;
        let onto_subjects = subjects(scratch, &format!("{branch}..{onto}")).await;
        let hunks = hunks(scratch, &paths);
        let prompt_text = prompt::rebase_conflict(&prompt::RebaseConflict {
            instruction: &state.instruction,
            worktree: scratch,
            branch,
            onto,
            paths: &paths,
            branch_subjects: &branch_subjects,
            onto_subjects: &onto_subjects,
            hunks: &hunks,
            round,
            cap,
            language: &state.config.graph.language,
        });
        let prompt_text = if state.config.cache_dir().is_some() {
            format!("{prompt_text}\n\n{}", prompt::build_cache_note("fix", true))
        } else {
            prompt_text
        };

        // Spent before the call, on disk: a run killed mid-round must not
        // get the round back.
        state.rebase_fixes.push(RebaseFixRecord {
            agent: spec.id.clone(),
            paths: paths.clone(),
            finished: false,
            error: None,
        });
        state.event(
            "rebase",
            format!(
                "{branch} conflicts with {onto} ({} path(s)); fixer round {round} of {cap}",
                paths.len()
            ),
        );
        state.save()?;

        let mut seat = seat_of(state, &seat_key, &spec.id);
        let artifacts = agent::artifacts_dir(&state.dir());
        let out = agent::invoke(
            &spec,
            &mut seat,
            &Invocation {
                cwd: scratch,
                prompt: &prompt_text,
                timeout: Duration::from_secs(state.config.graph.timeout_fix),
                allow_write: true,
                sessions: state.config.graph.sessions,
                artifacts: &artifacts,
                stem: &format!("rebase-fix-{round}"),
                run: &state.id,
                node: "rebase",
                cache_dir: state.config.cache_dir().as_deref(),
                attachments: &[],
            },
        )
        .await;
        let seat_name = seat.key.clone();
        state.seats.insert(seat.key.clone(), seat);

        let mut error = None;
        let mut quota = false;
        match out {
            Ok(o) if o.quota_exhausted() => {
                state.quota.push(QuotaLoss {
                    seat: seat_name,
                    node: "rebase".to_owned(),
                    at: Timestamp::now(),
                    reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                });
                error = Some("rate limited (quota); the fixer could not run".to_owned());
                quota = true;
            }
            Ok(o) if !o.usable() => {
                error = Some(format!(
                    "the fixer produced nothing usable (exit {:?}, timed out: {})",
                    o.exit_code, o.timed_out
                ));
            }
            Ok(_) => {}
            Err(e) => error = Some(format!("{e:#}")),
        }

        let finished = !git::rebase_in_progress(scratch).await;
        if let Some(r) = state.rebase_fixes.last_mut() {
            r.finished = finished;
            r.error = error.clone();
        }
        state.save()?;

        if quota {
            // A retry now fails the same way; do not burn the rest.
            let paths = git::unmerged_paths(scratch).await.unwrap_or_default();
            let why = reason(
                state.rebase_fixes.len(),
                cap,
                &paths,
                &said,
                "the fixer hit its rate limit",
            );
            abandon(&repo, scratch, branch, &orig).await;
            return Ok(Rebased::Stopped(why));
        }
    }
}

/// Judge a rebase that is no longer in progress by what git says.
async fn finish(
    state: &mut RunState,
    scratch: &Path,
    branch: &str,
    orig: &str,
    onto_sha: &str,
    touched: &[String],
    said: &str,
) -> Result<Rebased> {
    let repo = state.repo.clone();
    let spent = state.rebase_fixes.len();
    let cap = state.config.graph.review_rounds;
    let unmerged = git::unmerged_paths(scratch).await.unwrap_or_default();
    let head = git::rev_parse(scratch, "HEAD").await.unwrap_or_default();
    let marked: Vec<String> = touched
        .iter()
        .filter(|p| has_markers(scratch, p))
        .cloned()
        .collect();

    let problem = if !unmerged.is_empty() {
        Some(("paths are still unmerged", unmerged))
    } else if !marked.is_empty() {
        Some(("conflict markers were left in the tree", marked))
    } else if head.is_empty() || !git::is_ancestor(&repo, onto_sha, &head).await {
        Some((
            "the rebase ended without the base in the result (abandoned or skipped)",
            touched.to_vec(),
        ))
    } else {
        None
    };
    match problem {
        None => {
            git::worktree_remove(&repo, scratch).await.ok();
            state.event(
                "rebase",
                format!("{branch} rebased after {spent} fixer round(s)"),
            );
            state.save()?;
            Ok(Rebased::Applied)
        }
        Some((what, paths)) => {
            let why = reason(spent, cap, &paths, said, what);
            abandon(&repo, scratch, branch, orig).await;
            Ok(Rebased::Stopped(why))
        }
    }
}

/// Give up: abort whatever is standing, drop the worktree and put the branch
/// ref back where it was, whatever a fixer did to it.
async fn abandon(repo: &Path, scratch: &Path, branch: &str, orig: &str) {
    git::rebase_abort(repo, scratch).await;
    let full = format!("refs/heads/{branch}");
    if git::rev_parse(repo, &full).await.ok().as_deref() != Some(orig) {
        git::git_raw(repo, &["update-ref", &full, orig]).await.ok();
    }
}

/// The reason a caller records and the conductor quotes: it says what was
/// tried, so nobody has to open the run directory to find out.
fn reason(spent: usize, cap: usize, paths: &[String], said: &str, what: &str) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(PATHS_IN_REASON)
        .map(String::as_str)
        .collect();
    let mut list = shown.join(", ");
    if paths.len() > shown.len() {
        list.push_str(&format!(" and {} more", paths.len() - shown.len()));
    }
    if list.is_empty() {
        list.push_str("none recorded");
    }
    let mut s = format!(
        "conflict not resolved after {spent} of {cap} fixer round(s) ({what}); remaining \
         conflicted path(s): {list}"
    );
    let said = said.trim();
    if !said.is_empty() {
        s.push_str("; git said: ");
        s.extend(said.chars().take(250));
    }
    s
}

/// Commit subjects in `range`, newest first, capped.
async fn subjects(worktree: &Path, range: &str) -> Vec<String> {
    let n = format!("-n{SUBJECTS}");
    git::git(worktree, &["log", "--format=%s", &n, range])
        .await
        .map(|o| o.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn has_markers(worktree: &Path, path: &str) -> bool {
    std::fs::read_to_string(worktree.join(path)).is_ok_and(|t| {
        t.lines().any(|l| l.starts_with("<<<<<<< ")) && t.lines().any(|l| l.starts_with(">>>>>>> "))
    })
}

/// The conflicted regions of `paths`, marker lines included, truncated. This
/// only *shows* the conflict; nothing here decides how to resolve it.
fn hunks(worktree: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for p in paths {
        if out.len() >= HUNK_TOTAL {
            out.push_str("\n(more conflicted files omitted)\n");
            break;
        }
        out.push_str(&format!("=== {p} ===\n"));
        let Ok(text) = std::fs::read_to_string(worktree.join(p)) else {
            out.push_str("(not readable as text; use git to inspect it)\n");
            continue;
        };
        let mut file = String::new();
        let mut inside = false;
        for line in text.lines() {
            if line.starts_with("<<<<<<< ") {
                inside = true;
            }
            if inside {
                file.push_str(line);
                file.push('\n');
            }
            if line.starts_with(">>>>>>> ") {
                inside = false;
            }
        }
        if file.len() > HUNK_PER_FILE {
            file = file.chars().take(HUNK_PER_FILE).collect();
            file.push_str("\n(truncated)\n");
        }
        out.push_str(&file);
    }
    out
}
