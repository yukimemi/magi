//! Audited task completion and conservative, held-only reopening.
//!
//! All operator surfaces share admission checks. Task claims serialize queue
//! drivers; retained owner/fix/execution locks exclude direct run control until
//! the admitted task snapshot has been committed. These operations never alter
//! a run, a public pull request, or the attempt budget.
use std::path::Path;

use anyhow::{Context, Result, ensure};

use crate::{
    queue::{CompletionActor, CompletionContext, Queue, Task, TaskStatus},
    run::{Liveness, RunState, RunStatus},
};

/// Locks retained from admission through the caller's durable task commit.
/// Dropping these guards never changes any run or publication record.
#[must_use = "keep completion guards alive until the task decision is committed"]
pub struct CompletionGuards {
    _fixes: Vec<crate::graph::FixClaim>,
    _owners: Vec<crate::pr_head::OwnerLock>,
    _leases: Vec<crate::driver::ExecutionLease>,
}

fn completed(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Ready | RunStatus::Merged | RunStatus::AlreadyInBase
    )
}

/// Capture local evidence and require explicit abandonment of unfinished work.
/// Call [`admit_close`] when committing an operator decision; this local helper
/// alone does not exclude run owners or validate the forge.
pub fn prepare_close(
    task: &Task,
    home: &Path,
    actor: CompletionActor,
    reason: Option<String>,
    discard: bool,
) -> Result<CompletionContext> {
    ensure!(
        task.status != TaskStatus::Running,
        "task {} is running; stop its driver before closing it",
        task.id
    );
    let mut context = CompletionContext::new(actor, reason, home, task);
    context.discard = discard;
    validate_discard(&context)?;
    Ok(context)
}

fn validate_discard(context: &CompletionContext) -> Result<()> {
    if let Some(run) = &context.run {
        ensure!(
            context.discard
                || run.status.is_some_and(completed)
                || run.pr_state.as_deref() == Some("merged"),
            "latest run {} is {} (PR {}, state {}, branches {}); use --discard to explicitly abandon unfinished work{}",
            run.id,
            run.status.map_or("unknown", RunStatus::as_str),
            run.pr_url.as_deref().unwrap_or("none"),
            run.pr_state.as_deref().unwrap_or("none"),
            run.branches.join(", "),
            run.error
                .as_ref()
                .map_or(String::new(), |error| format!(": {error}")),
        );
    }
    Ok(())
}

