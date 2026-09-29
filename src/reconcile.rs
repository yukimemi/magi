//! Deciding what a local branch and its remote twin mean when both moved.
//!
//! A run's branch can be rewritten on one side only: magi rebases it locally
//! (a retry, a fixer) and, until this module, never pushed the result, so the
//! next attempt found `refs/heads/<b>` and `<remote>/<b>` diverged - the same
//! change on two different bases. Picking the remote side there silently
//! discards the rebase; picking the local one discards whatever a person
//! pushed. [`classify`] therefore only lets magi settle the question when it
//! can *prove* nothing is lost, and otherwise hands both sides to the operator
//! as a [`Diverged`] that says what each one carries.
//!
//! The proof is patch-id, via `git cherry`: two commits that apply the same
//! diff are the same change on different bases. A merge, a squash or a rebase
//! that needed conflict resolution changes the patch-id and is therefore
//! [`Divergence::Genuine`] - the safe side of every doubt.

use std::path::Path;

use anyhow::Result;

use crate::git;

/// One commit on one side of a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Full object id.
    pub sha: String,
    /// First line of the message.
    pub subject: String,
    /// Which run made it, when a run's record names it; `None` reads as
    /// "outside magi" as far as this machine can tell.
    pub made_by: Option<String>,
}

/// A divergence magi could not settle by itself.
#[derive(Debug, Clone)]
pub struct Diverged {
    /// Branch name.
    pub branch: String,
    /// Remote name.
    pub remote: String,
    /// Local tip sha.
    pub local_tip: String,
    /// Remote tip sha.
    pub origin_tip: String,
    /// Commits only the local branch has that no origin commit repeats.
    pub local_only: Vec<Commit>,
    /// Commits only the remote has that no local commit repeats.
    pub origin_only: Vec<Commit>,
    /// Why this was not a plain rebase.
    pub reason: String,
}

impl std::fmt::Display for Diverged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "local `{}` ({}) and {}/{} ({}) have diverged and carry different changes: {}",
            self.branch,
            crate::run::short_of(&self.local_tip),
            self.remote,
            self.branch,
            crate::run::short_of(&self.origin_tip),
            self.reason
        )
    }
}

impl std::error::Error for Diverged {}

fn describe(c: &Commit) -> String {
    format!(
        "- `{}` {} (made by: {})",
        c.sha.chars().take(7).collect::<String>(),
        c.subject,
        c.made_by.as_deref().unwrap_or("unknown, outside magi")
    )
}

impl Diverged {
    /// The one line the operator sees first.
    pub fn summary(&self) -> String {
        format!(
            "`{}` differs between this machine and {}: which side should magi keep?",
            self.branch, self.remote
        )
    }

    /// Both sides, what each holds, and what choosing the other loses.
    pub fn detail(&self) -> String {
        let list = |cs: &[Commit]| {
            if cs.is_empty() {
                "- (none)".to_owned()
            } else {
                cs.iter().map(describe).collect::<Vec<_>>().join("\n")
            }
        };
        format!(
            "Local `{b}` ({l}) and {r}/{b} ({o}) diverged, and magi could not show \
             that one is the other rebased.\n\nWhy: {why}\n\n\
             Only on the local branch:\n{local}\n\nOnly on {r}:\n{origin}\n\n\
             Pushing the local branch (with a lease on {o}) drops the {r} commits above; \
             taking {r} drops the local ones. Nothing has been moved yet.",
            b = self.branch,
            r = self.remote,
            l = self.local_tip.chars().take(7).collect::<String>(),
            o = self.origin_tip.chars().take(7).collect::<String>(),
            why = self.reason,
            local = list(&self.local_only),
            origin = list(&self.origin_only),
        )
    }
}

/// What a divergence turned out to be.
#[derive(Debug)]
pub enum Divergence {
    /// Every commit only local has is empty: a placeholder the remote's work
    /// replaced. Moving the branch onto the remote loses nothing.
    Placeholder,
    /// Local is the remote's change rebased onto a newer base.
    PureRebase,
    /// Different changes on each side.
    Genuine(Box<Diverged>),
}

