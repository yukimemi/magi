//! Is a branch's change already in the base under a different commit id?
//!
//! The same change can reach the base twice over: a candidate's commit is
//! cherry-picked or squashed in by another task, and from then on the
//! original branch is a pile of commits `main` has no ancestry link to. Base
//! sync would try to rebase it and report a conflict with itself, and a pull
//! request or queue task for it would sit open for ever because the forge
//! never saw *that* branch merge.
//!
//! [`already_in`] is the pure decision and [`classify`] feeds it from git.
//! Two proofs are accepted, and nothing else:
//!
//! - **patch-id**: every commit the branch adds has a `-` line in
//!   `git cherry <base> <branch>`, i.e. the base carries a commit with the
//!   same diff. A merge commit has no patch-id, so a branch with one is never
//!   proven this way.
//! - **tree**: merging the branch into the base changes nothing, which also
//!   covers a squash of several commits into one.
//!
//! A branch that is only partly in the base, or whose re-land needed a
//! conflict resolution (so its patch-id changed), is [`AlreadyIn::No`]: a miss
//! costs an operator a look, a false positive would close live work.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::git;
use crate::proc::Quiet as _;

/// How many base commits are scanned for the twin of a matched branch commit.
/// Past this the proof still stands; only the pairing in the report is lost.
const PAIRING_LIMIT: usize = 1000;

/// Which proof established that a branch is already in the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proof {
    /// Every added commit has a patch-id twin on the base.
    PatchId,
    /// Merging the branch into the base yields the base's own tree.
    Tree,
}

impl Proof {
    /// The word reports and events use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PatchId => "patch-id",
            Self::Tree => "tree",
        }
    }
}

/// The verdict of [`already_in`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlreadyIn {
    /// Everything the branch adds is represented on the base.
    Yes(Proof),
    /// Not proven: partly present, genuinely different, or nothing added.
    No,
}

/// Pure decision. `added` is every commit in `<merge-base>..<branch>`;
/// `unmatched` and `matched` are the `+` and `-` lines of `git cherry`;
/// `tree_unchanged` says a merge of the branch into the base left the base's
/// tree as it was.
pub fn already_in(
    added: &[String],
    unmatched: &[String],
    matched: &[String],
    tree_unchanged: bool,
) -> AlreadyIn {
    if added.is_empty() {
        return AlreadyIn::No;
    }
    // `git cherry` skips merge commits, so a commit it did not mention is one
    // whose equivalence was never tested.
    let untested = added
        .iter()
        .any(|c| !unmatched.contains(c) && !matched.contains(c));
    if unmatched.is_empty() && !matched.is_empty() && !untested {
        return AlreadyIn::Yes(Proof::PatchId);
    }
    if tree_unchanged {
        return AlreadyIn::Yes(Proof::Tree);
    }
    AlreadyIn::No
}

/// Where the branch's change lives on the base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// How it was proven.
    pub proof: Proof,
    /// The base tip the check ran against.
    pub tip: String,
    /// The base commits carrying the same change. Empty for a tree proof, or
    /// when the twins could not be paired up; `tip` is then the only pointer.
    #[serde(default)]
    pub commits: Vec<String>,
}

impl Evidence {
    /// The commit to name: the first twin, else the base tip.
    pub fn commit(&self) -> &str {
        self.commits
            .first()
            .map_or(self.tip.as_str(), String::as_str)
    }

