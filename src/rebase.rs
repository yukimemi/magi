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
            from: Some(orig.clone()),
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
                writable: &[],
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
    // Not only the paths seen unmerged at a round's start: one round may
    // continue through several commits, and a later commit's conflict never
    // shows up at a loop head. Everything the result changes relative to the
    // base is checked.
    let mut candidates: Vec<String> = touched.to_vec();
    if let Ok(changed) = git::git(scratch, &["diff", "--name-only", onto_sha, "HEAD"]).await {
        for p in changed.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !candidates.iter().any(|c| c == p) {
                candidates.push(p.to_owned());
            }
        }
    }
    let marked: Vec<String> = candidates
        .into_iter()
        .filter(|p| has_markers(scratch, p))
        .collect();

    // Skipping every conflicting commit leaves the branch equal to the base:
    // the ancestry test below passes and the branch's work is gone. Empty is
    // only acceptable when every original commit already has a patch twin on
    // the base.
    let emptied = head == onto_sha
        && git::cherry(&repo, onto_sha, orig)
            .await
            .map_or(true, |(unmatched, _)| !unmatched.is_empty());

    // A fixer that ran `git rebase --skip` on one of several commits lets the
    // rest apply and the rebase finish; `emptied` only sees the whole change
    // vanishing. Find the commits that are neither in the result nor already
    // on the base.
    let dropped = if unmerged.is_empty() && marked.is_empty() && !emptied && !head.is_empty() {
        dropped_commits(&repo, onto_sha, orig, &head).await
    } else {
        Vec::new()
    };

    let problem = if !unmerged.is_empty() {
        Some(("paths are still unmerged", unmerged))
    } else if !marked.is_empty() {
        Some(("conflict markers were left in the tree", marked))
    } else if emptied {
        Some((
            "the rebase ended with none of the branch's commits applied (all skipped)",
            touched.to_vec(),
        ))
    } else if !dropped.is_empty() {
        Some((
            "the rebase dropped some of the branch's commits (skipped?)",
            dropped,
        ))
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

/// Subjects of the commits `orig` had over `onto_sha` that the rebased `head`
/// no longer represents. Empty when all survive or the check could not run
/// (the other checks still apply).
///
/// Commits are matched by what a rebase preserves (author, author date,
/// subject), not by patch-id: a commit the fixer resolved has a new patch-id
/// by design. A commit whose patch already exists on the base is not expected
/// in the result, and one with a patch twin in the result is verified by content like any other candidate.
async fn dropped_commits(repo: &Path, onto_sha: &str, orig: &str, head: &str) -> Vec<String> {
    let Ok((unmatched, _)) = git::cherry(repo, onto_sha, orig).await else {
        return Vec::new();
    };
    if unmatched.is_empty() {
        return Vec::new();
    }
    let (Ok(origin), Ok(result)) = (
        git::commit_keys(repo, &format!("{onto_sha}..{orig}")).await,
        git::commit_keys(repo, &format!("{onto_sha}..{head}")).await,
    ) else {
        return Vec::new();
    };
    let expected: Vec<git::CommitKey> = origin
        .into_iter()
        .filter(|c| unmatched.contains(&c.sha))
        .collect();
    let have: Vec<String> = result.into_iter().map(|c| c.key).collect();
    let mut lost = Vec::new();
    for c in missing_commits(&expected, &have) {
        // Git also drops a commit on its own when its change is already on
        // the base under a different patch (e.g. folded into one upstream
        // commit). That is not a loss: every path it touched holds the same
        // content in the result.
        if !already_in_result(repo, &c.sha, orig, head).await {
            lost.push(c.key.rsplit('\u{1f}').next().unwrap_or(&c.key).to_owned());
        }
    }
    lost
}

/// `mode type blob` of `path` at `rev`, or `None` when it does not exist
/// there. Unlike `rev-parse rev:path` this carries the file mode.
async fn entry(repo: &Path, rev: &str, path: &str) -> Option<String> {
    let out = git::git(repo, &["ls-tree", rev, "--", path]).await.ok()?;
    out.split('\t')
        .next()
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
}

/// Is `sha`'s change represented in `head`?
///
/// First by merging: replaying `sha` onto `head` changes nothing when its
/// change is already there, in whatever company (extra upstream edits in the
/// same file included). Failing that, per touched path: `head` holds what
/// `sha` left there, or what the branch's final tip `orig` has there (a later
/// commit may have changed the path again). Mode is part of that comparison,
/// and paths come NUL-separated so a quoted name is never misread.
async fn already_in_result(repo: &Path, sha: &str, orig: &str, head: &str) -> bool {
    if replay_is_noop(repo, sha, head).await {
        return true;
    }
    let Ok(paths) = git::git(
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            "-z",
            sha,
        ],
    )
    .await
    else {
        return false;
    };
    for p in paths.split('\0').filter(|l| !l.is_empty()) {
        let got = entry(repo, head, p).await;
        if got != entry(repo, sha, p).await && got != entry(repo, orig, p).await {
            return false;
        }
    }
    true
}

