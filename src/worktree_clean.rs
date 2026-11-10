//! Explicit owned-worktree reset / pre-clean / clean boundary.
//! Callers retain the run driver claim; this module never searches other seats.
use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};

use crate::{git, run::RunState};

const PENDING: &str = "magi-pre-clean-pending";

/// Finish a recorded cleanup before base-sync can classify an already-moved
/// branch as synchronized. Legacy fixer records may have no Operation reservation.
/// Reading the Git file avoids extra subprocesses when no cleanup was started.
pub(crate) async fn resume_pending(state: &mut RunState, wt: &Path) -> Result<()> {
    let Ok(git_file) = std::fs::read_to_string(wt.join(".git")) else {
        return Ok(());
    };
    let Some(dir) = git_file.trim().strip_prefix("gitdir: ") else {
        return Ok(());
    };
    let marker = wt.join(dir).join(PENDING);
    let head = match std::fs::read_to_string(&marker) {
        Ok(head) => head,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("read unfinished pre-clean boundary"),
    };
    validate_owner(state, wt, false).await?;
    ensure!(
        git::rev_parse(wt, "HEAD").await? == head.trim()
            && git::is_clean(wt).await?
            && git::git(wt, &["ls-files", "--others", "-z"])
                .await?
                .is_empty(),
        "unfinished pre-clean in {} has changed HEAD, edits or untracked/ignored data; inspect before resuming synchronization",
        wt.display()
    );
    reset(state, wt, None).await?;
    // reset also removes it when hooks remain configured. A configuration
    // without hooks still has to finish the recorded Git clean first.
    match std::fs::remove_file(marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("complete unfinished pre-clean boundary"),
    }
}

/// Synchronize a candidate, or reset this run's numbered reviewer to `rev`.
/// The empty configuration follows the original Git path without admission.
pub(crate) async fn reset(state: &mut RunState, wt: &Path, rev: Option<&str>) -> Result<()> {
    if state.config.worktree.pre_clean.is_empty() {
        return match rev {
            Some(rev) => git::reset_detached(wt, rev).await,
            None => git::sync_to_head(wt).await,
        };
    }
    ensure!(
        std::env::var_os("MAGI_PRE_CLEAN_ACTIVE").is_none(),
        "recursive pre-clean invocation refused"
    );
    validate_owner(state, wt, rev.is_some()).await?;
    let dir = git::git(wt, &["rev-parse", "--absolute-git-dir"]).await?;
    let lock = crate::retention::OutputLease::pre_clean(Path::new(dir.trim()))?;
    let output = crate::retention::OutputLease::take(wt, true)?;
    let marker = Path::new(dir.trim()).join(PENDING);
    if rev.is_none() {
        std::fs::write(&marker, git::rev_parse(wt, "HEAD").await?)
            .context("record unfinished pre-clean boundary before reset")?;
    }
    let wait = crate::repo_lock::WaitPolicy::current();
    let coordination =
        crate::repo_lock::SeatLease::operation_with(wt, state.config.coordination.as_ref(), &wait)
            .await?;
    if let Some(rev) = rev {
        git::reset_detached_keep_ignored(wt, rev).await?;
    } else {
        git::reset_hard_head(wt).await?;
    }
    drop(coordination);
    let reason = if rev.is_some() {
        "reviewer reset"
    } else {
        "base synchronization"
    };
    let steps = state.config.worktree.pre_clean.clone();
    for (i, step) in steps.iter().enumerate() {
        let context = format!(
            "pre-clean {reason} in {} step {}/{}",
            wt.display(),
            i + 1,
            steps.len()
        );
        state.event("pre-clean", &context);
        state
            .save()
            .context("persist pre-clean attempt before starting command")?;
        let mut paths = vec![wt.to_owned()];
        if let Some(cache) = state.config.cache_dir() {
            paths.push(cache);
        }
        let mut outputs = crate::retention::OutputLease::admit(
            &crate::run::home(),
            &paths,
            state.config.disk.min_free_bytes,
        )?;
        outputs.push(lock.duplicate());
        outputs.push(output.duplicate());
        if let Some(cache) = state.config.cache_dir() {
            outputs.push(crate::retention::OutputLease::cache(
                &crate::run::home(),
                &cache,
                false,
            )?);
        }
        let lease = crate::repo_lock::SeatLease::operation_with(
            wt,
            state.config.coordination.as_ref(),
            &wait,
        )
        .await?;
        let result = crate::worktree_setup::run_command(
            &state.config.shell(),
            &step.run,
            wt,
            Duration::from_secs(
                step.timeout_secs
                    .unwrap_or_else(|| state.config.graph.verify_timeout()),
            ),
            lease,
            outputs,
            true,
        )
        .await
        .with_context(|| format!("{context}: git clean was not run"));
        if let Err(error) = result {
            state.event("pre-clean", format!("{error:#}"));
            if let Err(save) = state.save() {
                tracing::warn!("could not persist pre-clean failure: {save:#}; {error:#}");
            }
            return Err(error);
        }
    }
    let _coordination =
        crate::repo_lock::SeatLease::operation_with(wt, state.config.coordination.as_ref(), &wait)
            .await?;
    git::clean_all(wt)
        .await
        .with_context(|| format!("{reason}: git clean after pre-clean in {}", wt.display()))?;
    if rev.is_none() {
        std::fs::remove_file(marker).context("complete pre-clean boundary")?;
    }
    Ok(())
}

