//! Reproduce a retained launcher's rebase without moving any repository refs.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::Selection;
use crate::{git, run::RunState};

/// Exact replay correspondence retained with the operator's selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProof {
    /// Retained historical run supplying the launcher provenance.
    pub run: String,
    /// Exact public update lease approved by the operator.
    pub public_head: String,
    /// Complete adopted implementation, including subsequent review fixes.
    pub source_head: String,
    /// Shared base before replaying the selected public implementation.
    pub old_base: String,
    /// Immutable base recorded by the successful launcher base-sync.
    pub new_base: String,
    /// Fetched landing base against which this selection was approved.
    pub current_base: String,
    /// Original commit, rebased commit and reproduced tree, in replay order.
    pub commits: Vec<(String, String, String)>,
    /// All later reviewed changes are adopted, rather than just the replayed tip.
    pub fixes: Vec<String>,
}

struct ReplayDir(std::path::PathBuf);

impl ReplayDir {
    fn new() -> Result<Self> {
        let path =
            std::env::temp_dir().join(format!("magi-source-replay-{:016x}", crate::rng::entropy()));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut private = builder;
            private.mode(0o700);
            private
        };
        builder
            .create(&path)
            .context("creating private source replay directory")?;
        Ok(Self(path))
    }
}

impl Drop for ReplayDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn linear(repo: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    ensure!(
        git::ancestry(repo, base, head).await?,
        "{base} is not an ancestor of {head}"
    );
    let raw = git::git(
        repo,
        &[
            "rev-list",
            "--reverse",
            "--parents",
            &format!("{base}..{head}"),
        ],
    )
    .await?;
    let mut previous = base.to_owned();
    let mut commits = Vec::new();
    for line in raw.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        ensure!(
            words.len() == 2 && words[1] == previous,
            "unsupported non-linear source history at {line}; inspect the retained run before reconstructing"
        );
        let tree = git::rev_parse(repo, &format!("{}^{{tree}}", words[0])).await?;
        let parent_tree = git::rev_parse(repo, &format!("{previous}^{{tree}}")).await?;
        ensure!(tree != parent_tree, "unsupported empty commit {}", words[0]);
        previous = words[0].to_owned();
        commits.push(previous.clone());
    }
    Ok(commits)
}

