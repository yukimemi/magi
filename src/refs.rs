//! References a task makes to work that already exists in the repository.
//!
//! A task such as "land the fix that already lives on `magi/27b2/A`" is about
//! a change that is not on the base branch. Started from the base alone, the
//! candidates have nothing to land: the run ends with an empty branch. So the
//! task text is scanned for `magi/<run>/<label>` branch names and commit
//! shas, each is checked against the repository, and the ones that name
//! unmerged work seed the candidates (`Runner::prep`).
//!
//! Every judgement here is a question git can answer, and a question git
//! cannot answer (a shallow clone, a failing command) is reported as
//! unresolved rather than read as "not merged".

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

/// A candidate reference lifted out of prose, not yet checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The reference as written (shas lower-cased).
    pub text: String,
    /// A branch name rather than a sha.
    pub branch: bool,
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.')
}

/// Pull `magi/<run>/<label>` branch names and 7-40 digit hex shas out of
/// `text`, branch names first, then in order of first appearance, without
/// duplicates. A sha must
/// contain a digit, which drops words such as `defaced` and `facade`.
pub fn scan(text: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut push = |t: Token| {
        if !out.iter().any(|o| o.text == t.text) {
            out.push(t);
        }
    };
    let mut word = String::new();
    let flush = |word: &mut String, push: &mut dyn FnMut(Token)| {
        let w = word.trim_matches(|c| matches!(c, '.' | '-' | '/' | '_'));
        if let Some(rest) = w.strip_prefix("magi/") {
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
                push(Token {
                    text: w.to_owned(),
                    branch: true,
                });
            }
        } else if (7..=40).contains(&w.len())
            && w.chars().all(|c| c.is_ascii_hexdigit())
            && w.chars().any(|c| c.is_ascii_digit())
        {
            push(Token {
                text: w.to_ascii_lowercase(),
                branch: false,
            });
        }
        word.clear();
    };
    for c in text.chars() {
        if is_word(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut push);
        }
    }
    flush(&mut word, &mut push);
    // Branch names first: a sha that is one of them is then the same
    // reference, and the name is the one worth keeping.
    out.sort_by_key(|t| !t.branch);
    out
}

/// Check every reference in `text` against `repo`, relative to `base_commit`.
///
/// A bare hex string that names no commit is dropped silently (it was most
/// likely never a sha); a `magi/...` branch that cannot be found is kept as
/// [`SeedKind::Unresolved`] so the run says so.
pub async fn resolve(repo: &Path, base_commit: &str, remote: &str, text: &str) -> Vec<Seed> {
    let mut seeds: Vec<Seed> = Vec::new();
    for token in scan(text) {
        let found = match commit_for(repo, remote, &token).await {
            Some(sha) => sha,
            None => {
                if token.branch {
                    seeds.push(Seed {
                        token: token.text.clone(),
                        kind: SeedKind::Unresolved,
                        sha: String::new(),
                        branch: true,
                        detail: format!("no branch or commit named {}", token.text),
                    });
                }
                continue;
            }
        };
        // A sha that is some named branch's tip is the same reference.
        if seeds.iter().any(|s| s.sha == found) {
            continue;
        }
        let seed = match git::ancestry(repo, &found, base_commit).await {
            Ok(true) => {
                let holders = git::branches_containing(repo, &found)
                    .await
                    .map(|b| b.join(", "))
                    .unwrap_or_default();
                Seed {
                    token: token.text,
                    kind: SeedKind::AlreadyMerged,
                    sha: found,
                    branch: token.branch,
                    detail: holders,
                }
            }
            Ok(false) => {
                let holders = git::branches_containing(repo, &found)
                    .await
                    .map(|b| b.join(", "))
                    .unwrap_or_default();
                Seed {
                    token: token.text,
                    kind: SeedKind::Unmerged,
                    sha: found,
                    branch: token.branch,
                    detail: holders,
                }
            }
            Err(e) => Seed {
                token: token.text,
                kind: SeedKind::Unresolved,
                sha: found,
                branch: token.branch,
                detail: format!("could not tell whether the base contains it: {e}"),
            },
        };
        seeds.push(seed);
    }
    seeds
}

async fn commit_for(repo: &Path, remote: &str, token: &Token) -> Option<String> {
    if let Some(sha) = git::commit_of(repo, &token.text).await {
        // `magi/x/A` must be a branch, not e.g. a tag or path-like revision.
        if !token.branch || git::branch_exists(repo, &token.text).await.unwrap_or(false) {
            return Some(sha);
        }
    }
    if token.branch {
        return git::commit_of(repo, &format!("refs/remotes/{remote}/{}", token.text)).await;
    }
    None
}

/// Where the candidates start and what is applied on top.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Commit to branch the candidates from, in place of the base.
    pub start: Option<String>,
    /// Unmerged commits to cherry-pick onto the start, oldest first.
    pub picks: Vec<String>,
}