async fn validate_owner(state: &RunState, wt: &Path, reviewer: bool) -> Result<()> {
    let actual = wt.canonicalize()?;
    ensure!(
        actual != state.repo.canonicalize()?,
        "pre-clean refuses the primary checkout"
    );
    ensure!(
        wt.join(".git").is_file(),
        "pre-clean requires an owned linked worktree"
    );
    let root = state.worktree_root().canonicalize()?;
    ensure!(
        actual.parent() == Some(root.as_path()),
        "pre-clean target is outside this run's worktree root"
    );
    ensure!(
        actual == root.join(wt.file_name().context("missing worktree name")?),
        "pre-clean refuses a worktree alias"
    );
    let owned = if reviewer {
        wt.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("review-"))
            .and_then(|n| n.parse::<usize>().ok())
            .is_some_and(|n| {
                n > 0
                    && n <= state
                        .config
                        .resolve_roles()
                        .map_or(0, |r| r.reviewers.len())
            })
    } else {
        state.foreign_checkout.is_none() && state.candidates.iter().any(|c| c.worktree == wt)
    };
    ensure!(
        owned,
        "pre-clean target is not a recorded candidate or this run's reviewer"
    );
    ensure!(
        crate::repo_lock::common_dir(wt).await?
            == crate::repo_lock::common_dir(&state.repo).await?,
        "pre-clean target belongs to a different repository"
    );
    let git_dir = std::path::PathBuf::from(
        git::git(wt, &["rev-parse", "--absolute-git-dir"])
            .await?
            .trim(),
    )
    .canonicalize()?;
    let common = crate::repo_lock::common_dir(wt)
        .await?
        .context("missing Git common directory")?;
    ensure!(
        git_dir.parent() == Some(common.join("worktrees").as_path()),
        "pre-clean refuses an independent checkout"
    );
    let backref = std::fs::read_to_string(git_dir.join("gitdir"))?;
    ensure!(
        Path::new(backref.trim()).canonicalize()? == wt.join(".git").canonicalize()?,
        "pre-clean Git registration points at another worktree"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, PreCleanStep};
    use std::path::PathBuf;

    async fn fixture(commands: &[&str]) -> (tempfile::TempDir, RunState, PathBuf) {
        crate::run::pin_test_home();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            git::git(&repo, &args).await.unwrap();
        }
        std::fs::write(repo.join("tracked"), "original").unwrap();
        git::git(&repo, &["add", "."]).await.unwrap();
        git::git(&repo, &["commit", "-m", "initial"]).await.unwrap();
        let mut cfg = Config::default();
        cfg.graph.worktree_root = Some(tmp.path().join("worktrees"));
        cfg.disk.min_free_bytes = 0;
        cfg.worktree.pre_clean = commands
            .iter()
            .map(|run| PreCleanStep {
                run: (*run).into(),
                timeout_secs: Some(2),
            })
            .collect();
        let mut state = RunState::new(repo, "main".into(), "HEAD".into(), "test".into(), cfg);
        let wt = state.worktree_root().join("cand-A");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git::worktree_add_detached(&state.repo, &wt, "HEAD")
            .await
            .unwrap();
        let candidate =
            serde_json::json!({"index":0,"label":"A","agent":"test","branch":"main","worktree":wt});
        state
            .candidates
            .push(serde_json::from_value(candidate).unwrap());
        (tmp, state, wt)
    }

    #[tokio::test]
    async fn absent_hooks_preserve_original_clean_behavior() {
        let (_tmp, mut state, wt) = fixture(&[]).await;
        std::fs::write(wt.join("junk"), "x").unwrap();
        reset(&mut state, &wt, None).await.unwrap();
        assert!(!wt.join("junk").exists());
        assert!(
            !PathBuf::from(
                git::git(&wt, &["rev-parse", "--absolute-git-dir"])
                    .await
                    .unwrap()
                    .trim()
            )
            .join("magi-pre-clean.lock")
            .exists()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hooks_run_after_reset_in_order_in_only_the_target() {
        let (_tmp, mut state, wt) = fixture(&[
            "test \"$(cat tracked)\" = original && test -f junk && echo first > order",
            "test \"$(cat order)\" = first && test -z \"$MAGI_RUN$MAGI_NODE\" && echo done >> order",
        ]).await;
        std::fs::write(wt.join("tracked"), "changed").unwrap();
        std::fs::write(wt.join("junk"), "x").unwrap();
        let other = state.worktree_root().join("review-99");
        git::worktree_add_detached(&state.repo, &other, "HEAD")
            .await
            .unwrap();
        std::fs::write(other.join("junk"), "keep").unwrap();
        std::fs::write(state.repo.join("junk"), "keep").unwrap();
        reset(&mut state, &wt, None).await.unwrap();
        assert!(!wt.join("order").exists());
        assert!(other.join("junk").exists());
        assert!(state.repo.join("junk").exists());
    }

    #[tokio::test]
    async fn failure_stops_later_steps_and_clean_and_can_retry() {
        let (_tmp, mut state, wt) = fixture(&["echo diagnostic && exit 7", "exit 99"]).await;
        std::fs::write(wt.join("junk"), "keep").unwrap();
        let error = format!("{:#}", reset(&mut state, &wt, None).await.unwrap_err());
        assert!(
            error.contains("step 1/2")
                && error.contains("diagnostic")
                && error.contains("git clean was not run"),
            "{error}"
        );
        assert!(wt.join("junk").exists());
        state.config.worktree.pre_clean.truncate(1);
        state.config.worktree.pre_clean[0].run = "exit 0".into();
        reset(&mut state, &wt, None).await.unwrap();
        assert!(!wt.join("junk").exists());
    }

    #[tokio::test]
    async fn primary_and_unrecorded_worktrees_are_refused_before_reset() {
        let (_tmp, mut state, wt) = fixture(&["exit 99"]).await;
        let repo = state.repo.clone();
        std::fs::write(repo.join("tracked"), "keep").unwrap();
        assert!(reset(&mut state, &repo, None).await.is_err());
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked")).unwrap(),
            "keep"
        );
        state.candidates.clear();
        std::fs::write(wt.join("tracked"), "keep").unwrap();
        assert!(reset(&mut state, &wt, None).await.is_err());
        assert_eq!(std::fs::read_to_string(wt.join("tracked")).unwrap(), "keep");
    }

    #[tokio::test]
    async fn reviewer_reset_runs_only_in_its_own_checkout() {
        let (_tmp, mut state, candidate) = fixture(&["exit 0"]).await;
        state.config.agents = vec![
            serde_json::from_value(
                serde_json::json!({"id":"test", "kind":"command", "command":["unused"]}),
            )
            .unwrap(),
        ];
        state.config.roles.reviewers = vec!["test".into()];
        state.config.roles.implementers = vec!["test".into()];
        let reviewer = state.worktree_root().join("review-1");
        git::worktree_add_detached(&state.repo, &reviewer, "HEAD")
            .await
            .unwrap();
        std::fs::write(reviewer.join("junk"), "remove").unwrap();
        std::fs::write(candidate.join("junk"), "keep").unwrap();
        reset(&mut state, &reviewer, Some("HEAD")).await.unwrap();
        assert!(!reviewer.join("junk").exists());
        assert!(candidate.join("junk").exists());
    }

    #[tokio::test]
    async fn a_forged_git_file_cannot_redirect_cleanup_to_another_worktree() {
        let (_tmp, mut state, candidate) = fixture(&["exit 0"]).await;
        let impostor = state.worktree_root().join("impostor");
        std::fs::create_dir(&impostor).unwrap();
        std::fs::copy(candidate.join(".git"), impostor.join(".git")).unwrap();
        std::fs::write(impostor.join("tracked"), "keep").unwrap();
        state.candidates[0].worktree = impostor.clone();
        assert!(reset(&mut state, &impostor, None).await.is_err());
        assert_eq!(
            std::fs::read_to_string(impostor.join("tracked")).unwrap(),
            "keep"
        );
    }

    #[tokio::test]
    async fn legacy_sync_boundary_survives_failure_without_an_operation_reservation() {
        let (_tmp, mut state, wt) = fixture(&["exit 7"]).await;
        assert!(reset(&mut state, &wt, None).await.is_err());
        assert!(state.base_sync_recovery.operations.is_empty());
        std::fs::write(wt.join("local-data"), "keep").unwrap();
        state.config.worktree.pre_clean[0].run = "exit 0".into();
        assert!(resume_pending(&mut state, &wt).await.is_err());
        assert!(wt.join("local-data").exists());
        std::fs::remove_file(wt.join("local-data")).unwrap();
        // Even removing the hook configuration cannot skip the recorded clean.
        state.config.worktree.pre_clean.clear();
        resume_pending(&mut state, &wt).await.unwrap();
        resume_pending(&mut state, &wt).await.unwrap();
    }

    #[tokio::test]
    async fn moved_branch_cleanup_failure_leaves_sync_reservation_unfinished() {
        let (_tmp, mut state, wt) = fixture(&["exit 7"]).await;
        git::git(&wt, &["checkout", "-b", "candidate"])
            .await
            .unwrap();
        let old = git::rev_parse(&wt, "HEAD").await.unwrap();
        std::fs::write(state.repo.join("tracked"), "new base").unwrap();
        git::git(&state.repo, &["commit", "-am", "advance base"])
            .await
            .unwrap();
        let new = git::rev_parse(&state.repo, "HEAD").await.unwrap();
        state.reserve_base_operation(&old, &new);
        git::git(
            &state.repo,
            &["update-ref", "refs/heads/candidate", &new, &old],
        )
        .await
        .unwrap();
        std::fs::write(wt.join("cache"), "keep").unwrap();
        assert!(reset(&mut state, &wt, None).await.is_err());
        assert_eq!(git::rev_parse(&wt, "HEAD").await.unwrap(), new);
        assert!(!state.base_sync_recovery.operations.last().unwrap().finished);
        assert!(wt.join("cache").exists());
        // This is the existing recovery guard: untracked data after a ref move
        // requires inspection rather than rerunning a destructive reset.
        assert!(
            !git::git(&wt, &["ls-files", "--others", "-z"])
                .await
                .unwrap()
                .is_empty()
        );
        let saved = RunState::load(&state.id).unwrap();
        assert!(!saved.base_sync_recovery.operations.last().unwrap().finished);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Privileged processes can bypass the permission failure.
            if unsafe { libc::geteuid() } != 0 {
                let cache = wt.join("readonly-module");
                std::fs::create_dir(&cache).unwrap();
                std::fs::write(cache.join("data"), "cache").unwrap();
                std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o555)).unwrap();
                state.config.worktree.pre_clean[0].run = "exit 0".into();
                let result = reset(&mut state, &wt, None).await;
                std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).unwrap();
                assert!(
                    result.is_err(),
                    "git clean unexpectedly removed a read-only cache"
                );
                assert!(!state.base_sync_recovery.operations.last().unwrap().finished);
                let saved = RunState::load(&state.id).unwrap();
                assert!(!saved.base_sync_recovery.operations.last().unwrap().finished);
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_readonly_cache_uses_project_attribute_cleanup() {
        let (_tmp, mut state, wt) = fixture(&["attrib -R cache\\module"]).await;
        state.config.verify.shell = Some(vec!["cmd".into(), "/C".into()]);
        std::fs::create_dir_all(wt.join("cache")).unwrap();
        let file = wt.join("cache/module");
        std::fs::write(&file, "cache").unwrap();
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file, permissions).unwrap();
        reset(&mut state, &wt, None).await.unwrap();
        assert!(!file.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_readonly_cache_is_removed_before_clean() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, mut state, wt) =
            fixture(&["if test -d cache; then chmod u+w cache/module && rm -rf cache; fi"]).await;
        std::fs::create_dir_all(wt.join("cache/module")).unwrap();
        std::fs::write(wt.join("cache/module/file"), "cache").unwrap();
        std::fs::set_permissions(
            wt.join("cache/module"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        reset(&mut state, &wt, None).await.unwrap();
        assert!(!wt.join("cache").exists());
        reset(&mut state, &wt, None).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_reads_only_a_bounded_output_tail() {
        let (_tmp, mut state, wt) =
            fixture(&["head -c 100000 /dev/zero; echo last-diagnostic; exit 7"]).await;
        let error = format!("{:#}", reset(&mut state, &wt, None).await.unwrap_err());
        assert!(error.contains("last-diagnostic"));
        assert!(
            error.len() < 5000,
            "unbounded output: {} bytes",
            error.len()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_keeps_partial_output_and_never_cleans() {
        let (_tmp, mut state, wt) = fixture(&["echo partial; sleep 30", "touch after"]).await;
        state.config.worktree.pre_clean[0].timeout_secs = Some(1);
        std::fs::write(wt.join("junk"), "keep").unwrap();
        let error = format!("{:#}", reset(&mut state, &wt, None).await.unwrap_err());
        assert!(
            error.contains("timed out")
                && error.contains("partial")
                && error.contains("git clean was not run"),
            "{error}"
        );
        assert!(wt.join("junk").exists());
        assert!(!wt.join("after").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interruption_reaps_descendants_before_releasing_pre_clean_lock() {
        let (_tmp, mut state, wt) = fixture(&["sleep 30 & echo $! > child; wait"]).await;
        let target = wt.clone();
        let task = tokio::spawn(async move { reset(&mut state, &target, None).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !wt.join("child").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pid: u32 = std::fs::read_to_string(wt.join("child"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(crate::retention::OutputLease::take(&wt, true).is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let dir = PathBuf::from(
            git::git(&wt, &["rev-parse", "--absolute-git-dir"])
                .await
                .unwrap()
                .trim(),
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(_lock) = crate::retention::OutputLease::pre_clean(&dir)
                    && crate::retention::OutputLease::take(&wt, true).is_ok()
                {
                    assert!(
                        !crate::proc::pid_alive(pid),
                        "descendant survived lock release"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(crate::retention::OutputLease::take(&wt, true).is_ok());
        assert!(wt.join("child").exists());
    }
}
