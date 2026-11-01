//! Explicit reconciliation, never an automatic janitor or a retry authorization.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::run::{Liveness, RunState, RunStatus, Startup, StartupFailure};

/// Operator-supplied attribution, supported by the preserved worker log excerpt.
#[derive(Debug, Serialize, Deserialize)]
pub struct Evidence {
    /// Exact run identity; no prefix matching in an audit operation.
    pub run: String,
    /// Exact originating task identity.
    pub task: String,
    /// Branch the refused review attempted to acquire.
    pub branch: String,
    /// Relevant worker log excerpt, reviewed by the operator before applying.
    pub refusal_log: String,
}

/// Validate under task, owner and execution locks. Applying changes only startup
/// evidence, the empty run's status, and the task's separate startup history.
/// A repeat repairs a partially saved task link without duplicating evidence.
pub fn reconcile(home: &Path, evidence: &Evidence, apply: bool) -> Result<String> {
    ensure!(
        crate::run::is_run_id(&evidence.run),
        "exact run id required"
    );
    ensure!(
        crate::run::is_run_id(&evidence.task),
        "exact task id required"
    );
    ensure!(
        !evidence.branch.trim().is_empty() && !evidence.refusal_log.trim().is_empty(),
        "review branch and supporting refusal log required"
    );
    let queue = crate::queue::Queue::at(home.join("queue"));
    let _claim = queue.claim(&evidence.task)?;
    let _task_lock = queue.lock_task(&evidence.task)?;
    let _owner = crate::pr_head::owner_lock(home, &evidence.run)?;
    let mut task = queue.get(&evidence.task)?;
    let mut state = RunState::load_under(&evidence.run, home)?;
    ensure!(state.repo == task.repo, "repository attribution mismatch");
    ensure!(
        state.origin.as_ref().and_then(|o| o.task.as_deref()) == Some(evidence.task.as_str()),
        "origin must already identify the supplied task"
    );
    let now = jiff::Timestamp::now();
    ensure!(
        !crate::daemon::is_working_on(home, &state.id, now)
            && !crate::daemon::is_working_on_task(home, &task.id, now),
        "daemon claims this startup or task"
    );
    ensure!(
        !crate::worker::observe_active(home)?
            .iter()
            .any(|j| j.key == task.id
                || j.key.ends_with(&format!(":{}", task.id))
                || j.payload.to_string().contains(&task.id)
                || j.payload.to_string().contains(&state.id)),
        "worker evidence requires inspection"
    );
    ensure!(
        state.liveness_under(home, false) == Liveness::Dead,
        "startup liveness is live or unknown"
    );
    let _execution = crate::driver::maintenance(home, &state.id)?;
    // Never derive non-execution from an empty instruction or an unlocked lease.
    ensure!(
        state.driver_claimed == Some(false)
            && state.driver_pid.is_none()
            && state.driver_started_at.is_none()
            && state.passes.is_empty()
            && state.active.is_empty(),
        "execution evidence or unknown execution ownership"
    );
    ensure!(
        state.candidates.is_empty()
            && state.prep_complete == Some(false)
            && state.reviewed_commits.is_none()
            && state.pr_head.is_none()
            && state.released_to.is_none()
            && state.foreign_checkout.is_none()
            && !state.worktree_root().try_exists()?,
        "retained repository assets require manual inspection"
    );
    for entry in std::fs::read_dir(home.join("runs").join(&state.id))? {
        let name = entry?.file_name();
        ensure!(
            ["run.json", "pr-owner.lock", "driver.lock"]
                .iter()
                .any(|n| name == *n),
            "unexpected startup artifact requires inspection: {}",
            name.to_string_lossy()
        );
    }
    for id in crate::run::list_ids_in(&home.join("runs")) {
        if id == state.id {
            continue;
        }
        let other = RunState::load_under(&id, home)
            .context("cannot exclude handover from unreadable run")?;
        ensure!(
            other.released_to.as_deref() != Some(state.id.as_str())
                && other.pr_taken_over_by.as_deref() != Some(state.id.as_str())
                && other.retired_to.as_deref() != Some(state.id.as_str()),
            "another run handed assets to this reservation"
        );
    }
    ensure!(
        !task.runs.contains(&state.id),
        "run is in execution history"
    );
    let repeated = state.startup.as_ref().is_some_and(|s| {
        state.status == RunStatus::Failed
            && !s.complete
            && s.kind == "reconciled-review"
            && s.failure.as_deref() == Some(&evidence.refusal_log)
            && s.branch.as_deref() == Some(&evidence.branch)
    });
    ensure!(
        repeated || state.events.is_empty(),
        "unexpected startup events require inspection"
    );
    ensure!(
        repeated
            || (state.status == RunStatus::Prep
                && state.startup.is_none()
                && state.instruction.trim().is_empty()),
        "only an empty legacy reservation can be reconciled"
    );
    let stage = "handover-refused".to_owned();
    if apply {
        if !repeated {
            state.instruction = format!("Review the work already on branch `{}`.", evidence.branch);
            state.startup = Some(Startup {
                kind: "reconciled-review".to_owned(),
                branch: Some(evidence.branch.clone()),
                stage: stage.clone(),
                complete: false,
                failure: Some(evidence.refusal_log.clone()),
            });
            state.status = RunStatus::Failed;
            state.driver_exited = true;
            state.event("startup-reconciled", "operator supplied refusal evidence; durable never-claimed marker and ownership checks confirm no execution; task controls unchanged");
            state.save_under(home)?;
        }
        task.record_startup_failure(StartupFailure {
            run: state.id.clone(),
            stage,
        });
        // Avoid notice/status side effects of Queue::put: this is audit metadata only.
        queue.write_startup_history(&task)?;
    }
    Ok(format!(
        "{}: {} never-executed review reservation for task {}; task status, attempts, execution history and approvals unchanged",
        state.id,
        if apply {
            "reconciled"
        } else {
            "would reconcile"
        },
        task.id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Evidence, RunState) {
        let tmp = tempfile::tempdir().unwrap();
        let queue = crate::queue::Queue::at(tmp.path().join("queue"));
        let mut task = crate::queue::Task::new(
            "review".into(),
            "criteria".into(),
            tmp.path().join("repo"),
            crate::queue::Source::Human,
        );
        task.hold_manual(Some("keep this hold and its approvals".into()));
        queue.put(&mut task).unwrap();
        let mut config = crate::config::Config::default();
        config.graph.worktree_root = Some(tmp.path().join("worktrees"));
        let mut state = RunState::new(
            task.repo.clone(),
            "main".into(),
            "abc".into(),
            String::new(),
            config,
        );
        state.origin = Some(crate::run::Origin::queue(&task.id));
        state.reserve_under(tmp.path()).unwrap();
        let evidence = Evidence {
            run: state.id.clone(),
            task: task.id,
            branch: "feature".into(),
            refusal_log: "review-only takeover refused: previous run live".into(),
        };
        (tmp, evidence, state)
    }

    #[test]
    fn reconciliation_is_explicit_idempotent_and_preserves_task_controls() {
        let (tmp, evidence, state) = fixture();
        let queue = crate::queue::Queue::at(tmp.path().join("queue"));
        let before = serde_json::to_value(queue.get(&evidence.task).unwrap()).unwrap();
        assert!(
            reconcile(tmp.path(), &evidence, false)
                .unwrap()
                .contains("would reconcile")
        );
        assert_eq!(
            RunState::load_under(&state.id, tmp.path()).unwrap().status,
            RunStatus::Prep
        );
        reconcile(tmp.path(), &evidence, true).unwrap();
        let first =
            std::fs::read(tmp.path().join("runs").join(&state.id).join("run.json")).unwrap();
        reconcile(tmp.path(), &evidence, true).unwrap();
        assert_eq!(
            first,
            std::fs::read(tmp.path().join("runs").join(&state.id).join("run.json")).unwrap()
        );
        let task = queue.get(&evidence.task).unwrap();
        assert_eq!(task.startup_failures.len(), 1);
        assert!(task.runs.is_empty());
        let mut after = serde_json::to_value(task).unwrap();
        after["startup_failures"] = before["startup_failures"].clone();
        assert_eq!(before, after, "only audit metadata changes");
        let failed = RunState::load_under(&state.id, tmp.path()).unwrap();
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.startup.unwrap().failure, Some(evidence.refusal_log));
    }

    #[test]
    fn reconciliation_refuses_live_unknown_executed_and_handover_records() {
        let (tmp, evidence, mut state) = fixture();
        let lease = crate::driver::maintenance(tmp.path(), &state.id).unwrap();
        assert!(reconcile(tmp.path(), &evidence, true).is_err());
        drop(lease);
        state.driver_claimed = None;
        state.save_under(tmp.path()).unwrap();
        assert!(
            reconcile(tmp.path(), &evidence, true).is_err(),
            "unknown ownership stays untouched"
        );
        state.driver_claimed = Some(false);
        state.save_under(tmp.path()).unwrap();
        let artifact = tmp.path().join("runs").join(&state.id).join("seat.log");
        std::fs::write(&artifact, "execution evidence").unwrap();
        assert!(reconcile(tmp.path(), &evidence, true).is_err());
        std::fs::remove_file(artifact).unwrap();
        let mut other = state.clone();
        other.id = "20261025-080132-dead".into();
        other.released_to = Some(state.id.clone());
        other.save_under(tmp.path()).unwrap();
        assert!(
            reconcile(tmp.path(), &evidence, true)
                .unwrap_err()
                .to_string()
                .contains("handed assets")
        );
        assert_eq!(
            RunState::load_under(&state.id, tmp.path()).unwrap().status,
            RunStatus::Prep
        );
    }
}
