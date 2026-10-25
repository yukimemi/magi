//! Shared manual and owner-choice resume entry point.
use anyhow::{Context as _, Result, ensure};
use std::path::Path;

/// Preflight is repeated by Runner's claim under the execution lease before
/// any graph state is changed. A queued response is never evidence of a claim.
pub(crate) fn validate(home: &Path, id: &str) -> Result<crate::run::RunState> {
    ensure!(crate::run::is_run_id(id), "invalid run id");
    let _owner = crate::pr_head::owner_lock(home, id)?;
    let state = crate::run::RunState::load_under(id, home)?;
    if let Some(reason) = crate::handover::resume_refusal_under(&state, Some(home)) {
        anyhow::bail!(reason.diagnostic(&state));
    }
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
        task.runs.last().is_some_and(|run| run == id)
            && task.status != crate::queue::TaskStatus::Done,
        "run queue owner changed"
    );
    // A crashed worker can leave its task Running after releasing its claim
    // and execution lease. Preflight checked run liveness, and the task claim
    // acquired above excludes a concurrent queue driver; no scheduler reclaim
    // is required before this explicit resume.
    task.release_operator("resume", None);
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