/// Does merging `sha` (against its parent) into `head` cleanly yield `head`'s
/// own tree? False when it conflicts, changes anything, or git cannot say.
async fn replay_is_noop(repo: &Path, sha: &str, head: &str) -> bool {
    let base = format!("{sha}^");
    let Ok(out) = git::git_raw(
        repo,
        &[
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={base}"),
            head,
            sha,
        ],
    )
    .await
    else {
        return false;
    };
    if !out.ok() {
        return false;
    }
    let merged = out.stdout.lines().next().unwrap_or("").trim();
    match git::tree_of(repo, head).await {
        Ok(t) => !merged.is_empty() && merged == t,
        Err(_) => false,
    }
}

/// The pure half of [`dropped_commits`]: the `expected` commits whose
/// key is not left in `have` (a multiset: each result commit covers one
/// expected commit). A patch twin in the result is deliberately not excused
/// here: with duplicate keys it could mask a skipped commit, so the caller
/// verifies each candidate by content ([`already_in_result`]).
fn missing_commits(expected: &[git::CommitKey], have: &[String]) -> Vec<git::CommitKey> {
    let mut pool: Vec<Option<&String>> = have.iter().map(Some).collect();
    let mut lost = Vec::new();
    for c in expected {
        if let Some(slot) = pool.iter_mut().find(|s| **s == Some(&c.key)) {
            *slot = None;
        } else {
            lost.push(c.clone());
        }
    }
    lost
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ck(sha: &str, subject: &str) -> git::CommitKey {
        git::CommitKey {
            sha: sha.to_owned(),
            key: format!("n\u{1f}e\u{1f}1 +0000\u{1f}{subject}"),
        }
    }

    #[test]
    fn nothing_is_missing_when_every_key_is_present() {
        let exp = [ck("a", "one"), ck("b", "two")];
        let have = vec![exp[1].key.clone(), exp[0].key.clone()];
        assert!(missing_commits(&exp, &have).is_empty());
    }

    #[test]
    fn a_dropped_commit_is_named_by_subject() {
        let exp = [ck("a", "one"), ck("b", "two")];
        let have = vec![exp[1].key.clone()];
        assert_eq!(missing_commits(&exp, &have), vec![exp[0].clone()]);
    }

    #[test]
    fn duplicate_keys_are_counted_not_collapsed() {
        let exp = [ck("a", "same"), ck("b", "same")];
        let have = vec![exp[0].key.clone()];
        assert_eq!(missing_commits(&exp, &have), vec![exp[1].clone()]);
    }

    #[test]
    fn a_commit_without_a_key_match_is_a_candidate_even_if_a_twin_exists() {
        let exp = [ck("a", "same"), ck("b", "same")];
        let have = vec![exp[1].key.clone()];
        assert_eq!(missing_commits(&exp, &have).len(), 1);
    }

    fn sh(dir: &Path, args: &[&str]) {
        let o = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    #[tokio::test]
    async fn a_lost_mode_change_is_not_already_in_the_result() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        sh(d, &["config", "core.fileMode", "true"]);
        std::fs::write(d.join("script.sh"), "echo\n").unwrap();
        sh(d, &["add", "-A"]);
        sh(d, &["commit", "-q", "-m", "base"]);
        sh(d, &["update-index", "--chmod=+x", "script.sh"]);
        sh(d, &["commit", "-q", "-m", "chmod"]);
        let sha = git::git(d, &["rev-parse", "HEAD"]).await.unwrap();
        let base = git::git(d, &["rev-parse", "HEAD~1"]).await.unwrap();
        // The result is the base: same blob, lost mode.
        assert!(!already_in_result(d, &sha, &sha, &base).await);
        assert!(already_in_result(d, &sha, &sha, &sha).await);
    }

    async fn commit(d: &Path, msg: &str) -> String {
        sh(d, &["add", "-A"]);
        sh(d, &["commit", "-q", "-m", msg]);
        git::git(d, &["rev-parse", "HEAD"]).await.unwrap()
    }

    #[tokio::test]
    async fn a_change_inside_a_larger_upstream_edit_is_in_the_result() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        let body = "old\n1\n2\n3\n4\n5\n6\n7\n8\n9\nend\n";
        std::fs::write(d.join("f.txt"), body).unwrap();
        commit(d, "base").await;
        std::fs::write(d.join("f.txt"), body.replacen("old", "new", 1)).unwrap();
        let c = commit(d, "c1").await;
        sh(d, &["checkout", "-q", "-b", "up", "HEAD~1"]);
        let up = body.replacen("old", "new", 1).replace("end", "end plus");
        std::fs::write(d.join("f.txt"), up).unwrap();
        let head = commit(d, "upstream").await;
        assert!(already_in_result(d, &c, &c, &head).await);
    }

    #[tokio::test]
    async fn a_dropped_commit_on_a_non_ascii_path_is_still_noticed() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        let base = commit(d, "base").await;
        std::fs::write(d.join("日本語.txt"), "x\n").unwrap();
        let c = commit(d, "c").await;
        assert!(!already_in_result(d, &c, &c, &base).await);
    }
}
