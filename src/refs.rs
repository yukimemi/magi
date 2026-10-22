//! Structured implementation source selection. Instruction text is never input.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::git;

/// What a reference turned out to be, relative to the base commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedKind {
    /// A commit that exists but is not reachable from the base.
    Unmerged,
    /// A commit the base already contains.
    AlreadyMerged,
    /// Could not be resolved or judged.
    Unresolved,
}

/// One reference found in a task, and what the repository says about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seed {
    /// The text as written in the task.
    pub token: String,
    /// What the repository says about it.
    pub kind: SeedKind,
    /// Full commit id; empty for [`SeedKind::Unresolved`].
    #[serde(default)]
    pub sha: String,
    /// Was the token a branch name (as opposed to a bare sha)?
    #[serde(default)]
    pub branch: bool,
    /// Branches that contain the commit (unmerged), or why it is unresolved.
    #[serde(default)]
    pub detail: String,
}

/// Explicit CLI input, resolved once before a task is filed. Never built from prose.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Sources {
    /// Starting commit, including its ancestry, selected by `--start-from`.
    pub start_from: Option<Input>,
    /// Individual commits selected by `--pick-commit`, in application order.
    pub pick_commits: Vec<Input>,
}

/// The original argument and immutable commit identity. This type is only used
/// by structured source selection; old `RunState::seeds` remain legacy inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    /// Branch name or commit abbreviation originally supplied by the caller.
    pub token: String,
    /// Full immutable commit identity resolved when the task was filed.
    pub sha: String,
    /// Structured authorization for importing this commit.
    pub provenance: Provenance,
}

/// Authorization for new source inputs; legacy `Seed` records have no such
/// provenance and cannot be converted into this type automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Selected through an explicit source argument, independent of task text.
    ExplicitCli,
}

/// Validated immutable inputs used to prepare candidate worktrees.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Starting commit instead of the configured base, when selected.
    pub start: Option<String>,
    /// Single commits to apply in the caller's specified order.
    pub picks: Vec<String>,
}

impl Sources {
    /// No explicit input. Records then omit the field entirely, so a task or
    /// run without sources stays byte-compatible with builds that predate it
    /// (`Task` and `RunState` reject unknown fields).
    pub fn is_empty(&self) -> bool {
        self.start_from.is_none() && self.pick_commits.is_empty()
    }

    /// Resolve explicit CLI arguments to immutable commits without parsing prose.
    pub async fn resolve(
        repo: &Path,
        remote: &str,
        start: Option<&str>,
        picks: &[String],
    ) -> anyhow::Result<Self> {
        let start_from = match start {
            Some(token) => Some(resolve_input(repo, remote, token, false).await?),
            None => None,
        };
        let mut pick_commits = Vec::new();
        for token in picks {
            pick_commits.push(resolve_input(repo, remote, token, true).await?);
        }
        Ok(Self {
            start_from,
            pick_commits,
        })
    }

    /// Recheck immutable objects before creating any worktrees. Never resolve
    /// the saved token again: a branch may have moved since registration.
    pub async fn plan(&self, repo: &Path, base: &str) -> anyhow::Result<Plan> {
        let start = self.start_from.as_ref().map(|i| i.sha.clone());
        let onto = start.as_deref().unwrap_or(base);
        for input in self.start_from.iter().chain(&self.pick_commits) {
            anyhow::ensure!(
                input.sha.len() == 40 && input.sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid pinned source SHA"
            );
            anyhow::ensure!(
                git::commit_of(repo, &input.sha).await.as_deref() == Some(input.sha.as_str()),
                "missing source commit {}",
                input.sha
            );
        }
        let mut picks = Vec::new();
        for input in &self.pick_commits {
            let parents =
                git::git_raw(repo, &["rev-list", "--parents", "-n", "1", &input.sha]).await?;
            anyhow::ensure!(
                parents.ok() && parents.stdout.split_whitespace().count() <= 2,
                "--pick-commit does not support merge commits"
            );
            // Contained in the history the candidates actually start from
            // (`onto`: `--start-from` when given, else the base). A base-only
            // commit picked onto an explicit start that lacks it is a real
            // import, not a duplicate.
            anyhow::ensure!(
                !picks.contains(&input.sha) && !git::ancestry(repo, &input.sha, onto).await?,
                "duplicate or already contained --pick-commit {}",
                input.token
            );
            picks.push(input.sha.clone());
        }
        Ok(Plan { start, picks })
    }
}