async fn guard_runs(
    task: &Task,
    home: &Path,
    reopening: bool,
) -> Result<(CompletionGuards, Option<RunState>)> {
    let mut guards = CompletionGuards {
        _fixes: Vec::new(),
        _owners: Vec::new(),
        _leases: Vec::new(),
    };
    let mut latest = None;
    let ids: std::collections::BTreeSet<_> = task
        .runs
        .iter()
        .chain(task.startup_failures.iter().map(|failure| &failure.run))
        .collect();
    for id in ids {
        // Read before acquiring a fix lock: that lock creates its directory,
        // which must not make a missing retained run look like valid evidence.
        RunState::load_under(id, home)
            .with_context(|| format!("cannot verify retained run {id}; its outcome is unknown"))?;
        guards._fixes.push(
            crate::graph::FixClaim::acquire(&home.join("runs").join(id))
                .with_context(|| format!("run {id} targeted fix is active or unknown"))?,
        );
        guards._owners.push(
            crate::pr_head::owner_lock(home, id)
                .with_context(|| format!("run {id} ownership is busy"))?,
        );
        let mut state = RunState::load_under(id, home)
            .with_context(|| format!("cannot verify retained run {id}; its outcome is unknown"))?;
        let claims = crate::daemon::is_working_on(home, id, jiff::Timestamp::now());
        ensure!(
            state.liveness_under(home, claims) == Liveness::Dead,
            "run {id} driver is live or unknown; stop it before changing task completion"
        );
        ensure!(
            state.active.is_empty(),
            "run {id} retains active seats or commands; finish process cleanup first"
        );
        guards._leases.push(
            crate::driver::maintenance(home, id)
                .with_context(|| format!("run {id} execution is busy or unknown"))?,
        );
        if reopening {
            ensure!(
                !completed(state.status),
                "cannot reopen: retained run {id} is {} (PR {}, state {}); completed work must not execute again",
                state.status.as_str(),
                state.pr.as_ref().map_or("none", |pr| pr.url.as_str()),
                state.pr.as_ref().map_or("none", |pr| pr.state.as_str())
            );
            ensure!(
                !state.pr_publication_pending && !state.land_sync_pending,
                "cannot reopen: run {id} has unresolved publication/synchronization evidence"
            );
            ensure!(
                !state.pr.as_ref().is_some_and(|pr| pr.state == "merged")
                    && state.pr_merged_head.is_none(),
                "cannot reopen: run {id} retains merged public PR evidence"
            );
        }
        if let Some(pr) = &mut state.pr {
            let lifecycle = crate::land::lifecycle(&state.repo, &pr.url)
                .await
                .with_context(|| {
                    format!(
                        "run {id} PR {} forge state is unknown; no task decision was saved",
                        pr.url
                    )
                })?;
            ensure!(
                !reopening || lifecycle != crate::land::PrLifecycle::Merged,
                "cannot reopen: run {id} PR {} is merged on the forge",
                pr.url
            );
            pr.state = lifecycle.as_str().to_owned();
        } else if reopening {
            // A publication receipt may be missing after an interrupted save.
            // A winning branch must therefore also be checked on the forge.
            ensure!(
                state.winner().is_some()
                    || (state.pr_head.is_none() && state.pr_pushed_head.is_none()),
                "cannot reopen: run {id} has public head evidence but its publication outcome is unknown"
            );
            let merged = crate::land::find_external_merge(&state)
                .await
                .with_context(|| format!("run {id} public merge evidence is unknown"))?;
            ensure!(
                merged.is_none(),
                "cannot reopen: run {id} has a merged public PR {}",
                merged.as_ref().map_or("unknown", |pr| pr.url.as_str())
            );
        }
        if task.runs.last() == Some(id) {
            latest = Some(state);
        }
    }
    Ok((guards, latest))
}

/// Admit a close while the caller holds its task claim.
/// Keep the returned guards until the local-decision receipt and task save are
/// durable. Chat control uses this to commit its receipt and completion together.
pub async fn admit_close(
    task: &Task,
    home: &Path,
    actor: CompletionActor,
    reason: Option<String>,
    discard: bool,
) -> Result<(CompletionGuards, CompletionContext)> {
    ensure!(
        task.status != TaskStatus::Running,
        "task {} is running; stop its driver before closing it",
        task.id
    );
    let (guards, latest) = guard_runs(task, home, false).await?;
    let mut context = match latest {
        Some(state) => CompletionContext::from_run(actor, reason, task, &state),
        None => CompletionContext::new(actor, reason, home, task),
    };
    context.discard = discard;
    validate_discard(&context)?;
    Ok((guards, context))
}

/// Mark an idle task done with one immutable completion occurrence.
/// Already-done requests are idempotent and never create another notice.
pub async fn close(
    queue: &Queue,
    home: &Path,
    reference: &str,
    actor: CompletionActor,
    reason: Option<String>,
    discard: bool,
) -> Result<Task> {
    let id = queue.resolve_id(reference)?;
    let _claim = queue
        .claim(&id)
        .context("task is claimed; wait for its driver to stop")?;
    let mut task = queue.get(&id)?;
    if task.status == TaskStatus::Done {
        return Ok(task);
    }
    let expected = task.clone();
    let (_guards, context) = admit_close(&task, home, actor, reason, discard).await?;
    task.succeed(context);
    queue.commit_completion(&mut task, &expected)?;
    Ok(task)
}

