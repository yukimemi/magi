//! Shared manual and owner-choice resume entry point.
use anyhow::{Context as _, Result, ensure};
use std::path::Path;

use crate::{
    queue::{Queue, Task, TaskStatus},
    run::RunState,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GateRepairRetry {
    pub(crate) run: String,
    pub(crate) head: String,
    evidence: [u8; 32],
    at: jiff::Timestamp,
}

impl GateRepairRetry {
    fn evidence(state: &RunState) -> Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&(
            &state.gate_head,
            &state.gate,
            &state.gate_fixes,
            &state.gate_repair_e2e,
            state.reviews.last(),
            &state.config.verify,
            state.winner().map(|c| (&c.branch, &c.worktree)),
        ))?;
        Ok(Sha256::digest(bytes).into())
    }

    pub(crate) fn queued(task: &Task) -> Option<&Self> {
        let retry = task.gate_repair_retry.as_ref()?;
        let pin = task.resume_override.as_ref()?;
        (task.status == TaskStatus::Queued
            && pin.forced
            && pin.at == retry.at
            && pin.pinned_run.as_ref() == Some(&retry.run)
            && task.operator_decided_at == Some(retry.at))
        .then_some(retry)
    }

    fn check_owner(task: &Task, state: &RunState) -> Result<()> {
        ensure!(
            task.execution_runs().last() == Some(&state.id)
                && state.origin.as_ref().and_then(|o| o.task.as_ref()) == Some(&task.id)
                && task.assignment_accepts(state)
                && state.repo.canonicalize()? == task.repo.canonicalize()?
                && !task.fresh_start
                && state
                    .repair_grants
                    .last()
                    .is_none_or(|grant| grant.succeeded)
                && !crate::repair_budget::pending(task, state)
                && crate::daemon::compatible_run_instruction(state, task),
            "post-repair E2E retry requires the same current task, run and instruction"
        );
        Ok(())
    }

    pub(crate) fn check(&self, task: &Task, state: &RunState) -> Result<()> {
        Self::check_owner(task, state)?;
        ensure!(
            self.run == state.id
                && task.operator_decided_at == Some(self.at)
                && crate::handover::gate_repair_stop(state)
                && state.gate_repair_retry_pass.is_none()
                && state
                    .gate_repair_e2e
                    .as_ref()
                    .is_some_and(|e| e.head == self.head)
                && self.evidence == Self::evidence(state)?,
            "saved post-repair E2E checkpoint changed or its retry was already spent"
        );
        Ok(())
    }

    pub(crate) async fn check_head(&self, state: &RunState) -> Result<()> {
        let winner = state
            .winner()
            .context("post-repair E2E retry has no retained winner")?;
        let holder = crate::git::worktree_holding(&state.repo, &winner.branch)
            .await?
            .context("retained winner branch has no worktree")?;
        let path = winner.worktree.canonicalize()?;
        ensure!(
            !winner.folded
                && path.parent() == Some(state.worktree_root().canonicalize()?.as_path())
                && holder.canonicalize()? == path
                && crate::git::current_branch(&path).await?.as_deref()
                    == Some(winner.branch.as_str())
                && crate::git::rev_parse(&state.repo, &format!("refs/heads/{}", winner.branch))
                    .await?
                    == self.head
                && crate::git::rev_parse(&path, "HEAD").await? == self.head
                && crate::git::is_clean(&path).await?,
            "post-repair E2E retry does not match the clean retained winner worktree and head"
        );
        Ok(())
    }
}

/// Whether an explicit post-repair request was prepared for scheduler dispatch.
pub fn has_post_repair_pin(task: &Task) -> bool {
    GateRepairRetry::queued(task).is_some()
}

pub(crate) fn owner_refusal(
    state: &RunState,
    home: &Path,
) -> Option<crate::handover::ResumeRefusal> {
    crate::handover::resume_refusal_under(state, Some(home)).filter(|reason| {
        !matches!(
            reason,
            crate::handover::ResumeRefusal::GateRepairE2eOwnerResume
        )
    })
}