/// Commits reachable from `head` and not from `exclude`, newest first.
async fn range(repo: &Path, exclude: &str, head: &str) -> Result<Vec<String>> {
    let out = git::git(
        repo,
        &["rev-list", "--parents", &format!("{exclude}..{head}")],
    )
    .await?;
    Ok(out.lines().map(str::to_owned).collect())
}

async fn only_empty_non_merges(repo: &Path, exclude: &str, head: &str) -> Result<bool> {
    for line in range(repo, exclude, head).await? {
        let mut parts = line.split_whitespace();
        let (Some(sha), Some(parent), None) = (parts.next(), parts.next(), parts.next()) else {
            // A root commit or a merge is content nobody proved redundant.
            return Ok(false);
        };
        if git::tree_of(repo, sha).await? != git::tree_of(repo, parent).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Look up which run made `sha`: a run that recorded it in an event, or one
/// whose winning branch is `branch`. Best-effort and read-only.
fn made_by(branch: &str, sha: &str) -> Option<String> {
    crate::run::try_home()?;
    let short: String = sha.chars().take(7).collect();
    crate::run::list_ids().into_iter().find_map(|id| {
        let state = crate::run::RunState::load(&id).ok()?;
        let names_branch = state.candidates.iter().any(|c| c.branch == branch);
        let names_sha = state.events.iter().any(|e| e.message.contains(&short));
        (names_sha || names_branch).then(|| format!("run {}", crate::run::short_of(&id)))
    })
}

async fn commits(repo: &Path, branch: &str, shas: &[String]) -> Vec<Commit> {
    let mut out = Vec::new();
    for sha in shas {
        let subject = git::git(repo, &["log", "-1", "--format=%s", sha])
            .await
            .unwrap_or_default();
        out.push(Commit {
            sha: sha.clone(),
            subject,
            made_by: made_by(branch, sha),
        });
    }
    out
}

/// Classify a divergence between `local` and `origin` (both shas).
///
/// `base` is the base branch's tip: commits reachable from it are what a
/// rebase pulled in, not local work.
pub async fn classify(
    repo: &Path,
    remote: &str,
    branch: &str,
    local: &str,
    origin: &str,
    base: &str,
) -> Result<Divergence> {
    let mb = git::git(repo, &["merge-base", local, origin]).await.ok();
    if let Some(mb) = &mb
        && only_empty_non_merges(repo, mb, local).await?
    {
        return Ok(Divergence::Placeholder);
    }

    // What origin has that no local commit repeats, and what local has that
    // no origin commit repeats, ignoring commits the base already holds.
    let (origin_missing, _) = git::cherry(repo, local, origin).await?;
    let (local_extra, _) = git::cherry(repo, origin, local).await?;
    let mut local_missing = Vec::new();
    for sha in local_extra {
        if !git::is_ancestor(repo, &sha, base).await {
            local_missing.push(sha);
        }
    }
    if origin_missing.is_empty() && local_missing.is_empty() {
        return Ok(Divergence::PureRebase);
    }

    let mut reasons = Vec::new();
    if !local_missing.is_empty() {
        reasons.push(format!(
            "{} local commit(s) have no equivalent patch on {remote}",
            local_missing.len()
        ));
    }
    if !origin_missing.is_empty() {
        reasons.push(format!(
            "{} {remote} commit(s) have no equivalent patch locally",
            origin_missing.len()
        ));
    }
    Ok(Divergence::Genuine(Box::new(Diverged {
        branch: branch.to_owned(),
        remote: remote.to_owned(),
        local_tip: local.to_owned(),
        origin_tip: origin.to_owned(),
        local_only: commits(repo, branch, &local_missing).await,
        origin_only: commits(repo, branch, &origin_missing).await,
        reason: format!(
            "{} (a merge, a squash or a rebase with conflict resolution changes the patch \
             and cannot be matched)",
            reasons.join("; ")
        ),
    })))
}

/// If `local` is provably `origin`'s change rebased, push it with a lease
/// pinned to `origin` and say so; `Ok(false)` when it is not.
///
/// The one place a divergence is settled without asking. A refused lease is an
/// error: someone pushed since `origin` was read, and that is now a real
/// divergence for a person, so nothing is forced.
pub async fn reconcile(
    repo: &Path,
    remote: &str,
    branch: &str,
    local: &str,
    origin: &str,
    base: &str,
) -> Result<Reconciliation> {
    match classify(repo, remote, branch, local, origin, base).await? {
        Divergence::Placeholder => Ok(Reconciliation::Placeholder),
        Divergence::Genuine(d) => Ok(Reconciliation::Genuine(d)),
        Divergence::PureRebase => {
            let out = git::push_pinned(repo, remote, branch, origin).await?;
            if !out.ok() {
                anyhow::bail!(
                    "`{branch}` is a rebase of {remote}/{branch}, but the push was refused \
                     (someone may have pushed since {}): {}",
                    crate::run::short_of(origin),
                    out.stderr
                );
            }
            Ok(Reconciliation::Pushed)
        }
    }
}

/// What [`reconcile`] did.
#[derive(Debug)]
pub enum Reconciliation {
    /// Local was pushed over origin; keep local.
    Pushed,
    /// Local adds nothing; the caller may move it onto origin.
    Placeholder,
    /// Ask the operator.
    Genuine(Box<Diverged>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn commit(dir: &Path, file: &str, body: &str, msg: &str) {
        std::fs::write(dir.join(file), body).unwrap();
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "-q", "-m", msg]);
    }

    /// `repo` with a bare `origin`, `main` pushed, and `work` branched and
    /// pushed with one commit.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let origin = dir.path().join("origin.git");
        std::fs::create_dir_all(&repo).unwrap();
        sh(
            dir.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "user.email", "t@example.com"]);
        sh(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        commit(&repo, "a.txt", "one\n", "init");
        sh(&repo, &["push", "-q", "origin", "main"]);
        sh(&repo, &["checkout", "-q", "-b", "work"]);
        commit(&repo, "w.txt", "work\n", "the fix");
        sh(&repo, &["push", "-q", "origin", "work"]);
        (dir, repo, origin)
    }

    /// Move `main` on origin forward, then rebase `work` onto it locally.
    fn advance_and_rebase(repo: &Path) {
        sh(repo, &["checkout", "-q", "main"]);
        commit(repo, "b.txt", "two\n", "base moves");
        sh(repo, &["push", "-q", "origin", "main"]);
        sh(repo, &["checkout", "-q", "work"]);
        sh(repo, &["rebase", "-q", "main"]);
    }

    #[tokio::test]
    async fn a_rebase_is_pushed_under_a_lease_pinned_to_the_tip_seen() {
        let (_g, repo, origin) = fixture();
        let seen = sh(&repo, &["rev-parse", "work"]);
        advance_and_rebase(&repo);
        let out = git::push_pinned(&repo, "origin", "work", &seen)
            .await
            .unwrap();
        assert!(out.ok(), "{}", out.stderr);
        assert_eq!(
            sh(&origin, &["rev-parse", "work"]),
            sh(&repo, &["rev-parse", "work"])
        );
    }

    #[tokio::test]
    async fn a_push_from_someone_else_fails_the_pinned_lease_and_is_kept() {
        let (g, repo, origin) = fixture();
        let seen = sh(&repo, &["rev-parse", "work"]);
        advance_and_rebase(&repo);

        let other = g.path().join("other");
        sh(
            g.path(),
            &["clone", "-q", origin.to_str().unwrap(), "other"],
        );
        sh(&other, &["config", "user.name", "o"]);
        sh(&other, &["config", "user.email", "o@example.com"]);
        sh(&other, &["checkout", "-q", "work"]);
        commit(&other, "theirs.txt", "x\n", "a person's commit");
        sh(&other, &["push", "-q", "origin", "work"]);
        let theirs = sh(&origin, &["rev-parse", "work"]);

        // A fetch in between must not move the lease: it is pinned.
        sh(&repo, &["fetch", "-q", "origin"]);
        let out = git::push_pinned(&repo, "origin", "work", &seen)
            .await
            .unwrap();
        assert!(!out.ok(), "the lease must refuse");
        assert_eq!(sh(&origin, &["rev-parse", "work"]), theirs);
    }

    #[tokio::test]
    async fn a_pure_rebase_divergence_reconciles_and_pushes_local() {
        let (_g, repo, origin) = fixture();
        let origin_tip = sh(&repo, &["rev-parse", "work"]);
        advance_and_rebase(&repo);
        let local = sh(&repo, &["rev-parse", "work"]);
        assert_ne!(local, origin_tip);
        let base = sh(&repo, &["rev-parse", "main"]);

        let r = reconcile(&repo, "origin", "work", &local, &origin_tip, &base)
            .await
            .unwrap();
        assert!(matches!(r, Reconciliation::Pushed), "{r:?}");
        assert_eq!(sh(&origin, &["rev-parse", "work"]), local);
    }

    #[tokio::test]
    async fn a_genuine_divergence_is_refused_and_names_both_sides() {
        let (g, repo, origin) = fixture();
        let other = g.path().join("other");
        sh(
            g.path(),
            &["clone", "-q", origin.to_str().unwrap(), "other"],
        );
        sh(&other, &["config", "user.name", "o"]);
        sh(&other, &["config", "user.email", "o@example.com"]);
        sh(&other, &["checkout", "-q", "work"]);
        commit(&other, "theirs.txt", "x\n", "theirs: another change");
        sh(&other, &["push", "-q", "origin", "work"]);
        sh(&repo, &["fetch", "-q", "origin"]);
        commit(&repo, "mine.txt", "y\n", "mine: a different change");
        let local = sh(&repo, &["rev-parse", "work"]);
        let origin_tip = sh(&repo, &["rev-parse", "origin/work"]);
        let base = sh(&repo, &["rev-parse", "main"]);

        let r = reconcile(&repo, "origin", "work", &local, &origin_tip, &base)
            .await
            .unwrap();
        let Reconciliation::Genuine(d) = r else {
            panic!("expected Genuine, got {r:?}");
        };
        assert_eq!(d.local_only.len(), 1);
        assert_eq!(d.origin_only.len(), 1);
        assert!(d.detail().contains("mine: a different change"));
        assert!(d.detail().contains("theirs: another change"));
        assert_eq!(sh(&repo, &["rev-parse", "work"]), local, "local untouched");
        assert_eq!(
            sh(&origin, &["rev-parse", "work"]),
            origin_tip,
            "origin untouched"
        );
    }

    #[tokio::test]
    async fn an_empty_placeholder_is_recognised_but_a_revert_history_is_not() {
        let (_g, repo, _origin) = fixture();
        sh(&repo, &["branch", "ph", "work~1"]);
        sh(&repo, &["checkout", "-q", "ph"]);
        sh(
            &repo,
            &["commit", "-q", "--allow-empty", "-m", "placeholder"],
        );
        let ph = sh(&repo, &["rev-parse", "ph"]);
        let work = sh(&repo, &["rev-parse", "work"]);
        let base = sh(&repo, &["rev-parse", "main"]);
        let r = classify(&repo, "origin", "ph", &ph, &work, &base)
            .await
            .unwrap();
        assert!(matches!(r, Divergence::Placeholder), "{r:?}");

        // Adds a file then removes it: final diff is empty, history is not.
        sh(&repo, &["checkout", "-q", "-b", "churn", "work~1"]);
        commit(&repo, "t.txt", "t\n", "add");
        sh(&repo, &["rm", "-q", "t.txt"]);
        sh(&repo, &["commit", "-q", "-m", "remove"]);
        let churn = sh(&repo, &["rev-parse", "churn"]);
        let r = classify(&repo, "origin", "churn", &churn, &work, &base)
            .await
            .unwrap();
        assert!(matches!(r, Divergence::Genuine(_)), "{r:?}");
    }
}
