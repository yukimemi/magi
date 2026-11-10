//! Recoverable owner authorization and exact-run queueing for standing controls.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    queue::{Queue, Task, TaskStatus},
    run::RunState,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub run: String,
    pub head: String,
    pub actor: String,
    pub request: String,
    pub decision: Option<(String, jiff::Timestamp)>,
    pub expected_stop: Option<(crate::base_sync_recovery::Stop, usize)>,
}

pub(crate) struct Outcome {
    pub task: Task,
    pub repeated: bool,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    request: Request,
    binding: serde_json::Value,
    queued: Task,
    applied: bool,
}

pub(crate) struct Transaction {
    pub request: Request,
    path: PathBuf,
    receipt: Option<Receipt>,
}

fn save(path: &Path, receipt: &Receipt) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(receipt)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Hold the task claim across authorization and queue publication. The common
/// maintenance transition holds the owner and execution leases across both saves.
pub(crate) async fn recover(queue: &Queue, home: &Path, request: Request) -> Result<Outcome> {
    ensure!(
        crate::run::is_run_id(&request.run),
        "recovery needs a full run id"
    );
    ensure!(
        crate::ask::valid_asset_name(&request.actor)
            && crate::ask::valid_asset_name(&request.request),
        "recovery actor and request must be safe filenames of at most 64 bytes"
    );
    ensure!(
        (7..=40).contains(&request.head.len())
            && request.head.bytes().all(|c| c.is_ascii_hexdigit()),
        "recovery needs a retained HEAD SHA or prefix of 7 to 40 hex digits"
    );
    let dir = home.join("base-recovery-controls").join(&request.actor);
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{}.lock", request.request)))?;
    crate::pr_head::try_lock_file(&lock)?;
    let path = dir.join(format!("{}.json", request.request));
    let receipt: Option<Receipt> = if path.exists() {
        Some(serde_json::from_slice(&std::fs::read(&path)?)?)
    } else {
        None
    };
    if let Some(prior) = &receipt {
        ensure!(
            prior.request == request,
            "request ID already binds a different operation"
        );
        if prior.applied {
            return Ok(Outcome {
                task: prior.queued.clone(),
                repeated: true,
            });
        }
        let task = queue.get(&prior.queued.id)?;
        // The marker is saved atomically with the queue pin. Completion can be
        // acknowledged after a crash even if the daemon has already consumed it.
        if task
            .base_recovery_requests
            .contains(&format!("{}/{}", request.actor, request.request))
        {
            let mut prior = receipt.unwrap();
            prior.applied = true;
            save(&path, &prior)?;
            return Ok(Outcome {
                task: prior.queued,
                repeated: true,
            });
        }
    }
    crate::task_reassign::ensure_current(queue, &request.run)?;
    let mut owners = queue
        .list()
        .into_iter()
        .filter(|t| t.execution_runs().last() == Some(&request.run));
    let task = owners
        .next()
        .context("run has no eligible latest task owner")?;
    ensure!(owners.next().is_none(), "run has ambiguous task ownership");
    let _claim = queue
        .claim(&task.id)
        .context("task is claimed by a running daemon or another control")?;
    let mut transaction = Transaction {
        request,
        path,
        receipt,
    };
    crate::graph::Runner::authorize_base_recovery_control(home, &task.id, &mut transaction).await?;
    Ok(Outcome {
        task: queue.get(&task.id)?,
        repeated: false,
    })
}

impl Transaction {
    pub fn key(&self) -> String {
        format!("{}/{}", self.request.actor, self.request.request)
    }

