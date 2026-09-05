//! Git plumbing.
//!
//! magi drives the `git` CLI rather than linking a library: every operation it
//! needs is a one-liner, and shelling out keeps the behaviour identical to what
//! the operator sees when they inspect a run by hand.
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::proc::Quiet as _;
use anyhow::{Context as _, Result, bail};
use tokio::process::Command;

/// Output of a completed `git` invocation.
#[derive(Debug)]
pub struct GitOut {
    /// Exit status code, if the process was not killed by a signal.
    pub code: Option<i32>,
    /// Captured stdout, trailing newline trimmed.
    pub stdout: String,
    /// Captured stderr, trailing newline trimmed.
    pub stderr: String,
}

impl GitOut {
    /// Did the command succeed?
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `git` in `cwd` with `args`, returning the captured output regardless of
/// exit status.
pub async fn git_raw(cwd: &Path, args: &[&str]) -> Result<GitOut> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .quiet()
        // A hook that opens an editor or a credential prompt would hang a
        // headless run forever.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    Ok(GitOut {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
    })
}

/// Run `git`, failing on a non-zero exit status.
pub async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = git_raw(cwd, args).await?;
    if !out.ok() {
        bail!(
            "git {} failed in {} (exit {:?}): {}",
            args.join(" "),
            cwd.display(),
            out.code,
            if out.stderr.is_empty() {
                out.stdout.as_str()
            } else {
                out.stderr.as_str()
            }
        );
    }
    Ok(out.stdout)
}

/// Absolute path to the top level of the working tree containing `path`.
pub async fn toplevel(path: &Path) -> Result<PathBuf> {
    let out = git(path, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(out))
}

/// Resolve a revision to a full object id.
pub async fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    git(repo, &["rev-parse", rev]).await
}

/// Currently checked-out branch, or `None` when detached.
pub async fn current_branch(repo: &Path) -> Result<Option<String>> {
    let out = git_raw(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    Ok(if out.ok() && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    })
}

/// Is the working tree free of tracked modifications and untracked files?
pub async fn is_clean(repo: &Path) -> Result<bool> {
    Ok(git(repo, &["status", "--porcelain"]).await?.is_empty())
}

/// `git status --porcelain`, for reporting what is dirty.
pub async fn status_porcelain(repo: &Path) -> Result<String> {
    git(repo, &["status", "--porcelain"]).await
}

/// Create a worktree at `path` with a fresh branch `branch` starting at `base`.
pub async fn worktree_add_branch(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let path_s = path.to_string_lossy().to_string();
    git(repo, &["worktree", "add", "-b", branch, &path_s, base])
        .await
        .map(|_| ())
}

/// Create a worktree at `path` with a detached HEAD at `rev`.
pub async fn worktree_add_detached(repo: &Path, path: &Path, rev: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let path_s = path.to_string_lossy().to_string();
    git(repo, &["worktree", "add", "--detach", &path_s, rev])
        .await
        .map(|_| ())
}

/// Move an existing detached worktree to `rev`, discarding local state.
pub async fn reset_detached(worktree: &Path, rev: &str) -> Result<()> {
    git(worktree, &["checkout", "--detach", rev]).await?;
    git(worktree, &["reset", "--hard", rev]).await?;
    git(worktree, &["clean", "-fdx"]).await?;
    Ok(())
}

/// Remove a worktree. Returns `Ok(false)` when git refused (e.g. the path is
/// already gone), so callers can keep folding the rest of a run.
pub async fn worktree_remove(repo: &Path, path: &Path) -> Result<bool> {
    let path_s = path.to_string_lossy().to_string();
    let out = git_raw(repo, &["worktree", "remove", "--force", &path_s]).await?;
    if out.ok() {
        return Ok(true);
    }
    // A worktree whose directory was deleted by hand only needs pruning.
    git_raw(repo, &["worktree", "prune"]).await?;
    Ok(false)
}

/// Delete a branch, ignoring "not found".
pub async fn branch_delete(repo: &Path, branch: &str) -> Result<bool> {
    Ok(git_raw(repo, &["branch", "-D", branch]).await?.ok())
}

/// Does `branch` exist?
pub async fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    let refname = format!("refs/heads/{branch}");
    Ok(
        git_raw(repo, &["show-ref", "--verify", "--quiet", &refname])
            .await?
            .ok(),
    )
}

/// Patch of `head` against the merge base with `base`.
pub async fn diff(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let range = format!("{base}...{head}");
    git(
        worktree,
        &["diff", "--no-color", "--no-ext-diff", "-M", &range],
    )
    .await
}

