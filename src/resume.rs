//! Shared manual and owner-choice resume entry point.
use anyhow::{Context as _, Result, ensure};
use std::path::Path;

/// Preflight is repeated by Runner's claim under the execution lease before
/// any graph state is changed. A queued response is never evidence of a claim.
pub(crate) fn validate(home: &Path, id: &str) -> Result<crate::run::RunState> {
    ensure!(crate::run::is_run_id(id), "invalid run id");
    crate::task_reassign::ensure_current(&crate::queue::Queue::at(home.join("queue")), id)?;
    let _owner = crate::pr_head::owner_lock(home, id)?;
    let state = crate::run::RunState::load_under(id, home)?;
    check_run(
        home,
        &state,
        crate::daemon::is_working_on(home, id, jiff::Timestamp::now()),
    )?;
    Ok(state)
}

/// Read-only part of resume admission. The caller must not treat this
/// observation as a replacement for acquiring the execution and owner locks.
pub(crate) fn check_run(home: &Path, state: &crate::run::RunState, claimed: bool) -> Result<()> {
    ensure!(
        state.liveness_under(home, claimed) != crate::run::Liveness::Live,
        "run already has a live driver"
    );
    check_checkpoint(home, state)
}

pub(crate) fn check_checkpoint(home: &Path, state: &crate::run::RunState) -> Result<()> {
    if let Some(reason) = crate::handover::resume_refusal_under(state, Some(home)) {
        anyhow::bail!(reason.diagnostic(state));
    }
    Ok(())
}

/// Resumes outside the queue must not bypass the daemon's execution limits.
/// Shared by Web requests and owner choices without a queue owner.
pub(crate) fn validate_unqueued(home: &Path, id: &str) -> Result<()> {
    validate(home, id)?;
    if let Some(work) = crate::daemon::current_work(home, jiff::Timestamp::now())
        .into_iter()
        .next()
    {
        anyhow::bail!(
            "the loop is running run {} right now; stop it first, or wait for \
             it to finish, before resuming a run by hand.",
            crate::run::short_of(&work.run)
        );
    }
    Ok(())
}

/// Adopt a legacy ownerless run into the same claimed task-worker path as CLI
/// resumes. Success acknowledges a durable worker, not an in-process future.
pub(crate) async fn spawn_detached(home: &Path, id: &str) -> Result<crate::worker::Job> {
    validate_unqueued(home, id)?;
    let state = crate::run::RunState::load_under(id, home)?;
    let queue = crate::queue::Queue::at(home.join("queue"));
    let _serial = queue
        .claim(&format!("resume-{id}"))
        .context("another resume request is claiming this run")?;
    let (mut task, claim) = crate::direct::claim_resume_owner(
        &queue,
        id,
        &state.instruction,
        &state.repo,
        crate::queue::Source::Human,
        &state.sources,
    )?;
    ensure!(
        task.execution_runs().last().is_some_and(|run| run == id)
            && task.status != crate::queue::TaskStatus::Done,
        "run queue owner changed"
    );
    // A crashed worker can leave its task Running after releasing its claim
    // and execution lease. Preflight checked run liveness, and the task claim
    // acquired above excludes a concurrent queue driver; no scheduler reclaim
    // is required before this explicit resume.
    if !crate::repair_budget::queue_resumption(&queue, &mut task, id)? {
        task.release_operator("resume", None);
    }
    task.resume_override = Some(crate::queue::OperatorResume {
        question_id: String::new(),
        at: jiff::Timestamp::now(),
        conductor_rehold: None,
        forced: true,
        pinned_run: Some(id.to_owned()),
    });
    queue.put(&mut task)?;
    let opts = crate::daemon::Opts {
        repo: task.repo.clone(),
        config: task
            .overrides
            .as_ref()
            .and_then(|options| options.config.clone()),
        ..Default::default()
    };
    crate::daemon::spawn_claimed(home, &opts, &queue, &task, claim).await
}

pub(crate) async fn start(home: &Path, id: String) -> Result<()> {
    spawn_detached(home, &id).await.map(|_| ())
}

#[derive(Debug, thiserror::Error)]
#[error("resume did not reach its queue write: {0:#}")]
pub(crate) struct BeforeQueueWrite(pub anyhow::Error);

