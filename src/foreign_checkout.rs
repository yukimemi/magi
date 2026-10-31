//! Guarded fast-forward delivery to a hand-written branch's worktree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{git, run::RunState};

/// A foreign checkout is evidence, never a worktree owned by the run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForeignCheckout {
    /// Original branch, distinct from the isolated review branch.
    pub branch: String,
    /// Canonical checkout path.
    pub path: PathBuf,
    /// Canonical Git administration directory, to detect path replacement.
    pub git_dir: PathBuf,
    /// Last head delivered or observed when the review began.
    pub synced_head: String,
}

pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn administration(path: &Path) -> Result<PathBuf> {
    let dir = git::git(path, &["rev-parse", "--absolute-git-dir"]).await?;
    Ok(std::fs::canonicalize(dir.trim())?)
}

/// Inspect without modifying the checkout, including untracked files and Git operations.
pub(crate) async fn inspect(repo: &Path, branch: &str, path: &Path) -> Result<ForeignCheckout> {
    let checkout = ForeignCheckout {
        branch: branch.to_owned(),
        path: std::fs::canonicalize(path)?,
        git_dir: administration(path).await?,
        synced_head: git::rev_parse(repo, &format!("refs/heads/{branch}")).await?,
    };
    checkout.validate(repo).await?;
    Ok(checkout)
}

impl ForeignCheckout {
    async fn validate(&self, repo: &Path) -> Result<()> {
        ensure!(
            administration(&self.path).await? == self.git_dir,
            "checkout was replaced"
        );
        ensure!(
            git::git(
                &self.path,
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--ignore-submodules=none"
                ]
            )
            .await?
            .is_empty(),
            "checkout is dirty"
        );
        ensure!(
            git::git(&self.path, &["symbolic-ref", "HEAD"]).await?
                == format!("refs/heads/{}", self.branch),
            "checkout switched branches"
        );
        ensure!(
            git::rev_parse(&self.path, "HEAD").await? == self.synced_head
                && git::rev_parse(repo, &format!("refs/heads/{}", self.branch)).await?
                    == self.synced_head,
            "branch moved since the last synchronization"
        );
        for name in [
            "locked",
            "index.lock",
            "HEAD.lock",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
        ] {
            ensure!(
                !self.git_dir.join(name).exists(),
                "checkout has a lock or Git operation in progress ({name})"
            );
        }
        Ok(())
    }

    /// Fast-forward only after revalidating the original checkout and branch.
    pub(crate) async fn deliver(&mut self, repo: &Path, head: &str) -> Result<()> {
        self.validate(repo).await?;
        ensure!(
            git::is_ancestor(repo, &self.synced_head, head).await,
            "review rewrote history; automatic delivery requires a fast-forward"
        );
        if head != self.synced_head {
            git::git(
                &self.path,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "merge",
                    "--ff-only",
                    "--no-autostash",
                    "--no-edit",
                    head,
                ],
            )
            .await?;
            self.synced_head = head.to_owned();
            self.validate(repo).await?;
        }
        Ok(())
    }
}

/// Deliver the isolated head before completion or publication; failures retain both trees.
/// Acquires the configured repository coordination lease before inspecting or delivering.
pub(crate) async fn write_back(state: &mut RunState, head: &str) -> Result<bool> {
    let Some(mut checkout) = state.foreign_checkout.clone() else {
        return Ok(true);
    };
    let inherited = crate::repo_lock::WaitPolicy::current();
    let wait = crate::repo_lock::WaitPolicy::new(
        std::time::Duration::from_secs(state.config.graph.timeout_resource_wait),
        move || inherited.cancelled(),
    );
    let _coordination = crate::repo_lock::SeatLease::operation_with(
        &state.repo,
        state.config.coordination.as_ref(),
        &wait,
    )
    .await?;
    if let Err(error) = checkout.deliver(&state.repo, head).await {
        let shadow = state.winner().map(|w| w.branch.as_str()).unwrap_or(head);
        let message = format!(
            "foreign checkout delivery refused: {error:#}. Review fixes remain on `{shadow}`. After resolving the checkout, run: git -C {} merge --ff-only {}",
            quote(&checkout.path.to_string_lossy()),
            quote(shadow)
        );
        state.status = crate::run::RunStatus::Blocked;
        state.event("foreign-checkout", &message);
        state.save()?;
        crate::notices::raise(
            crate::notices::Notice::error(&format!("foreign-checkout:{}", state.id), message)
                .link(crate::notices::Link::Run {
                    id: state.id.clone(),
                })
                .about([state.id.clone()]),
        );
        return Ok(false);
    }
    state.foreign_checkout = Some(checkout);
    state
        .save()
        .context("save foreign checkout synchronization")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, PathBuf, ForeignCheckout) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git::git(&repo, &["init", "-b", "main"]).await.unwrap();
        git::git(&repo, &["config", "user.name", "Test"])
            .await
            .unwrap();
        git::git(&repo, &["config", "user.email", "test@example.invalid"])
            .await
            .unwrap();
        git::git(&repo, &["commit", "--allow-empty", "-m", "base"])
            .await
            .unwrap();
        let path = dir.path().join("foreign");
        git::worktree_add_branch(&repo, &path, "topic", "main")
            .await
            .unwrap();
        let checkout = inspect(&repo, "topic", &path).await.unwrap();
        (dir, repo, checkout)
    }

    #[tokio::test]
    async fn locked_checkout_is_refused_without_removing_its_lock() {
        let (_dir, repo, mut checkout) = fixture().await;
        let lock = checkout.git_dir.join("locked");
        std::fs::write(&lock, "operator lock").unwrap();
        let head = checkout.synced_head.clone();
        assert!(
            checkout
                .deliver(&repo, &head)
                .await
                .unwrap_err()
                .to_string()
                .contains("lock")
        );
        assert_eq!(std::fs::read_to_string(lock).unwrap(), "operator lock");
        assert_eq!(git::rev_parse(&checkout.path, "HEAD").await.unwrap(), head);
    }

    #[tokio::test]
    async fn rewritten_review_is_refused_without_moving_the_foreign_branch() {
        let (_dir, repo, mut checkout) = fixture().await;
        git::git(
            &checkout.path,
            &["commit", "--allow-empty", "-m", "original work"],
        )
        .await
        .unwrap();
        checkout.synced_head = git::rev_parse(&checkout.path, "HEAD").await.unwrap();
        let rewritten = git::rev_parse(&repo, "main").await.unwrap();
        assert!(
            checkout
                .deliver(&repo, &rewritten)
                .await
                .unwrap_err()
                .to_string()
                .contains("rewrote history")
        );
        assert_eq!(
            git::rev_parse(&checkout.path, "HEAD").await.unwrap(),
            checkout.synced_head
        );
        assert!(git::is_clean(&checkout.path).await.unwrap());
    }
}