pub(super) async fn prove(
    selected: &Selection,
    repo: &Path,
    home: &Path,
    current_base: &str,
) -> Result<SourceProof> {
    let id = selected
        .source_run
        .as_deref()
        .context("missing --source-run")?;
    ensure!(
        crate::run::is_run_id(id),
        "source-run requires its full run id"
    );
    let run = RunState::load_under(id, home).with_context(|| {
        format!("missing or unreadable source-run {id}; restore retained run provenance")
    })?;
    let common = |path: &Path| {
        let path = path.to_owned();
        async move {
            git::git(
                &path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await
        }
    };
    ensure!(
        common(repo).await? == common(&run.repo).await?,
        "source-run repository differs from recovery repository"
    );
    let task = selected
        .branch
        .strip_prefix("magi/task-")
        .context("invalid selected task branch")?;
    ensure!(
        run.id == id && run.origin.as_ref().and_then(|o| o.task.as_deref()) == Some(task),
        "source-run belongs to another task"
    );
    let original = run.pr_recovery.as_ref().context(
        "source-run lacks its original public-head selection; ancestry cannot be inferred",
    )?;
    ensure!(
        original.url == selected.url
            && original.branch == selected.branch
            && original.head == selected.head
            && original.source() == selected.head,
        "source-run PR/public lease or original implementation does not match the selection"
    );
    let sync = run
        .base_sync
        .as_ref()
        .context("source-run lacks base-sync provenance")?;
    ensure!(
        sync.attempts > 0 && sync.conflict.is_none() && sync.already_in.is_none(),
        "source-run has no successful base-sync; inspect its conflict/provenance"
    );
    ensure!(
        git::ancestry(repo, &sync.tip, current_base).await?,
        "recorded rebase base {} is outside current landing base {current_base}",
        sync.tip
    );
    let source = git::rev_parse(repo, &format!("{}^{{commit}}", selected.source())).await?;
    ensure!(
        source == selected.source(),
        "source-head must be a full SHA"
    );
    ensure!(
        run.reviews.last().is_some_and(|r| r.head == source),
        "source {source} is not the source-run's final recorded review head; inspect reviews and retain the exact reviewed source"
    );
    let mut retained = false;
    for candidate in &run.candidates {
        if git::commit_of(repo, &candidate.branch).await.as_deref() == Some(&source)
            && run.events.iter().any(|e| {
                e.node == "land"
                    && e.message
                        == format!(
                            "rebased {} onto {}/{}",
                            candidate.branch, run.config.merge.remote, run.base_branch
                        )
            })
        {
            retained = true;
        }
    }
    ensure!(
        retained,
        "source {source} lacks a retained candidate ref and successful launcher rebase event in run {id}"
    );
    let old_base = git::merge_base(repo, &selected.head, &sync.tip).await?;
    let originals = linear(repo, &old_base, &selected.head).await?;
    let rebased = linear(repo, &sync.tip, &source).await?;
    ensure!(
        !originals.is_empty() && rebased.len() >= originals.len(),
        "source {source} lacks the complete public implementation {} (base {old_base}, onto {})",
        selected.head,
        sync.tip
    );

    // A private bare object database isolates replay from the retained repository.
    // No branch, checkout, index, sequencer or historical run record is modified.
    let temp = ReplayDir::new()?;
    let objects = temp.0.join("replay.git");
    git::git(
        repo,
        &[
            "clone",
            "--bare",
            "--shared",
            "--quiet",
            "--",
            &repo.to_string_lossy(),
            &objects.to_string_lossy(),
        ],
    )
    .await?;
    let mut parent = old_base.clone();
    let mut onto = sync.tip.clone();
    let mut commits = Vec::new();
    for (old, new) in originals.iter().zip(&rebased) {
        let replay = git::git_raw(
            &objects,
            &[
                "merge-tree",
                "--write-tree",
                &format!("--merge-base={parent}"),
                &onto,
                old,
            ],
        )
        .await?;
        ensure!(
            replay.ok(),
            "cannot reproduce {old} -> {new} onto {onto}: {}; {}; restore provenance or reconstruct and review explicitly",
            replay.stdout,
            replay.stderr
        );
        let tree = replay
            .stdout
            .lines()
            .next()
            .context("replay returned no tree")?
            .to_owned();
        let expected = git::rev_parse(repo, &format!("{new}^{{tree}}")).await?;
        if tree != expected {
            let paths = git::git(&objects, &["diff", "--name-status", &tree, &expected]).await?;
            anyhow::bail!(
                "modified implementation {old} -> {new} (base {old_base}, onto {}): tree {tree} != {expected}; differing paths:\n{paths}",
                sync.tip
            );
        }
        commits.push((old.clone(), new.clone(), tree));
        parent = old.clone();
        onto = new.clone();
    }
    Ok(SourceProof {
        run: id.to_owned(),
        public_head: selected.head.clone(),
        source_head: source,
        old_base,
        new_base: sync.tip.clone(),
        current_base: current_base.to_owned(),
        fixes: rebased[originals.len()..].to_vec(),
        commits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, pr_recovery::Disposition, run::Origin};
    use serde_json::json;

    async fn commit(repo: &Path, name: &str, contents: &str) -> String {
        std::fs::write(repo.join(name), contents).unwrap();
        git::git(repo, &["add", "."]).await.unwrap();
        git::git(repo, &["commit", "-qm", name]).await.unwrap();
        git::rev_parse(repo, "HEAD").await.unwrap()
    }

    async fn fixture() -> (tempfile::TempDir, Selection, RunState, String) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git::git(&repo, &["init", "-q", "-b", "main"])
            .await
            .unwrap();
        git::git(&repo, &["config", "user.name", "Test"])
            .await
            .unwrap();
        git::git(&repo, &["config", "user.email", "test@example.invalid"])
            .await
            .unwrap();
        let base = commit(&repo, "shared", "old\n\n\n\n\n\n\n\n\n\noriginal\n").await;
        git::git(&repo, &["checkout", "-qb", "public"])
            .await
            .unwrap();
        for i in 0..4 {
            commit(
                &repo,
                &format!("implementation-{i}"),
                &format!("selected {i}\n"),
            )
            .await;
        }
        let public = commit(
            &repo,
            "shared",
            "old\n\n\n\n\n\n\n\n\n\nmeasured implementation\n",
        )
        .await;
        git::git(&repo, &["checkout", "-q", "main"]).await.unwrap();
        commit(&repo, "new-main-only", "keep new main\n").await;
        let new_base = commit(&repo, "shared", "new main\n\n\n\n\n\n\n\n\n\noriginal\n").await;
        git::git(&repo, &["checkout", "-qb", "retained", &public])
            .await
            .unwrap();
        git::git(&repo, &["rebase", "--onto", &new_base, &base])
            .await
            .unwrap();
        let rebased = git::rev_parse(&repo, "HEAD").await.unwrap();
        let source = commit(&repo, "review-fix", "keep complete reviewed fix\n").await;
        let mut selected = Selection {
            url: "https://example.test/team/project/pull/1".into(),
            branch: "magi/task-t".into(),
            head: public,
            source_head: Some(source.clone()),
            source_run: None,
            source_proof: None,
            disposition: Some(Disposition::Land),
            instruction: "preserve implementation".into(),
            approved_at: jiff::Timestamp::now(),
        };
        let mut state = RunState::new(
            repo,
            "main".into(),
            new_base.clone(),
            selected.instruction.clone(),
            Config::default(),
        );
        state.origin = Some(Origin::queue("t"));
        let mut original = selected.clone();
        original.source_head = None;
        original.disposition = None;
        state.pr_recovery = Some(original);
        state.base_sync = Some(crate::run::BaseSync {
            tip: new_base.clone(),
            behind: 0,
            attempts: 1,
            conflict: None,
            already_in: None,
        });
        state.candidates.push(serde_json::from_value(json!({
            "index":0,"label":"A","agent":"existing branch","branch":"retained","worktree":"unused"
        })).unwrap());
        state.reviews = [rebased, source]
            .into_iter()
            .enumerate()
            .map(|(i, head)| {
                serde_json::from_value(json!({"round":i+1,"head":head,"reviews":[]})).unwrap()
            })
            .collect();
        state.event("land", "rebased retained onto origin/main");
        selected.source_run = Some(state.id.clone());
        state.save_under(temp.path()).unwrap();
        (temp, selected, state, new_base)
    }

    #[tokio::test]
    async fn actual_rebase_preserves_all_five_commits_main_and_later_review_fixes() {
        let (temp, mut selected, state, base) = fixture().await;
        assert!(!git::is_ancestor(&state.repo, &selected.head, selected.source()).await);
        let proof = prove(&selected, &state.repo, temp.path(), &base)
            .await
            .unwrap();
        assert_eq!(proof.commits.len(), 5);
        assert_eq!(proof.fixes, [selected.source()]);
        for i in 0..4 {
            assert_eq!(
                git::git(
                    &state.repo,
                    &["show", &format!("{}:implementation-{i}", selected.source())]
                )
                .await
                .unwrap(),
                format!("selected {i}")
            );
        }
        assert_eq!(
            git::git(
                &state.repo,
                &["show", &format!("{}:shared", selected.source())]
            )
            .await
            .unwrap(),
            "new main\n\n\n\n\n\n\n\n\n\nmeasured implementation"
        );
        assert_eq!(
            git::git(
                &state.repo,
                &["show", &format!("{}:review-fix", selected.source())]
            )
            .await
            .unwrap(),
            "keep complete reviewed fix"
        );
        assert_eq!(
            git::git(
                &state.repo,
                &["show", &format!("{}:new-main-only", selected.source())]
            )
            .await
            .unwrap(),
            "keep new main"
        );
        selected.source_proof = Some(Box::new(proof.clone()));
        let saved = serde_json::to_string(&selected).unwrap();
        let restored: Selection = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            restored.source_proof.as_deref(),
            Some(
                &prove(&restored, &state.repo, temp.path(), &base)
                    .await
                    .unwrap()
            )
        );
        assert!(git::status_porcelain(&state.repo).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_launcher_records_foreign_identity_and_wrong_base_are_rejected() {
        let (temp, selected, state, base) = fixture().await;
        assert!(
            prove(&selected, &state.repo, temp.path(), &selected.head)
                .await
                .unwrap_err()
                .to_string()
                .contains("outside current")
        );
        for missing in [
            "original-selection",
            "rebase-event",
            "retained-ref",
            "review-head",
            "task",
            "conflict",
        ] {
            let mut damaged = state.clone();
            match missing {
                "original-selection" => damaged.pr_recovery = None,
                "rebase-event" => damaged.events.clear(),
                "retained-ref" => damaged.candidates.clear(),
                "review-head" => damaged.reviews.last_mut().unwrap().head = selected.head.clone(),
                "task" => damaged.origin = Some(Origin::queue("other")),
                _ => damaged.base_sync.as_mut().unwrap().conflict = Some("conflicted".into()),
            }
            damaged.save_under(temp.path()).unwrap();
            assert!(
                prove(&selected, &state.repo, temp.path(), &base)
                    .await
                    .is_err(),
                "{missing}"
            );
        }
    }

    #[tokio::test]
    async fn saved_proof_is_recomputed_and_never_silently_replaced() {
        let (temp, mut selected, mut state, base) = fixture().await;
        selected
            .prepare_source(&state.repo, temp.path(), &base)
            .await
            .unwrap();
        selected
            .verify_source_under(&state.repo, temp.path())
            .await
            .unwrap();
        let proof = selected.source_proof.clone();
        assert!(
            selected
                .prepare_source(&state.repo, temp.path(), &base)
                .await
                .is_err()
        );
        assert_eq!(selected.source_proof, proof);
        selected.source_proof.as_mut().unwrap().commits[0].2 = "0".repeat(40);
        assert!(
            selected
                .verify_source_under(&state.repo, temp.path())
                .await
                .unwrap_err()
                .to_string()
                .contains("proof changed")
        );
        selected.source_proof = proof;
        state.base_sync = None;
        state.save_under(temp.path()).unwrap();
        assert!(
            selected
                .verify_source_under(&state.repo, temp.path())
                .await
                .unwrap_err()
                .to_string()
                .contains("base-sync")
        );
    }

    #[tokio::test]
    async fn unrelated_modified_missing_and_moved_public_evidence_fail_closed() {
        let (temp, mut selected, mut state, base) = fixture().await;
        let home = temp.path();
        let public = selected.head.clone();
        selected.head = base.clone();
        assert!(
            prove(&selected, &state.repo, home, &base)
                .await
                .unwrap_err()
                .to_string()
                .contains("lease")
        );
        selected.head = public;
        let sync = state.base_sync.take();
        state.save_under(home).unwrap();
        assert!(
            prove(&selected, &state.repo, home, &base)
                .await
                .unwrap_err()
                .to_string()
                .contains("base-sync")
        );
        state.base_sync = sync;
        state.save_under(home).unwrap();
        let mut legacy = serde_json::to_value(&selected).unwrap();
        legacy.as_object_mut().unwrap().remove("source_proof");
        legacy.as_object_mut().unwrap().remove("source_run");
        let legacy: Selection = serde_json::from_value(legacy).unwrap();
        assert!(
            legacy
                .verify_source(&state.repo)
                .await
                .unwrap_err()
                .to_string()
                .contains("--source-run")
        );
        // Make the final review metadata name a genuinely modified rebase result.
        git::git(&state.repo, &["checkout", "-q", "retained~1"])
            .await
            .unwrap();
        std::fs::write(
            state.repo.join("implementation-0"),
            "replaced implementation\n",
        )
        .unwrap();
        git::git(&state.repo, &["add", "."]).await.unwrap();
        git::git(&state.repo, &["commit", "--amend", "--no-edit", "-q"])
            .await
            .unwrap();
        let modified = commit(&state.repo, "review-fix", "later review\n").await;
        git::git(&state.repo, &["branch", "-f", "retained", &modified])
            .await
            .unwrap();
        selected.source_head = Some(modified.clone());
        state.reviews.last_mut().unwrap().head = modified;
        state.save_under(home).unwrap();
        let error = prove(&selected, &state.repo, home, &base)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("modified implementation") && error.contains("implementation-0"),
            "{error}"
        );
        git::git(&state.repo, &["checkout", "-q", "--orphan", "unrelated"])
            .await
            .unwrap();
        git::git(&state.repo, &["rm", "-rf", "."]).await.unwrap();
        let unrelated = commit(&state.repo, "other", "unrelated\n").await;
        selected.source_head = Some(unrelated.clone());
        state.reviews.last_mut().unwrap().head = unrelated.clone();
        git::git(&state.repo, &["branch", "-f", "retained", &unrelated])
            .await
            .unwrap();
        state.save_under(home).unwrap();
        assert!(prove(&selected, &state.repo, home, &base).await.is_err());
        selected.source_run = Some("20000101-000000-abcd".into());
        assert!(
            prove(&selected, &state.repo, home, &base)
                .await
                .unwrap_err()
                .to_string()
                .contains("provenance")
        );
    }

    /// Operator-supplied paths allow read-only validation of an actual retained history.
    #[tokio::test]
    #[ignore = "requires a local retained run and repository"]
    async fn inspect_retained_rebase_history() {
        let env = |name| std::env::var(name).unwrap();
        let home = std::path::PathBuf::from(env("RECOVERY_HOME"));
        let state = RunState::load_under(&env("RECOVERY_RUN"), &home).unwrap();
        let mut selected = state.pr_recovery.clone().unwrap();
        selected.source_run = Some(state.id.clone());
        selected.source_head = Some(env("RECOVERY_SOURCE"));
        let proof = prove(&selected, &state.repo, &home, &env("RECOVERY_BASE"))
            .await
            .unwrap();
        assert_eq!(proof.commits.len(), 5);
        assert_eq!(proof.fixes, [selected.source()]);
        eprintln!("{}", serde_json::to_string_pretty(&proof).unwrap());
    }
}