pub(crate) fn check_queue_owner(
    task: &crate::queue::Task,
    run: &str,
    decided_before: Option<jiff::Timestamp>,
) -> Result<()> {
    use crate::queue::TaskStatus;
    ensure!(
        task.execution_runs().last().map(String::as_str) == Some(run)
            && !matches!(task.status, TaskStatus::Running | TaskStatus::Done),
        "run queue owner changed, is running, or is done"
    );
    ensure!(
        decided_before
            .is_none_or(|answered| task.operator_decided_at.is_none_or(|at| at <= answered)),
        "resume was superseded by a newer operator decision"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Unconfirmed(pub String);

/// Observe the queued maintenance path without creating locks or receipts.
pub(crate) fn inspect_queued(
    home: &Path,
    queue: &crate::queue::Queue,
    task: &crate::queue::Task,
    state: &crate::run::RunState,
) -> Result<()> {
    crate::task_reassign::ensure_current(queue, &state.id)?;
    check_queue_owner(task, &state.id, None)?;
    check_run(
        home,
        state,
        crate::daemon::is_working_on(home, &state.id, jiff::Timestamp::now()),
    )?;
    if state.liveness_under(home, false) == crate::run::Liveness::Unknown {
        return Err(Unconfirmed(
            "driver liveness is unknown; inspect the retained execution lease and driver identity"
                .into(),
        )
        .into());
    }
    check_pending_movement(queue, task, state)?;
    crate::repair_budget::check_queue_resumption(home, task, state)?;
    Ok(())
}

/// A request-backed approval must be published before ordinary resume can
/// use it. `true` means queueing must preserve the existing owner generation.
fn check_pending_movement(
    queue: &crate::queue::Queue,
    task: &crate::queue::Task,
    state: &crate::run::RunState,
) -> Result<bool> {
    if !state.base_sync_blocked() || !state.base_recovery_pending() {
        return Ok(false);
    }
    let head = crate::graph::Runner::base_recovery_head(state)?;
    state.check_base_recovery_head(&head)?;
    let authorization = state.base_sync_recovery.authorizations.last().unwrap();
    ensure!(
        authorization.task == task.id && !task.fresh_start && task.review_branch.is_none(),
        "base recovery task ownership or continuation changed"
    );
    if let Some(reason) = crate::base_recovery_control::queue_binding_refusal(authorization, task) {
        if let Some(key) = &authorization.request {
            anyhow::bail!(
                "pending base recovery cannot resume: {reason}; if queue publication was interrupted, complete the existing recover-base request {key} with the same ID and original arguments from base-recovery-controls/{key}.json, not ordinary resume"
            );
        }
        anyhow::bail!("pending base recovery cannot resume: {reason}");
    }
    if let Some(question) = &authorization.question {
        ensure!(
            crate::base_recovery_control::decision_current(queue, state, &task.id, question),
            "recovery decision ownership, routing or stop evidence changed"
        );
    }
    if let Some(scope) = &authorization.scope {
        ensure!(
            task.instruction == scope.instruction
                && state
                    .base_sync_recovery
                    .episodes
                    .last()
                    .is_some_and(|e| e.id == scope.episode),
            "base recovery task instruction or synchronization interval changed"
        );
    }
    Ok(authorization.request.is_some() || authorization.generation.is_some())
}

/// Hand a resumable run back to the queue task that owns it, pinned to that
/// exact run, so the loop's ordinary start logic resumes it under the graph's
/// execution lease. Never starts a driver itself. `Ok(false)` means no task
/// has this run as its latest, and the caller falls back to
/// [`validate_unqueued`]. The resume is the owner's explicit request (a chat
/// command or an answered question), so it also releases an operator hold.
/// `decided_before` (when the owner answered) refuses only when the owner
/// held or decided something newer than that; `applied` marks a question's
/// action as applied in the same write.
pub(crate) fn pin_queued(
    queue: &crate::queue::Queue,
    run: &str,
    decided_before: Option<jiff::Timestamp>,
    applied: Option<&str>,
) -> Result<bool> {
    crate::task_reassign::ensure_current(queue, run).map_err(BeforeQueueWrite)?;
    let Some(listed) = queue
        .list()
        .into_iter()
        .find(|t| t.runs.last().map(String::as_str) == Some(run))
    else {
        return Ok(false);
    };
    let _claim = queue.claim(&listed.id).map_err(BeforeQueueWrite)?;
    let mut task = queue.get(&listed.id).map_err(BeforeQueueWrite)?;
    check_queue_owner(&task, run, decided_before).map_err(BeforeQueueWrite)?;
    let home = queue
        .root()
        .parent()
        .context("queue has no home")
        .map_err(BeforeQueueWrite)?;
    let state = crate::run::RunState::load_under(run, home).map_err(BeforeQueueWrite)?;
    let preserve_movement =
        check_pending_movement(queue, &task, &state).map_err(BeforeQueueWrite)?;
    if applied.is_none()
        && task.status == crate::queue::TaskStatus::Queued
        && task
            .resume_override
            .as_ref()
            .is_some_and(|pin| pin.pinned_run.as_deref() == Some(run))
    {
        return Ok(true);
    }
    if !crate::repair_budget::queue_resumption_after_answer(home, &mut task, &state, applied)? {
        if preserve_movement {
            task.release();
        } else {
            task.release_operator(if applied.is_some() { "choice" } else { "chat" }, applied);
        }
    }
    task.resume_override = Some(crate::queue::OperatorResume {
        question_id: applied.unwrap_or_default().to_owned(),
        at: jiff::Timestamp::now(),
        conductor_rehold: None,
        forced: true,
        pinned_run: Some(run.to_owned()),
    });
    if let Some(id) = applied {
        task.mark_action_applied(id);
    }
    queue.put(&mut task)?;
    Ok(true)
}