/// Turn resolved seeds into a starting point. Unmerged branches must form a
/// single line of history (each one contained in the next); two that diverge
/// cannot both be the start and guessing which the task meant is refused.
/// Unmerged bare shas already contained in the start are not picked twice.
pub async fn plan(repo: &Path, seeds: &[Seed]) -> anyhow::Result<Plan> {
    let mut start: Option<&Seed> = None;
    for s in seeds
        .iter()
        .filter(|s| s.kind == SeedKind::Unmerged && s.branch)
    {
        match start {
            None => start = Some(s),
            Some(cur) => {
                if git::ancestry(repo, &cur.sha, &s.sha).await? {
                    start = Some(s);
                } else if !git::ancestry(repo, &s.sha, &cur.sha).await? {
                    anyhow::bail!(
                        "the task names {} and {}, which have diverged; name one, \
                         or the commits to take, so magi does not have to guess",
                        cur.token,
                        s.token
                    );
                }
            }
        }
    }
    let mut picks = Vec::new();
    for s in seeds
        .iter()
        .filter(|s| s.kind == SeedKind::Unmerged && !s.branch)
    {
        if let Some(st) = start
            && git::ancestry(repo, &s.sha, &st.sha).await?
        {
            continue;
        }
        picks.push(s.sha.clone());
    }
    Ok(Plan {
        start: start.map(|s| s.sha.clone()),
        picks,
    })
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// One line per reference, as plain fact. `None` when there is nothing to say.
pub fn describe(seeds: &[Seed]) -> Option<String> {
    if seeds.is_empty() {
        return None;
    }
    let lines: Vec<String> = seeds
        .iter()
        .map(|s| match s.kind {
            SeedKind::Unmerged => format!(
                "- `{}` ({}) is NOT on the base branch{}",
                s.token,
                short(&s.sha),
                if s.detail.is_empty() {
                    String::new()
                } else {
                    format!("; contained in: {}", s.detail)
                }
            ),
            SeedKind::AlreadyMerged => format!(
                "- `{}` ({}) is already contained in the base branch{}",
                s.token,
                short(&s.sha),
                if s.detail.is_empty() {
                    String::new()
                } else {
                    format!("; contained in: {}", s.detail)
                }
            ),
            SeedKind::Unresolved => format!("- `{}` could not be resolved: {}", s.token, s.detail),
        })
        .collect();
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_branches_and_shas() {
        let t = scan(
            "Land the fix dc2e888 from `magi/27b2/A`, see also magi/27b2/A. \
             Not facade, not defaced, not magi/x.",
        );
        let texts: Vec<&str> = t.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["magi/27b2/A", "dc2e888"]);
        assert!(t[0].branch && !t[1].branch);
    }

    #[test]
    fn scan_ignores_short_or_long_hex() {
        assert!(scan("abc123 and 0123456789abcdef0123456789abcdef012345678").is_empty());
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
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
    async fn an_unmerged_branch_is_the_start_and_a_merged_sha_is_reported_as_such() {
        let (_g, repo, base, fix, merged) = repo_with_side_branch();
        let text = format!(
            "Land the fix {} from magi/27b2/A; {} is old",
            &fix[..8],
            &merged[..8]
        );
        let seeds = resolve(&repo, &base, "origin", &text).await;

        // The sha and the branch name are one reference.
        assert_eq!(seeds.len(), 2, "{seeds:?}");
        let branch = &seeds[0];
        assert_eq!(branch.token, "magi/27b2/A");
        assert_eq!(branch.kind, SeedKind::Unmerged);
        assert_eq!(branch.sha, fix);
        assert!(branch.detail.contains("magi/27b2/A"), "{}", branch.detail);
        assert_eq!(seeds[1].kind, SeedKind::AlreadyMerged);
        assert_eq!(seeds[1].sha, merged);

        let plan = plan(&repo, &seeds).await.unwrap();
        assert_eq!(plan.start.as_deref(), Some(fix.as_str()));
        assert!(plan.picks.is_empty());
        let facts = describe(&seeds).unwrap();
        assert!(facts.contains("NOT on the base branch"), "{facts}");
        assert!(facts.contains("already contained"), "{facts}");
    }

    #[tokio::test]
    async fn a_bare_unmerged_sha_is_cherry_picked_and_an_unknown_branch_is_reported() {
        let (_g, repo, base, fix, _) = repo_with_side_branch();
        let seeds = resolve(
            &repo,
            &base,
            "origin",
            &format!("take {} and magi/zzzz/A", &fix[..10]),
        )
        .await;
        assert_eq!(seeds.len(), 2, "{seeds:?}");
        assert_eq!(seeds[0].kind, SeedKind::Unresolved);
        assert_eq!(seeds[1].kind, SeedKind::Unmerged);
        assert!(!seeds[1].branch);

        let plan = plan(&repo, &seeds).await.unwrap();
        assert_eq!(plan.start, None);
        assert_eq!(plan.picks, vec![fix.clone()]);

        sh(&repo, &["checkout", "-q", "-b", "cand"]);
        git::cherry_pick(&repo, &fix).await.unwrap();
        assert!(repo.join("fix.txt").is_file());
    }

    #[tokio::test]
    async fn a_task_that_names_nothing_resolves_to_nothing() {
        let (_g, repo, base, _, _) = repo_with_side_branch();
        assert!(
            resolve(&repo, &base, "origin", "just fix the bug")
                .await
                .is_empty()
        );
    }
}
