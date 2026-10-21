//! Shared manual and owner-choice resume entry point.
use anyhow::{Result, ensure};
use std::path::Path;

/// Preflight is repeated by Runner's claim under the execution lease before
/// any graph state is changed. A queued response is never evidence of a claim.
pub(crate) fn validate(home: &Path, id: &str) -> Result<crate::run::RunState> {
    ensure!(crate::run::is_run_id(id), "invalid run id");
    let _owner = crate::pr_head::owner_lock(home, id)?;
    let state = crate::run::RunState::load_under(id, home)?;
    ensure!(
        state.status.resumable() && !state.released(),
        "run {} is `{}`, and only a stalled or blocked run can be resumed",
        state.short(),
        state.status.as_str()
    );
    ensure!(state.pr_taken_over_by.is_none(), "run ownership moved");
    let claims = crate::daemon::is_working_on(home, id, jiff::Timestamp::now());
    ensure!(
        state.liveness_under(home, claims) != crate::run::Liveness::Live,
        "run already has a live driver"
    );
    Ok(state)
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

pub(crate) async fn execute(id: &str) {
    match crate::graph::Runner::resume(id) {
        Ok(mut runner) => {
            if let Err(e) = runner.execute().await {
                tracing::warn!("resume of run {id} stopped: {e:#}");
            }
        }
        Err(e) => tracing::warn!("run {id} could not be resumed: {e:#}"),
    }
}

pub(crate) fn start(id: String) {
    tokio::spawn(async move { execute(&id).await });
}

/// Hand a resumable run back to the queue task that owns it, pinned to that
/// exact run, so the loop's ordinary start logic resumes it under the graph's
/// execution lease. Never starts a driver itself. `Ok(false)` means no task
/// has this run as its latest, and the caller falls back to
/// [`validate_unqueued`]. `decided_before` refuses when the owner held or
/// decided something newer than the request; `applied` marks a question's
/// action as applied in the same write.
pub(crate) fn pin_queued(
    queue: &crate::queue::Queue,
    run: &str,
    decided_before: Option<jiff::Timestamp>,
    applied: Option<&str>,
) -> Result<bool> {
    use crate::queue::TaskStatus;
    let Some(listed) = queue
        .list()
        .into_iter()
        .find(|t| t.runs.last().map(String::as_str) == Some(run))
    else {
        return Ok(false);
    };
    let _claim = queue.claim(&listed.id)?;
    let mut task = queue.get(&listed.id)?;
    ensure!(
        task.runs.last().map(String::as_str) == Some(run)
            && !matches!(task.status, TaskStatus::Running | TaskStatus::Done),
        "run queue owner changed"
    );
    ensure!(
        !task.operator_held()
            && match decided_before {
                Some(asked) => task.operator_decided_at.is_none_or(|at| at <= asked),
                None => true,
            },
        "resume was superseded by an operator hold or newer operator decision"
    );
    if applied.is_none()
        && task.status == TaskStatus::Queued
        && task
            .resume_override
            .as_ref()
            .is_some_and(|pin| pin.pinned_run.as_deref() == Some(run))
    {
        return Ok(true);
    }
    task.release_operator(if applied.is_some() { "choice" } else { "chat" }, applied);
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