    /// Called only inside the shared maintenance locks after fetching the base.
    pub fn prepare(
        &mut self,
        queue: &Queue,
        state: &RunState,
        task: &Task,
        head: &str,
        tip: &str,
    ) -> Result<()> {
        ensure!(
            head.starts_with(&self.request.head),
            "retained HEAD changed since the owner request"
        );
        ensure!(
            crate::daemon::compatible_run_instruction(state, task),
            "task instruction does not match the retained run"
        );
        ensure!(
            !matches!(task.status, TaskStatus::Running | TaskStatus::Done),
            "run queue owner changed"
        );
        if let Some((owner, at)) = &self.request.decision {
            ensure!(
                decision_current(queue, state, owner, &self.request.request),
                "recovery decision ownership, routing or stop evidence changed"
            );
            ensure!(
                owner == &task.id
                    && task
                        .operator_decided_at
                        .is_none_or(|decision| decision <= *at),
                "recovery decision has stale task ownership or was superseded"
            );
        }
        if let Some((stop, episode)) = &self.request.expected_stop {
            ensure!(
                state.base_sync_recovery.stop.as_ref() == Some(stop)
                    && state
                        .base_sync_recovery
                        .episodes
                        .last()
                        .is_some_and(|e| e.id == *episode),
                "recovery decision names a stale stop or interval"
            );
        }
        let binding = serde_json::json!({"task": task, "stop": state.base_sync_recovery.stop, "tip": tip, "episode": state.base_sync_recovery.episodes.last().map(|e| e.id)});
        if let Some(receipt) = &self.receipt {
            ensure!(
                receipt.binding == binding,
                "recovery request is stale: task, instruction, owner decision, stop or fetched base changed"
            );
            return Ok(());
        }
        let mut queued = task.clone();
        if !crate::repair_budget::queue_resumption(queue, &mut queued, &state.id)? {
            queued.release_operator(
                "base-recovery",
                self.request
                    .decision
                    .as_ref()
                    .map(|_| self.request.request.as_str()),
            );
        }
        queued.resume_override = Some(crate::queue::OperatorResume {
            question_id: if self.request.decision.is_some() {
                self.request.request.clone()
            } else {
                String::new()
            },
            at: jiff::Timestamp::now(),
            conductor_rehold: None,
            forced: true,
            pinned_run: Some(state.id.clone()),
        });
        queued.base_recovery_requests.push(self.key());
        if self.request.decision.is_some() {
            queued.mark_action_applied(&self.request.request);
        }
        let receipt = Receipt {
            request: self.request.clone(),
            binding,
            queued,
            applied: false,
        };
        save(&self.path, &receipt)?;
        self.receipt = Some(receipt);
        Ok(())
    }

    pub fn generation(&self) -> Option<u64> {
        self.receipt
            .as_ref()
            .unwrap()
            .queued
            .resumption
            .as_ref()
            .map(|r| r.generation)
    }
    pub fn decision(&self) -> Option<jiff::Timestamp> {
        self.receipt.as_ref().unwrap().queued.operator_decided_at
    }

    pub fn enqueue(&mut self, queue: &Queue) -> Result<()> {
        let receipt = self.receipt.as_mut().unwrap();
        queue.commit_base_recovery(&mut receipt.queued, &receipt.binding["task"])?;
        receipt.applied = true;
        save(&self.path, receipt)
    }
}

/// Recheck the recorded choice both at admission and at live-driver consumption.
pub(crate) fn decision_current(
    queue: &Queue,
    state: &RunState,
    owner: &str,
    question: &str,
) -> bool {
    let Some(home) = queue.root().parent() else {
        return false;
    };
    let Ok(q) = crate::ask::Questions::at(home.join("questions")).get(question) else {
        return false;
    };
    let tasks = queue.list();
    crate::task_chat::task_of(&q, &tasks).as_deref() == Some(owner)
        && q.routed_task
            .as_deref()
            .is_none_or(|routed| routed == owner)
        && tasks.iter().find(|t| t.id == owner).is_some_and(|t| {
            !t.stale_question(&q) && (q.run == t.id || t.execution_runs().last() == Some(&q.run))
        })
        && matches!(q.chosen_action(), Some(crate::ask::ChoiceAction::RecoverBase { run, stop, episode })
            if run == &state.id && state.base_sync_recovery.stop.as_ref() == Some(stop)
                && state.base_sync_recovery.episodes.last().is_some_and(|e| e.id == *episode))
}