    /// Short ids of everything named, for one line of prose.
    pub fn names(&self) -> String {
        if self.commits.is_empty() {
            return short_sha(&self.tip).to_owned();
        }
        self.commits
            .iter()
            .map(|c| short_sha(c))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The first seven characters of an object id.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Ask git whether `head` is already represented in `base`. Both are pinned
/// to ids first so a ref that moves mid-check cannot split the answer.
pub async fn classify(repo: &Path, base: &str, head: &str) -> Result<Option<Evidence>> {
    let base = git::rev_parse(repo, base).await?;
    let head = git::rev_parse(repo, head).await?;
    let merge_base = git::git(repo, &["merge-base", &base, &head]).await?;
    let range = format!("{merge_base}..{head}");
    let added: Vec<String> = git::git(repo, &["rev-list", &range])
        .await?
        .lines()
        .map(str::to_owned)
        .collect();
    if added.is_empty() {
        return Ok(None);
    }
    let (unmatched, matched) = git::cherry(repo, &base, &head).await?;
    let tree_unchanged = tree_unchanged(repo, &base, &head).await;
    let proof = match already_in(&added, &unmatched, &matched, tree_unchanged) {
        AlreadyIn::Yes(p) => p,
        AlreadyIn::No => return Ok(None),
    };
    let commits = match proof {
        Proof::PatchId => twins(repo, &merge_base, &base, &head, &matched)
            .await
            .unwrap_or_default(),
        Proof::Tree => Vec::new(),
    };
    Ok(Some(Evidence {
        proof,
        tip: base,
        commits,
    }))
}

/// Does merging `head` into `base` give exactly `base`'s tree? A conflict, an
/// old git without `merge-tree --write-tree`, or any error is "no".
async fn tree_unchanged(repo: &Path, base: &str, head: &str) -> bool {
    let Ok(merged) = git::git_raw(repo, &["merge-tree", "--write-tree", base, head]).await else {
        return false;
    };
    if !merged.ok() {
        return false;
    }
    let Some(tree) = merged.stdout.lines().next() else {
        return false;
    };
    let base_tree = format!("{base}^{{tree}}");
    git::rev_parse(repo, &base_tree)
        .await
        .is_ok_and(|t| t == tree.trim())
}

/// Patch-ids of every non-merge commit in `range`, as `(patch_id, commit)`.
async fn patch_ids(repo: &Path, range: &str) -> Result<Vec<(String, String)>> {
    let log = git::git(repo, &["log", "-p", "--no-merges", "--no-color", range]).await?;
    let mut child = Command::new("git")
        .args(["patch-id", "--stable"])
        .current_dir(repo)
        .quiet()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn git patch-id")?;
    let mut stdin = child.stdin.take().context("git patch-id stdin")?;
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(log.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
    });
    let out = child.wait_with_output().await.context("git patch-id")?;
    let _ = feed.await;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (p, c) = l.split_once(' ')?;
            Some((p.to_owned(), c.trim().to_owned()))
        })
        .collect())
}