/// Admit a held-only reopen while the caller holds the task claim.
/// Keep the returned guards through the atomic task/decision-receipt save.
pub async fn admit_reopen(
    task: &Task,
    home: &Path,
    actor: CompletionActor,
    reason: &str,
) -> Result<(CompletionGuards, CompletionContext)> {
    ensure!(
        !reason.trim().is_empty(),
        "reopen requires a non-empty reason"
    );
    ensure!(
        task.status == TaskStatus::Done,
        "only a done task can be reopened (currently {})",
        task.status.as_str()
    );
    let (guards, latest) = guard_runs(task, home, true).await?;
    let reason = Some(reason.to_owned());
    let context = match latest {
        Some(state) => CompletionContext::from_run(actor, reason, task, &state),
        None => CompletionContext::new(actor, reason, home, task),
    };
    Ok((guards, context))
}
/// Reopen a completed task into an explicit operator hold, never into execution.
/// Every retained run is checked, even runs from older repository assignments.
pub async fn reopen(
    queue: &Queue,
    home: &Path,
    reference: &str,
    actor: CompletionActor,
    reason: &str,
) -> Result<Task> {
    ensure!(
        !reason.trim().is_empty(),
        "reopen requires a non-empty reason"
    );
    let id = queue.resolve_id(reference)?;
    let _claim = queue
        .claim(&id)
        .context("task is claimed; wait for its driver to stop")?;
    let mut task = queue.get(&id)?;
    ensure!(
        task.status == TaskStatus::Done,
        "only a done task can be reopened (currently {})",
        task.status.as_str()
    );
    let expected = task.clone();
    let (_guards, context) = admit_reopen(&task, home, actor, reason).await?;
    task.reopen(context)?;
    queue.commit_completion(&mut task, &expected)?;
    Ok(task)
}

