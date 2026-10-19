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