/// The base commits whose patch-id equals one of `matched` (branch commits).
async fn twins(
    repo: &Path,
    merge_base: &str,
    base: &str,
    head: &str,
    matched: &[String],
) -> Result<Vec<String>> {
    let count = git::commits_ahead(repo, merge_base, base).await?;
    if count > PAIRING_LIMIT {
        return Ok(Vec::new());
    }
    let ours = patch_ids(repo, &format!("{merge_base}..{head}")).await?;
    let theirs = patch_ids(repo, &format!("{merge_base}..{base}")).await?;
    let mut out: Vec<String> = Vec::new();
    for (pid, commit) in &ours {
        if !matched.contains(commit) {
            continue;
        }
        if let Some((_, twin)) = theirs.iter().find(|(p, _)| p == pid)
            && !out.contains(twin)
        {
            out.push(twin.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn every_commit_matched_is_already_in() {
        assert_eq!(
            already_in(&v(&["a", "b"]), &[], &v(&["a", "b"]), false),
            AlreadyIn::Yes(Proof::PatchId)
        );
    }

    #[test]
    fn a_partial_overlap_is_not_already_in() {
        assert_eq!(
            already_in(&v(&["a", "b"]), &v(&["b"]), &v(&["a"]), false),
            AlreadyIn::No
        );
    }

    #[test]
    fn a_merge_commit_cherry_never_saw_blocks_the_patch_id_proof() {
        assert_eq!(
            already_in(&v(&["a", "m"]), &[], &v(&["a"]), false),
            AlreadyIn::No
        );
        assert_eq!(
            already_in(&v(&["a", "m"]), &[], &v(&["a"]), true),
            AlreadyIn::Yes(Proof::Tree)
        );
    }

    #[test]
    fn a_squash_is_proven_by_the_tree() {
        assert_eq!(
            already_in(&v(&["a", "b"]), &v(&["a", "b"]), &[], true),
            AlreadyIn::Yes(Proof::Tree)
        );
    }

    #[test]
    fn nothing_added_is_never_already_in() {
        assert_eq!(already_in(&[], &[], &[], true), AlreadyIn::No);
    }

    #[test]
    fn evidence_names_the_twin_or_else_the_tip() {
        let e = Evidence {
            proof: Proof::PatchId,
            tip: "1234567890".into(),
            commits: v(&["abcdef0123"]),
        };
        assert_eq!(e.commit(), "abcdef0123");
        assert_eq!(e.names(), "abcdef0");
        let t = Evidence {
            proof: Proof::Tree,
            tip: "1234567890".into(),
            commits: Vec::new(),
        };
        assert_eq!(t.commit(), "1234567890");
        assert_eq!(t.names(), "1234567");
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .quiet()
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn commit(dir: &Path, file: &str, body: &str, msg: &str) -> String {
        std::fs::write(dir.join(file), body).unwrap();
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "-q", "-m", msg]);
        sh(dir, &["rev-parse", "HEAD"])
    }

    fn repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "user.email", "t@example.com"]);
        commit(&repo, "base.txt", "base\n", "base");
        (dir, repo)
    }

    #[tokio::test]
    async fn a_cherry_picked_reland_is_detected_and_names_the_twin() {
        let (_d, repo) = repo();
        sh(&repo, &["switch", "-q", "-c", "work"]);
        let orig = commit(&repo, "feat.txt", "feature\n", "feature");
        sh(&repo, &["switch", "-q", "main"]);
        commit(&repo, "other.txt", "other\n", "unrelated");
        sh(&repo, &["cherry-pick", &orig]);
        let twin = sh(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(twin, orig);
        let e = classify(&repo, "main", "work")
            .await
            .unwrap()
            .expect("already in");
        assert_eq!(e.proof, Proof::PatchId);
        assert_eq!(e.commits, vec![twin]);
    }

    #[tokio::test]
    async fn a_squashed_reland_is_detected_by_tree() {
        let (_d, repo) = repo();
        sh(&repo, &["switch", "-q", "-c", "work"]);
        commit(&repo, "a.txt", "a\n", "one");
        commit(&repo, "b.txt", "b\n", "two");
        sh(&repo, &["switch", "-q", "main"]);
        sh(&repo, &["merge", "--squash", "work"]);
        sh(&repo, &["commit", "-q", "-m", "squashed"]);
        commit(&repo, "later.txt", "later\n", "later");
        let e = classify(&repo, "main", "work")
            .await
            .unwrap()
            .expect("already in");
        assert_eq!(e.proof, Proof::Tree);
        assert!(e.commits.is_empty());
    }

    #[tokio::test]
    async fn a_partly_present_branch_is_not_already_in() {
        let (_d, repo) = repo();
        sh(&repo, &["switch", "-q", "-c", "work"]);
        let first = commit(&repo, "a.txt", "a\n", "one");
        commit(&repo, "b.txt", "b\n", "two");
        sh(&repo, &["switch", "-q", "main"]);
        sh(&repo, &["cherry-pick", &first]);
        assert!(classify(&repo, "main", "work").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_conflicting_branch_is_not_already_in() {
        let (_d, repo) = repo();
        sh(&repo, &["switch", "-q", "-c", "work"]);
        commit(&repo, "base.txt", "mine\n", "mine");
        sh(&repo, &["switch", "-q", "main"]);
        commit(&repo, "base.txt", "theirs\n", "theirs");
        assert!(classify(&repo, "main", "work").await.unwrap().is_none());
    }
}
