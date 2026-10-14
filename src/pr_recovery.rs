//! Explicit recovery of a task's public implementation, independent of historical candidates.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{git, land};

/// Durable operator selection. Only the recovery command creates this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// Existing pull request in this repository.
    pub url: String,
    /// Task-scoped public branch.
    pub branch: String,
    /// Complete implementation and initial publication lease.
    pub head: String,
    /// Criteria the operator approved, retained without rewriting them.
    pub instruction: String,
    /// Approval generation: a later selection does not resume an earlier run.
    pub approved_at: jiff::Timestamp,
}

const RETAIN_SOURCES: &str = "retain-recovery-sources";

/// Preserve every historical run's source artifacts before releasing a recovery task.
/// A sidecar survives stale run-state saves and applies even to older run schemas.
pub fn retain_sources(task: &crate::queue::Task, home: &Path) -> Result<()> {
    for id in task.runs.iter().collect::<std::collections::BTreeSet<_>>() {
        ensure!(
            crate::run::is_run_id(id),
            "invalid historical run id; refusing source retention"
        );
        let dir = home.join("runs").join(id);
        ensure!(
            dir.is_dir(),
            "historical run {id} is missing; cannot retain its sources"
        );
        std::fs::write(dir.join(RETAIN_SOURCES), &task.id)
            .with_context(|| format!("retaining sources of historical run {id}"))?;
    }
    Ok(())
}

pub(crate) fn sources_retained(home: &Path, id: &str) -> bool {
    home.join("runs")
        .join(id)
        .join(RETAIN_SOURCES)
        .try_exists()
        .unwrap_or(true)
}

/// A partially created recovery is retained and linked to its task for inspection.
#[derive(Debug)]
pub(crate) struct StartupFailed {
    pub run: String,
    pub cause: String,
}

impl std::fmt::Display for StartupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "recovery run {} could not start: {}",
            self.run, self.cause
        )
    }
}
impl std::error::Error for StartupFailed {}

impl Selection {
    /// Confirm task identity and immutable criteria before starting work.
    pub fn check_task(&self, task: &str, instruction: &str) -> Result<()> {
        ensure!(
            self.branch == format!("magi/task-{task}"),
            "recovery head belongs to another task"
        );
        ensure!(
            self.instruction == instruction,
            "recovery criteria changed; select the public head again"
        );
        ensure!(
            self.head.len() == 40 && self.head.bytes().all(|b| b.is_ascii_hexdigit()),
            "recovery requires a full commit SHA"
        );
        Ok(())
    }

    /// Read-only forge validation plus fetch; no local branch is moved or pushed.
    pub async fn verify(&self, repo: &Path, remote: &str, base: &str, lease: &str) -> Result<()> {
        let pr_repo = land::slug_of_pr_url(&self.url).context("invalid recovery PR URL")?;
        for args in [
            vec!["remote", "get-url", "--all", remote],
            vec!["remote", "get-url", "--push", "--all", remote],
        ] {
            let urls = git::git(repo, &args).await?;
            ensure!(
                !urls.trim().is_empty(),
                "recovery remote has no repository URL"
            );
            for url in urls.lines() {
                let remote_repo =
                    remote_slug(url).context("cannot prove recovery remote repository identity")?;
                land::ensure_same_repo(&remote_repo, &pr_repo)?;
            }
        }
        let found = land::find_open_pr(repo, &self.branch, base).await?;
        ensure!(
            matches!(found, land::OpenPr::One { ref url, .. } if url == &self.url),
            "selected PR is not the unique open task PR in this repository"
        );
        let (ok, raw) = land::gh(
            repo,
            &[
                "pr".into(),
                "view".into(),
                self.url.clone(),
                "--json".into(),
                "headRefName,headRefOid,baseRefName,state,isCrossRepository".into(),
            ],
        )
        .await?;
        ensure!(ok, "could not verify selected PR: {raw}");
        validate_view(&raw, &self.branch, base, lease)?;
        let fetched = git::fetch(repo, remote, &self.branch).await?;
        ensure!(
            fetched.ok(),
            "fetch selected public head failed: {}",
            fetched.stderr
        );
        let fetched_head =
            git::rev_parse(repo, &format!("refs/remotes/{remote}/{}", self.branch)).await?;
        ensure!(
            fetched_head == lease,
            "public head moved; select and approve its new head before recovery"
        );
        ensure!(
            git::remote_head(repo, remote, &self.branch)
                .await?
                .as_deref()
                == Some(lease),
            "public head lease changed during recovery"
        );
        Ok(())
    }
}