async fn resolve_input(
    repo: &Path,
    remote: &str,
    token: &str,
    commit_only: bool,
) -> anyhow::Result<Input> {
    let hex = (7..=40).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_hexdigit());
    anyhow::ensure!(!commit_only || hex, "--pick-commit requires a commit SHA");
    let sha = if hex {
        // rev-parse refuses ambiguous object abbreviations. Also refuse a
        // branch/tag of the same name rather than relying on Git precedence.
        let refs = git::git_raw(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/heads/{token}"),
                &format!("refs/tags/{token}"),
            ],
        )
        .await?;
        anyhow::ensure!(
            refs.ok() && refs.stdout.is_empty(),
            "ambiguous source {token}"
        );
        git::commit_of(repo, token).await
    } else {
        let valid =
            git::git_raw(repo, &["check-ref-format", &format!("refs/heads/{token}")]).await?;
        anyhow::ensure!(
            valid.ok() && !token.starts_with('-'),
            "invalid source branch {token}"
        );
        let local = git::commit_of(repo, &format!("refs/heads/{token}")).await;
        let tracked = git::commit_of(repo, &format!("refs/remotes/{remote}/{token}")).await;
        match (local, tracked) {
            // A fetch that leaves the local branch behind (or an unpushed
            // local commit ahead) is one line of history: take its tip. Only
            // true divergence is ambiguous.
            (Some(l), Some(t)) if l != t => {
                if git::ancestry(repo, &l, &t).await? {
                    Some(t)
                } else if git::ancestry(repo, &t, &l).await? {
                    Some(l)
                } else {
                    anyhow::bail!(
                        "local and remote source branches have diverged: {token}; specify a full SHA"
                    )
                }
            }
            (local, tracked) => local.or(tracked),
        }
    }
    .ok_or_else(|| anyhow::anyhow!("unknown or ambiguous source {token}"))?;
    Ok(Input {
        token: token.to_owned(),
        sha,
        provenance: Provenance::ExplicitCli,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::Quiet as _;

    fn sh(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .quiet()
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit_file(dir: &Path, name: &str, body: &str) -> String {
        std::fs::write(dir.join(name), body).unwrap();
        sh(dir, &["add", "-A"]);
        sh(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@localhost",
                "commit",
                "-q",
                "-m",
                name,
            ],
        );
        let out = std::process::Command::new("git")
            .quiet()
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn repo_with_side_branch() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        String,
        String,
        String,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        let merged = commit_file(&repo, "a.txt", "a\n");
        sh(&repo, &["checkout", "-q", "-b", "magi/27b2/A"]);
        let fix = commit_file(&repo, "fix.txt", "fix\n");
        sh(&repo, &["checkout", "-q", "main"]);
        let base = merged.clone();
        (tmp, repo, base, fix, merged)
    }

    #[tokio::test]
    async fn explicit_picks_apply_single_commits_in_supplied_order() {
        let (_g, repo, base, first, _) = repo_with_side_branch();
        sh(&repo, &["checkout", "-q", "magi/27b2/A"]);
        let second = commit_file(&repo, "second.txt", "second");
        sh(&repo, &["checkout", "-q", "main"]);
        // Picking only the second commit must not bring the first one's file.
        let single = Sources::resolve(&repo, "origin", None, std::slice::from_ref(&second))
            .await
            .unwrap();
        let plan = single.plan(&repo, &base).await.unwrap();
        git::cherry_pick(&repo, &plan.picks[0]).await.unwrap();
        assert!(repo.join("second.txt").exists());
        assert!(!repo.join("fix.txt").exists());
        sh(&repo, &["reset", "--hard", &base]);
        let supplied = vec![second.clone(), first.clone()];
        let picks = Sources::resolve(&repo, "origin", None, &supplied)
            .await
            .unwrap();
        let plan = picks.plan(&repo, &base).await.unwrap();
        assert_eq!(plan.picks, supplied);
        for pick in plan.picks {
            git::cherry_pick(&repo, &pick).await.unwrap();
        }
        assert!(repo.join("second.txt").exists() && repo.join("fix.txt").exists());
        let duplicate = Sources::resolve(&repo, "origin", None, &[first.clone(), first])
            .await
            .unwrap();
        assert!(duplicate.plan(&repo, &base).await.is_err());
    }

    #[tokio::test]
    async fn branch_tips_resolve_along_one_line_and_divergence_and_merge_picks_are_rejected() {
        let (_g, repo, base, fix, _) = repo_with_side_branch();
        let start = |repo: std::path::PathBuf| async move {
            Sources::resolve(&repo, "origin", Some("magi/27b2/A"), &[])
                .await
                .map(|s| s.start_from.unwrap().sha)
        };
        // The remote is behind the local branch: the local tip is the newer one.
        sh(
            &repo,
            &["update-ref", "refs/remotes/origin/magi/27b2/A", &base],
        );
        assert_eq!(start(repo.clone()).await.unwrap(), fix);
        // A fetch left the local branch behind: the remote tip is the newer one.
        sh(
            &repo,
            &["update-ref", "refs/remotes/origin/magi/27b2/A", &fix],
        );
        sh(&repo, &["branch", "-f", "magi/27b2/A", &base]);
        assert_eq!(start(repo.clone()).await.unwrap(), fix);
        // Neither contains the other: ambiguous, so a full SHA is required.
        let diverged = commit_file(&repo, "main-only.txt", "main");
        sh(&repo, &["branch", "-f", "magi/27b2/A", &diverged]);
        let err = start(repo.clone()).await.unwrap_err().to_string();
        assert!(err.contains("diverged"), "{err}");
        assert!(
            Sources::resolve(&repo, "origin", None, &["magi/27b2/A".into()])
                .await
                .is_err()
        );
        sh(&repo, &["checkout", "-q", "-b", "other"]);
        commit_file(&repo, "other.txt", "other");
        sh(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@localhost",
                "merge",
                "--no-ff",
                "-m",
                "merge",
                &fix,
            ],
        );
        let merge = git::rev_parse(&repo, "HEAD").await.unwrap();
        let picks = Sources::resolve(&repo, "origin", None, &[merge])
            .await
            .unwrap();
        assert!(picks.plan(&repo, &base).await.is_err());
    }

    #[tokio::test]
    async fn a_base_only_commit_can_be_picked_onto_an_explicit_start() {
        let (_g, repo, _, fix, _) = repo_with_side_branch();
        // main moves on after the side branch forked: B is in the base, not in F.
        let b = commit_file(&repo, "b.txt", "b\n");
        let base = b.clone();
        let onto_start = Sources::resolve(
            &repo,
            "origin",
            Some("magi/27b2/A"),
            std::slice::from_ref(&b),
        )
        .await
        .unwrap();
        let plan = onto_start.plan(&repo, &base).await.unwrap();
        assert_eq!(plan.start.as_deref(), Some(fix.as_str()));
        assert_eq!(plan.picks, vec![b.clone()]);
        // Without an explicit start the candidates begin at the base, which
        // already contains B: picking it again is a duplicate.
        let onto_base = Sources::resolve(&repo, "origin", None, std::slice::from_ref(&b))
            .await
            .unwrap();
        assert!(onto_base.plan(&repo, &base).await.is_err());
        // A start that already contains the picked commit rejects it too.
        let contained = Sources::resolve(
            &repo,
            "origin",
            Some("magi/27b2/A"),
            std::slice::from_ref(&fix),
        )
        .await
        .unwrap();
        assert!(contained.plan(&repo, &base).await.is_err());
    }

    #[tokio::test]
    async fn explicit_sources_are_pinned_and_legacy_text_is_inert() {
        let (_g, repo, base, fix, _) = repo_with_side_branch();
        let sources = Sources::resolve(&repo, "origin", Some("magi/27b2/A"), &[])
            .await
            .unwrap();
        sh(&repo, &["branch", "-f", "magi/27b2/A", &base]);
        assert_eq!(
            sources.plan(&repo, &base).await.unwrap().start,
            Some(fix.clone())
        );
        let picks = Sources::resolve(&repo, "origin", None, std::slice::from_ref(&fix))
            .await
            .unwrap();
        let saved = serde_json::to_string(&picks).unwrap();
        let restored: Sources = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            restored.plan(&repo, &base).await.unwrap().picks,
            vec![fix.clone()]
        );
        assert!(
            Sources::resolve(&repo, "origin", Some("missing"), &[])
                .await
                .is_err()
        );
        let duplicate = Sources::resolve(&repo, "origin", Some(&fix), std::slice::from_ref(&fix))
            .await
            .unwrap();
        assert!(duplicate.plan(&repo, &base).await.is_err());
        assert_eq!(
            Sources::default().plan(&repo, &base).await.unwrap(),
            Plan::default()
        );
    }
}