/// Preflight is repeated by Runner's claim under the execution lease before
/// any graph state is changed. A queued response is never evidence of a claim.
pub fn validate(home: &Path, id: &str) -> Result<RunState> {
    ensure!(crate::run::is_run_id(id), "invalid run id");
    crate::task_reassign::ensure_current(&crate::queue::Queue::at(home.join("queue")), id)?;
    let _owner = crate::pr_head::owner_lock(home, id)?;
    let state = crate::run::RunState::load_under(id, home)?;
    ensure!(
        state.pending_sync_only().is_none(),
        "base-only synchronization has not succeeded; resume with --sync-base-only before verifying"
    );
    if let Some(reason) = owner_refusal(&state, home) {
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
        task.execution_runs().last().is_some_and(|run| run == id)
            && task.status != crate::queue::TaskStatus::Done,
        "run queue owner changed"
    );
    // A crashed worker can leave its task Running after releasing its claim
    // and execution lease. Preflight checked run liveness, and the task claim
    // acquired above excludes a concurrent queue driver; no scheduler reclaim
    // is required before this explicit resume.
    ensure!(
        task.sync_only.is_none(),
        "the task has a pending base-only pass"
    );
    prepare_owner_pin(&queue, &mut task, id, "resume", None)?;
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
    use crate::queue::TaskStatus;
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
    let preflight = || -> Result<()> {
        ensure!(
            task.execution_runs().last().map(String::as_str) == Some(run)
                && !matches!(task.status, TaskStatus::Running | TaskStatus::Done),
            "run queue owner changed"
        );
        ensure!(
            task.sync_only.is_none(),
            "the task has a pending base-only pass"
        );
        ensure!(
            match decided_before {
                Some(answered) => task.operator_decided_at.is_none_or(|at| at <= answered),
                None => true,
            },
            "resume was superseded by a newer operator decision"
        );
        Ok(())
    };
    preflight().map_err(BeforeQueueWrite)?;
    let home = queue
        .root()
        .parent()
        .context("queue has no home")
        .map_err(BeforeQueueWrite)?;
    let state = validate(home, run).map_err(BeforeQueueWrite)?;
    let post_repair = crate::handover::gate_repair_stop(&state);
    if applied.is_none()
        && task.status == TaskStatus::Queued
        && task
            .resume_override
            .as_ref()
            .is_some_and(|pin| pin.pinned_run.as_deref() == Some(run))
        && (!post_repair || GateRepairRetry::queued(&task).is_some())
    {
        return Ok(true);
    }
    prepare_pin(
        &state,
        &mut task,
        queue,
        if applied.is_some() { "choice" } else { "chat" },
        applied,
    )
    .map_err(BeforeQueueWrite)?;
    if let Some(id) = applied {
        task.mark_action_applied(id);
    }
    queue.put(&mut task)?;
    Ok(true)
}

/// Prepare a manual pin while the caller holds the owning task's claim. The
/// caller saves the task and pin together before handing it to a worker.
pub fn prepare_owner_pin(
    queue: &Queue,
    task: &mut Task,
    run: &str,
    source: &str,
    applied: Option<&str>,
) -> Result<()> {
    let home = queue.root().parent().context("queue has no home")?;
    let state = validate(home, run)?;
    prepare_pin(&state, task, queue, source, applied)
}

pub(crate) fn prepare_pin(
    state: &RunState,
    task: &mut Task,
    queue: &Queue,
    source: &str,
    applied: Option<&str>,
) -> Result<()> {
    ensure!(
        task.sync_only.is_none(),
        "the task has a pending base-only pass"
    );
    ensure!(
        state.pending_sync_only().is_none(),
        "base-only synchronization has not succeeded; resume with --sync-base-only before verifying"
    );
    let run = &state.id;
    if crate::handover::gate_repair_stop(state) {
        GateRepairRetry::check_owner(task, state)?;
        ensure!(task.status != TaskStatus::Done, "the owning task is done");
        let evidence = GateRepairRetry::evidence(state)?;
        let head = state.gate_repair_e2e.as_ref().unwrap().head.clone();
        release_standing(task);
        pin(task, run, applied);
        let at = task.resume_override.as_ref().unwrap().at;
        task.operator_decided_at = Some(at);
        task.gate_repair_retry = Some(GateRepairRetry {
            run: run.clone(),
            head,
            evidence,
            at,
        });
    } else {
        if !crate::repair_budget::queue_resumption(queue, task, run)? {
            task.release_operator(source, applied);
        }
        pin(task, run, applied);
    }
    Ok(())
}

/// Queue the same retained execution without manufacturing an operator release.
pub(crate) fn pin_standing(
    queue: &crate::queue::Queue,
    task: &mut crate::queue::Task,
    run: &str,
) -> Result<()> {
    ensure!(
        task.sync_only.is_none(),
        "the task has a pending base-only pass"
    );
    if !crate::repair_budget::queue_resumption(queue, task, run)? {
        release_standing(task);
    }
    pin(task, run, None);
    queue.put(task)
}

/// Reuse the release transition without refilling the task or recovery budgets.
pub(crate) fn release_standing(task: &mut crate::queue::Task) {
    let attempts = task.attempts;
    task.release();
    task.attempts = attempts;
}

fn pin(task: &mut crate::queue::Task, run: &str, applied: Option<&str>) {
    task.resume_override = Some(crate::queue::OperatorResume {
        question_id: applied.unwrap_or_default().to_owned(),
        at: jiff::Timestamp::now(),
        conductor_rehold: None,
        forced: true,
        pinned_run: Some(run.to_owned()),
    });
}