// Require a recognizable forge URL rather than guessing through local paths or SSH aliases.
fn remote_slug(url: &str) -> Option<String> {
    let url = url
        .trim()
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(url.trim().trim_end_matches('/'));
    let (host, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
    {
        let (authority, path) = rest.split_once('/')?;
        (authority.rsplit('@').next()?, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        let (_, host) = authority.split_once('@')?;
        (host, path)
    };
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if host.is_empty() || owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{host}/{owner}/{repo}"))
}

fn validate_view(raw: &str, branch: &str, base: &str, lease: &str) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct View {
        head_ref_name: String,
        head_ref_oid: String,
        base_ref_name: String,
        state: String,
        is_cross_repository: bool,
    }
    let view: View = serde_json::from_str(raw).context("selected public head metadata")?;
    ensure!(
        view.state == "OPEN"
            && !view.is_cross_repository
            && view.head_ref_name == branch
            && view.base_ref_name == base,
        "selected PR ownership or base changed"
    );
    ensure!(
        view.head_ref_oid == lease,
        "selected public head moved; refusing recovery"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_source_requires_the_same_fetch_and_push_repository() {
        for remote in [
            "https://example.test/team/project.git",
            "git@example.test:team/project.git",
            "ssh://git@example.test/team/project.git",
        ] {
            assert_eq!(
                remote_slug(remote).as_deref(),
                Some("example.test/team/project")
            );
        }
        assert!(remote_slug("/tmp/another.git").is_none());
        assert!(
            land::ensure_same_repo(
                &remote_slug("git@example.test:team/other.git").unwrap(),
                "example.test/team/project"
            )
            .is_err()
        );
        assert!(
            land::ensure_same_repo(
                &remote_slug("git@other.test:team/project.git").unwrap(),
                "example.test/team/project"
            )
            .is_err()
        );
    }

    #[test]
    fn selected_head_refuses_moved_or_foreign_public_heads() {
        let raw = r#"{"headRefName":"magi/task-t","headRefOid":"aaa","baseRefName":"main","state":"OPEN","isCrossRepository":false}"#;
        assert!(validate_view(raw, "magi/task-t", "main", "aaa").is_ok());
        assert!(validate_view(raw, "magi/task-t", "main", "bbb").is_err());
        assert!(validate_view(raw, "magi/task-other", "main", "aaa").is_err());
        assert!(validate_view(raw, "magi/task-t", "other", "aaa").is_err());
        assert!(
            validate_view(&raw.replace("false", "true"), "magi/task-t", "main", "aaa").is_err()
        );
        assert!(
            validate_view(&raw.replace("OPEN", "CLOSED"), "magi/task-t", "main", "aaa").is_err()
        );
    }

    #[test]
    fn selected_head_is_bound_to_authored_criteria_and_task() {
        let selected = Selection {
            url: "url".into(),
            branch: "magi/task-t".into(),
            head: "a".repeat(40),
            instruction: "approved".into(),
            approved_at: jiff::Timestamp::now(),
        };
        assert!(selected.check_task("t", "approved").is_ok());
        assert!(
            selected
                .check_task("t", "prose mentioning another candidate")
                .is_err()
        );
        assert!(selected.check_task("other", "approved").is_err());
    }
}