/// Human-readable audit and run/PR evidence for CLI and chat responses.
pub fn describe(task: &Task) -> String {
    let Some(event) = task.completion_history.last() else {
        return format!(
            "task {}: {} (legacy completion; actor and run evidence unknown)",
            task.id,
            task.status.as_str()
        );
    };
    let mut text = format!(
        "task {}: {}; occurrence {}; {:?} by {:?} at {}; prior {}; discard {}",
        task.id,
        task.status.as_str(),
        event.occurrence,
        event.action,
        event.actor,
        event.at,
        event.prior_status.as_str(),
        event.discard
    );
    if let Some(reason) = &event.reason {
        text.push_str(&format!("; reason: {reason}"));
    }
    for (label, value) in [
        ("prior hold", &event.prior_hold_reason),
        ("prior dependency", &event.prior_block_reason),
        ("prior error", &event.prior_error),
    ] {
        if let Some(value) = value {
            text.push_str(&format!("; {label}: {value}"));
        }
    }
    if let Some(run) = &event.run {
        text.push_str(&format!(
            "; run {} {}; PR {} {}; head {}; branches {}",
            run.id,
            run.status.map_or("unknown", RunStatus::as_str),
            run.pr_url.as_deref().unwrap_or("none"),
            run.pr_state.as_deref().unwrap_or("none"),
            run.pr_head.as_deref().unwrap_or("unknown"),
            run.branches.join(", ")
        ));
        if let Some(error) = &run.error {
            text.push_str(&format!("; snapshot error: {error}"));
        }
    } else {
        text.push_str("; no retained run");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{CompletionAction, Source};

    fn task() -> Task {
        Task::new(
            "audit".into(),
            "do the work".into(),
            "repo".into(),
            Source::Human,
        )
    }

    fn retained(home: &Path, task: &mut Task, status: RunStatus) -> RunState {
        let mut run = RunState::new(
            "repo".into(),
            "main".into(),
            "base".into(),
            "work".into(),
            crate::config::Config::default(),
        );
        run.status = status;
        run.save_under(home).unwrap();
        task.link_run(&run.id);
        run
    }

    #[tokio::test]
    async fn unfinished_work_requires_discard_and_records_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut task = task();
        let run = retained(dir.path(), &mut task, RunStatus::Blocked);
        task.hold_manual(Some("review".into()));
        queue.put(&mut task).unwrap();
        let error = close(
            &queue,
            dir.path(),
            &task.id,
            CompletionActor::Cli,
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("--discard"));
        let done = close(
            &queue,
            dir.path(),
            &task.id,
            CompletionActor::Cli,
            Some("abandon".into()),
            true,
        )
        .await
        .unwrap();
        let event = done.completion_history.last().unwrap();
        assert_eq!(event.run.as_ref().unwrap().id, run.id);
        assert_eq!(event.run.as_ref().unwrap().status, Some(RunStatus::Blocked));
        assert!(event.discard);
        assert_eq!(event.prior_status, TaskStatus::Held);
        assert!(describe(&done).contains(&run.id));
        assert_eq!(
            RunState::load_under(&run.id, dir.path()).unwrap().status,
            RunStatus::Blocked
        );
    }

    #[tokio::test]
    async fn reopening_is_held_reasoned_and_preserves_all_runs() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut task = task();
        retained(dir.path(), &mut task, RunStatus::Failed);
        retained(dir.path(), &mut task, RunStatus::Blocked);
        queue.put(&mut task).unwrap();
        let done = close(
            &queue,
            dir.path(),
            &task.id,
            CompletionActor::Web,
            None,
            true,
        )
        .await
        .unwrap();
        assert!(
            reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, " ")
                .await
                .is_err()
        );
        let held = reopen(
            &queue,
            dir.path(),
            &task.id,
            CompletionActor::Chat("talk".into()),
            "inspect before retry",
        )
        .await
        .unwrap();
        assert_eq!(held.status, TaskStatus::Held);
        assert_eq!(held.runs, done.runs);
        assert_eq!(held.attempts, done.attempts);
        assert_eq!(held.hold_reason.as_deref(), Some("inspect before retry"));
        assert_eq!(held.completion_history.len(), 2);
        assert_eq!(held.completion_history[1].action, CompletionAction::Reopen);
        assert!(!held.status.runnable());
    }

    #[tokio::test]
    async fn any_successful_retained_run_prevents_reopen() {
        for status in [
            RunStatus::Ready,
            RunStatus::Merged,
            RunStatus::AlreadyInBase,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let queue = Queue::at(dir.path().join("queue"));
            let mut task = task();
            retained(dir.path(), &mut task, status);
            retained(dir.path(), &mut task, RunStatus::Failed);
            task.succeed(CompletionContext::new(
                CompletionActor::DaemonSettle,
                None,
                dir.path(),
                &task,
            ));
            queue.put(&mut task).unwrap();
            let error = reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
                .await
                .unwrap_err();
            assert!(error.to_string().contains(status.as_str()), "{error:#}");
            assert_eq!(queue.get(&task.id).unwrap().status, TaskStatus::Done);
        }
    }

    #[tokio::test]
    async fn unknown_retained_record_and_active_claim_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut task = task();
        task.link_run("missing");
        task.succeed(CompletionContext::new(
            CompletionActor::Cli,
            None,
            dir.path(),
            &task,
        ));
        queue.put(&mut task).unwrap();
        let claim = queue.claim(&task.id).unwrap();
        assert!(
            reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
                .await
                .is_err()
        );
        drop(claim);
        let error = reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown"));
        assert_eq!(queue.get(&task.id).unwrap().completion_history.len(), 1);
        let stored = queue.get(&task.id).unwrap();
        let snapshot = stored.completion_history[0].run.as_ref().unwrap();
        assert_eq!(snapshot.id, "missing");
        assert_eq!(snapshot.status, None);
        assert!(snapshot.error.is_some(), "unreadable is not no run");
    }

    #[tokio::test]
    async fn legacy_completion_reopens_then_pins_the_same_run_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut task = task();
        let mut run = retained(dir.path(), &mut task, RunStatus::Blocked);
        run.candidates.push(crate::run::Candidate {
            index: 0,
            label: 'A',
            agent: "implementer".into(),
            branch: "retained-candidate-A".into(),
            worktree: dir.path().join("worktree-A"),
            summary: "retained implementation".into(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        });
        run.gate_head = Some("retained-checkpoint".into());
        run.gate_ran = true;
        run.save_under(dir.path()).unwrap();
        let before = serde_json::to_value(&run).unwrap();
        // Legacy Done has no event. Do not fabricate a historical actor.
        task.status = TaskStatus::Done;
        task.attempts = 2;
        queue.put(&mut task).unwrap();
        let mut raw = serde_json::to_value(&task).unwrap();
        raw.as_object_mut().unwrap().remove("completion_history");
        std::fs::write(queue.path_of(&task.id), serde_json::to_vec(&raw).unwrap()).unwrap();
        let held = reopen(
            &queue,
            dir.path(),
            &task.id,
            CompletionActor::Cli,
            "inspect the old close",
        )
        .await
        .unwrap();
        assert_eq!(held.status, TaskStatus::Held);
        assert_eq!(held.completion_history.len(), 1);
        assert_eq!(held.completion_history[0].action, CompletionAction::Reopen);
        assert_eq!(held.completion_history[0].prior_status, TaskStatus::Done);
        assert_eq!(held.runs, vec![run.id.clone()]);
        assert_eq!(held.attempts, 2);
        assert_eq!(
            held.completion_history[0].run.as_ref().unwrap().branches,
            vec!["retained-candidate-A"]
        );
        assert!(crate::resume::pin_queued(&queue, &run.id, None, None).unwrap());
        let queued = queue.get(&task.id).unwrap();
        assert_eq!(queued.status, TaskStatus::Queued);
        assert_eq!(queued.runs, held.runs);
        assert_eq!(
            queued.resume_override.unwrap().pinned_run.as_deref(),
            Some(run.id.as_str())
        );
        assert_eq!(
            serde_json::to_value(RunState::load_under(&run.id, dir.path()).unwrap()).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn owner_and_execution_locks_are_retained_until_the_task_commit() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut task = task();
        let run = retained(dir.path(), &mut task, RunStatus::Blocked);
        queue.put(&mut task).unwrap();
        let claim = queue.claim(&task.id).unwrap();
        let expected = queue.get(&task.id).unwrap();
        let (guards, context) = admit_close(&task, dir.path(), CompletionActor::Cli, None, true)
            .await
            .unwrap();
        assert!(crate::pr_head::owner_lock(dir.path(), &run.id).is_err());
        assert!(crate::driver::maintenance(dir.path(), &run.id).is_err());
        task.succeed(context);
        queue.commit_completion(&mut task, &expected).unwrap();
        assert!(crate::pr_head::owner_lock(dir.path(), &run.id).is_err());
        drop(guards);
        drop(claim);
        assert!(crate::pr_head::owner_lock(dir.path(), &run.id).is_ok());
        assert!(crate::driver::maintenance(dir.path(), &run.id).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forge_merge_and_unknown_forge_are_checked_before_task_changes() {
        if !crate::standalone_test(
            "task_completion::tests::forge_merge_and_unknown_forge_are_checked_before_task_changes",
        ) {
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf '%s\\n' '{\"state\":\"MERGED\"}'\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        // This fixture runs alone in a dedicated process, not alongside tests
        // or agents that could read a concurrently changing environment.
        unsafe {
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        }
        let queue = Queue::at(dir.path().join("queue"));
        let mut task = task();
        let mut run = retained(dir.path(), &mut task, RunStatus::Blocked);
        run.repo = dir.path().to_owned();
        run.pr = Some(crate::run::PrRecord {
            url: "https://example.invalid/pull/1".into(),
            number: 1,
            state: "open".into(),
            checks: "red".into(),
            round: 1,
            rounds: 1,
            red_at_merge: Vec::new(),
        });
        run.save_under(dir.path()).unwrap();
        queue.put(&mut task).unwrap();
        // A stale blocked record is not an unfinished PR once the forge proves
        // its merge. Closing it must not require claiming that work was discarded.
        let done = close(
            &queue,
            dir.path(),
            &task.id,
            CompletionActor::Cli,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!done.completion_history[0].discard);
        assert_eq!(
            done.completion_history[0]
                .run
                .as_ref()
                .unwrap()
                .pr_state
                .as_deref(),
            Some("merged")
        );
        assert_eq!(
            done.completion_history[0].run.as_ref().unwrap().status,
            Some(RunStatus::Blocked)
        );
        let error = reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("merged on the forge"),
            "{error:#}"
        );
        std::fs::write(
            &gh,
            "#!/bin/sh\nprintf '%s\\n' 'forge unavailable' >&2\nexit 1\n",
        )
        .unwrap();
        let error = reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown"), "{error:#}");
        assert_eq!(
            queue.get(&task.id).unwrap().completion_history,
            done.completion_history
        );
        assert_eq!(
            RunState::load_under(&run.id, dir.path())
                .unwrap()
                .pr
                .unwrap()
                .state,
            "open"
        );
    }
}