/// `--stat` summary of `base...head`.
pub async fn diff_stat(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let range = format!("{base}...{head}");
    git(worktree, &["diff", "--no-color", "--stat", &range]).await
}

/// Number of files touched by `base...head`.
pub async fn changed_files(worktree: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let range = format!("{base}...{head}");
    let out = git(worktree, &["diff", "--name-only", &range]).await?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// One-line log of `base..head`, oldest first.
pub async fn log_oneline(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let range = format!("{base}..{head}");
    git(
        worktree,
        &["log", "--reverse", "--format=%s%n%b%n--", &range],
    )
    .await
}

/// How many commits `head` is ahead of `base`.
pub async fn commits_ahead(worktree: &Path, base: &str, head: &str) -> Result<usize> {
    let range = format!("{base}..{head}");
    let out = git(worktree, &["rev-list", "--count", &range]).await?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// Stage everything and commit under a neutral identity.
///
/// Used to rescue an agent that edited files but never committed: without this
/// its candidate would silently be empty. The neutral identity is part of the
/// blindness contract — a real `user.name` in a candidate's history would name
/// the operator, and an agent-configured one would name the vendor.
pub async fn commit_all(worktree: &Path, message: &str) -> Result<bool> {
    if git(worktree, &["status", "--porcelain"]).await?.is_empty() {
        return Ok(false);
    }
    git(worktree, &["add", "-A"]).await?;
    let out = git_raw(
        worktree,
        &[
            "-c",
            "user.name=magi candidate",
            "-c",
            "user.email=magi@localhost",
            "commit",
            "--no-verify",
            "-m",
            message,
        ],
    )
    .await?;
    if !out.ok() {
        bail!("rescue commit failed: {}", out.stderr);
    }
    Ok(true)
}

/// Enable `extensions.worktreeConfig` if it is not already on.
///
/// Returns `true` when magi turned it on, so the caller can turn it back off
/// during cleanup and leave the repo exactly as it found it.
pub async fn enable_worktree_config(repo: &Path) -> Result<bool> {
    let out = git_raw(repo, &["config", "--get", "extensions.worktreeConfig"]).await?;
    if out.ok() && out.stdout.trim() == "true" {
        return Ok(false);
    }
    git(repo, &["config", "extensions.worktreeConfig", "true"]).await?;
    Ok(true)
}

/// Undo [`enable_worktree_config`].
pub async fn disable_worktree_config(repo: &Path) -> Result<()> {
    git_raw(repo, &["config", "--unset", "extensions.worktreeConfig"]).await?;
    Ok(())
}

/// Point a single worktree at its own hooks directory.
///
/// `core.hooksPath` is normally repo-wide; scoping it with `--worktree` keeps
/// the operator's own hooks untouched in the primary worktree, and the setting
/// disappears together with the worktree.
pub async fn set_worktree_hooks_path(worktree: &Path, hooks_dir: &Path) -> Result<()> {
    let dir = hooks_dir.to_string_lossy().replace('\\', "/");
    git(worktree, &["config", "--worktree", "core.hooksPath", &dir])
        .await
        .map(|_| ())
}

/// Exclude a path from a worktree's status without touching `.gitignore`.
pub async fn local_exclude(worktree: &Path, pattern: &str) -> Result<()> {
    let git_dir = git(worktree, &["rev-parse", "--git-path", "info/exclude"]).await?;
    let path = worktree.join(git_dir);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut body = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    if body.lines().any(|l| l.trim() == pattern) {
        return Ok(());
    }
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(pattern);
    body.push('\n');
    tokio::fs::write(&path, body)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// `git merge --no-ff` of `branch` into the currently checked-out branch.
pub async fn merge_no_ff(repo: &Path, branch: &str, message: &str) -> Result<GitOut> {
    git_raw(
        repo,
        &["merge", "--no-ff", "--no-edit", "-m", message, branch],
    )
    .await
}

/// Push a branch to `remote`.
pub async fn push(repo: &Path, remote: &str, branch: &str) -> Result<GitOut> {
    git_raw(repo, &["push", "-u", remote, branch]).await
}

/// Force-push a branch that has been rewritten, refusing to clobber work
/// pushed since this side last looked.
///
/// `--force-with-lease` rather than `--force`: a rebase replaces the branch's
/// commits, so a plain push is rejected, but a blind force would also throw
/// away anything a person pushed to the same branch meanwhile. The lease
/// turns that case into a failure instead of a loss.
pub async fn push_rewritten(repo: &Path, remote: &str, branch: &str) -> Result<GitOut> {
    git_raw(repo, &["push", "--force-with-lease", remote, branch]).await
}

/// Rebase a branch onto `onto`, inside a throwaway worktree.
///
/// A worktree of its own for two reasons. The repository magi runs in may be
/// jj-colocated, where git `HEAD` is detached and a rebase in the primary
/// tree would move it under the operator; and a rebase that hits a conflict
/// leaves state behind, which is far easier to discard with the whole
/// directory than to unpick in a tree somebody is using.
///
/// `Ok(None)` means it applied and the branch now points at the rebased
/// commits. `Ok(Some(why))` means it did not: the branch is untouched, and
/// the string is what git said - a person has to decide.
pub async fn rebase_branch_in_temp(
    repo: &Path,
    scratch: &Path,
    branch: &str,
    onto: &str,
) -> Result<Option<String>> {
    // Removed first so a leftover from an interrupted attempt cannot make
    // `worktree add` fail on a path that already exists.
    worktree_remove(repo, scratch).await.ok();
    git_raw(
        repo,
        &[
            "worktree",
            "add",
            "--force",
            &scratch.to_string_lossy(),
            branch,
        ],
    )
    .await?;

    let out = git_raw(scratch, &["rebase", onto]).await?;
    if out.ok() {
        worktree_remove(repo, scratch).await.ok();
        return Ok(None);
    }
    // Leave nothing half-rebased behind: abort, then drop the tree entirely.
    git_raw(scratch, &["rebase", "--abort"]).await.ok();
    let why = if out.stderr.trim().is_empty() {
        out.stdout.trim().to_owned()
    } else {
        out.stderr.trim().to_owned()
    };
    worktree_remove(repo, scratch).await.ok();
    Ok(Some(why))
}

/// Fetch one branch from `remote`, updating its remote-tracking ref.
///
/// The refspec is spelled out rather than left to `git fetch <remote>
/// <branch>`, which writes `FETCH_HEAD` and updates
/// `refs/remotes/<remote>/<branch>` only as a side effect of the remote's
/// configured refspec. Naming the destination makes the thing this function
/// exists for - a tracking ref that moved - the operation rather than a
/// consequence of configuration magi does not own.
///
/// Honest note: a CI failure was first read as proof that some git versions do
/// not update the tracking ref here. That was wrong - the fetch had nothing to
/// update because the test had pushed to the wrong branch - so this is
/// determinism, not a fix for a demonstrated portability bug.
///
/// Refs, not the working copy: nothing is checked out and no local branch
/// moves, so this is safe to run while the operator has uncommitted work.
/// Returned as a [`GitOut`] rather than an error so the caller can decide - a
/// machine with no network must still be able to start a run.
pub async fn fetch(repo: &Path, remote: &str, branch: &str) -> Result<GitOut> {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    git_raw(repo, &["fetch", "--quiet", remote, &refspec]).await
}

/// Does this ref resolve?
pub async fn rev_exists(repo: &Path, rev: &str) -> bool {
    git_raw(repo, &["rev-parse", "--verify", "--quiet", rev])
        .await
        .is_ok_and(|o| o.ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        tokio::fs::create_dir_all(&repo).await.unwrap();
        git(&repo, &["init", "-b", "main"]).await.unwrap();
        git(&repo, &["config", "user.name", "test"]).await.unwrap();
        git(&repo, &["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        tokio::fs::write(repo.join("a.txt"), "one\n").await.unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "init"]).await.unwrap();
        (dir, repo)
    }

    #[tokio::test]
    async fn a_branch_rebases_onto_a_moved_base_and_says_when_it_cannot() {
        let (_g, repo) = scratch().await;

        // A side branch touching a different file: rebases cleanly.
        git(&repo, &["checkout", "-b", "side"]).await.unwrap();
        tokio::fs::write(repo.join("b.txt"), "side\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "side work"]).await.unwrap();

        // main moves under it, which is what a repository merging other
        // pull requests does to a competition that took two hours.
        git(&repo, &["checkout", "main"]).await.unwrap();
        tokio::fs::write(repo.join("c.txt"), "main\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "main moved"]).await.unwrap();

        let scratch_tree = repo.parent().unwrap().join("rebase-scratch");
        let clean = rebase_branch_in_temp(&repo, &scratch_tree, "side", "main")
            .await
            .unwrap();
        assert!(clean.is_none(), "a disjoint change rebases: {clean:?}");
        assert_eq!(
            commits_ahead(&repo, "main", "side").await.unwrap(),
            1,
            "one commit, replayed onto the new base"
        );
        assert!(
            !scratch_tree.exists(),
            "the throwaway worktree is not left behind"
        );

        // A real conflict: both sides edit the same line.
        git(&repo, &["checkout", "-b", "clash"]).await.unwrap();
        tokio::fs::write(repo.join("a.txt"), "clash\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "clash"]).await.unwrap();
        git(&repo, &["checkout", "main"]).await.unwrap();
        tokio::fs::write(repo.join("a.txt"), "main edit\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "main edit"]).await.unwrap();

        let before = rev_parse(&repo, "clash").await.unwrap();
        let why = rebase_branch_in_temp(&repo, &scratch_tree, "clash", "main")
            .await
            .unwrap()
            .expect("a same-line clash cannot be rebased silently");
        assert!(
            why.to_lowercase().contains("conflict"),
            "the reason is what git said, which is what a person needs: {why}"
        );
        assert_eq!(
            rev_parse(&repo, "clash").await.unwrap(),
            before,
            "a failed rebase leaves the branch exactly where it was"
        );
        assert!(!scratch_tree.exists(), "and cleans up after itself");
    }

    #[tokio::test]
    async fn clean_repo_reports_clean_then_dirty() {
        let (_g, repo) = scratch().await;
        assert!(is_clean(&repo).await.unwrap());
        tokio::fs::write(repo.join("a.txt"), "two\n").await.unwrap();
        assert!(!is_clean(&repo).await.unwrap());
    }

    #[tokio::test]
    async fn worktree_lifecycle_and_diff() {
        let (guard, repo) = scratch().await;
        let base = rev_parse(&repo, "HEAD").await.unwrap();
        let wt = guard.path().join("wt-a");
        worktree_add_branch(&repo, &wt, "magi/test/a", &base)
            .await
            .unwrap();
        tokio::fs::write(wt.join("b.txt"), "candidate\n")
            .await
            .unwrap();

        assert!(commit_all(&wt, "candidate work").await.unwrap());
        assert!(!commit_all(&wt, "nothing left").await.unwrap());

        assert_eq!(commits_ahead(&wt, &base, "HEAD").await.unwrap(), 1);
        let patch = diff(&wt, &base, "HEAD").await.unwrap();
        assert!(patch.contains("b.txt"), "patch was: {patch}");
        assert_eq!(
            changed_files(&wt, &base, "HEAD").await.unwrap(),
            ["b.txt".to_owned()]
        );

        // The rescue commit must not carry the operator's identity.
        let author = git(&wt, &["log", "-1", "--format=%an <%ae>"])
            .await
            .unwrap();
        assert_eq!(author, "magi candidate <magi@localhost>");

        assert!(worktree_remove(&repo, &wt).await.unwrap());
        assert!(branch_exists(&repo, "magi/test/a").await.unwrap());
        assert!(branch_delete(&repo, "magi/test/a").await.unwrap());
        assert!(!branch_exists(&repo, "magi/test/a").await.unwrap());
    }

    #[tokio::test]
    async fn worktree_scoped_hooks_path_does_not_leak_to_primary() {
        let (guard, repo) = scratch().await;
        let base = rev_parse(&repo, "HEAD").await.unwrap();
        let wt = guard.path().join("wt-h");
        worktree_add_branch(&repo, &wt, "magi/test/h", &base)
            .await
            .unwrap();
        let hooks = guard.path().join("hooks");
        tokio::fs::create_dir_all(&hooks).await.unwrap();

        assert!(enable_worktree_config(&repo).await.unwrap());
        set_worktree_hooks_path(&wt, &hooks).await.unwrap();

        let in_wt = git(&wt, &["config", "--get", "core.hooksPath"])
            .await
            .unwrap();
        assert!(!in_wt.is_empty());
        let in_primary = git_raw(&repo, &["config", "--get", "core.hooksPath"])
            .await
            .unwrap();
        assert!(
            !in_primary.ok(),
            "primary worktree must keep its own hooks: {in_primary:?}"
        );

        disable_worktree_config(&repo).await.unwrap();
    }

    #[tokio::test]
    async fn local_exclude_is_idempotent() {
        let (_g, repo) = scratch().await;
        local_exclude(&repo, "/.magi/").await.unwrap();
        local_exclude(&repo, "/.magi/").await.unwrap();
        let path = repo.join(".git/info/exclude");
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(body.matches("/.magi/").count(), 1);
    }
}
