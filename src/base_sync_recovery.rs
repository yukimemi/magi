//! Durable, single-use authorization for an exhausted base synchronization.
use anyhow::{Result, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::run::{CommandOutcome, ReviewRound, RunState, RunStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Why synchronization stopped. Only exhaustion admits an extra attempt.
pub enum StopKind {
    /// The ordinary moving-base budget was spent.
    Exhausted,
    /// Replaying the retained changes failed.
    Conflict,
    /// A remote ownership or publication operation failed.
    Publication,
    /// The change was integrated, but closing its PR failed and may be retried.
    ClosingIntegratedPr,
    /// Legacy or unexpected evidence cannot justify recovery.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Stable identity of a stopped synchronization.
pub struct Stop {
    /// Typed cause.
    pub kind: StopKind,
    /// Retained candidate commit.
    pub head: String,
    /// Base tip observed at the stop.
    pub tip: String,
    /// Cumulative attempts when stopped.
    pub attempts: usize,
    /// Original diagnostic, retained verbatim.
    pub detail: String,
    /// When this record was created.
    pub at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// One explicit owner request, consumed durably before rebasing.
pub struct Authorization {
    /// Task whose claim admitted the request.
    pub task: String,
    /// Exact saved execution authorized to recover.
    pub run: String,
    /// Immutable stop authorized by this request.
    pub stop: Stop,
    /// When this record was created.
    pub at: Timestamp,
    /// Persisted before any Git mutation. An interrupted consumption grants no retry.
    pub consumed: bool,
    /// Queue approval generation, absent for an explicit recovery command.
    #[serde(default)]
    pub generation: Option<u64>,
    /// Historical evidence stays attached to its original heads.
    pub reviews: Vec<ReviewRound>,
    /// Prior gate outcomes, never evidence for the recovered head.
    pub gate: Vec<CommandOutcome>,
    /// Head targeted by the archived gate.
    pub gate_head: Option<String>,
    /// Whether the archived gate was attempted.
    pub gate_ran: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
/// Persistent stop and authorization history, additive for legacy runs.
pub struct Recovery {
    /// Current stop, if any.
    pub stop: Option<Stop>,
    /// Requests and archived evidence in authorization order.
    pub authorizations: Vec<Authorization>,
}

impl RunState {
    pub(crate) fn base_sync_blocked(&self) -> bool {
        self.base_sync
            .as_ref()
            .is_some_and(|s| s.conflict.is_some())
    }

    /// This checkpoint only rechecks integration and retries PR closure. It
    /// never clears the conflict guard or grants an additional rebase.
    pub(crate) fn base_sync_closure_pending(&self) -> bool {
        let Some(detail) = self
            .base_sync
            .as_ref()
            .and_then(|sync| sync.conflict.as_ref())
        else {
            return false;
        };
        let Some(winner) = self.winner() else {
            return false;
        };
        // Also recognizes the diagnostic written before typed stops existed.
        let prefix = format!(
            "{} is already in {}/{}, but its pull request could not be closed (",
            winner.branch, self.config.merge.remote, self.base_branch
        );
        detail.starts_with(&prefix) && detail.ends_with("); resume to retry")
    }

    /// Legacy exhaustion is recognized only by the exact message and budget.
    /// Once recorded, its identity remains stable across no-progress passes.
    pub(crate) fn record_base_sync_stop(&mut self, head: &str) {
        let Some(sync) = self.base_sync.as_ref() else {
            return;
        };
        let Some(detail) = sync.conflict.clone() else {
            return;
        };
        if self
            .base_sync_recovery
            .stop
            .as_ref()
            .is_some_and(|s| s.head == head && s.tip == sync.tip && s.detail == detail)
        {
            self.status = RunStatus::Blocked;
            return;
        }
        let expected = self.winner().map(|w| format!(
            "{} moved {} commit(s) ahead of {} after {} rebase(s); rebasing again would only race it",
            self.base_branch, sync.behind, w.branch, crate::graph::BASE_SYNC_ROUNDS
        ));
        let kind = if self.base_sync_closure_pending() {
            StopKind::ClosingIntegratedPr
        } else if sync.attempts == crate::graph::BASE_SYNC_ROUNDS
            && expected.as_deref() == Some(detail.as_str())
        {
            StopKind::Exhausted
        } else if detail.contains("refused the push")
            || detail.contains("could not push")
            || detail.contains("could not be pushed")
            || detail.contains("not rebasing over")
        {
            StopKind::Publication
        } else if detail.contains("conflicts with") && detail.contains("did not rebase") {
            StopKind::Conflict
        } else {
            StopKind::Unknown
        };
        let at = self
            .events
            .iter()
            .rev()
            .find(|event| event.node == "land" && event.message == detail)
            .map_or(self.updated_at, |event| event.at);
        self.base_sync_recovery.stop = Some(Stop {
            kind,
            head: head.to_owned(),
            tip: sync.tip.clone(),
            attempts: sync.attempts,
            detail,
            at,
        });
        self.status = RunStatus::Blocked;
    }

    pub(crate) fn base_recovery_pending(&self) -> bool {
        self.base_sync_recovery
            .authorizations
            .last()
            .is_some_and(|a| {
                !a.consumed
                    && a.run == self.id
                    && self.base_sync_recovery.stop.as_ref() == Some(&a.stop)
            })
    }

    pub(crate) fn authorize_base_sync(&mut self, task: &str) -> Result<()> {
        let stop = self
            .base_sync_recovery
            .stop
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no recorded base-sync stop"))?;
        ensure!(
            stop.kind == StopKind::Exhausted,
            "only exhausted base movement permits recovery"
        );
        ensure!(
            !self
                .base_sync_recovery
                .authorizations
                .last()
                .is_some_and(|a| !a.consumed && a.stop == stop),
            "this stop already has a pending authorization; resume the existing request"
        );
        self.base_sync_recovery.authorizations.push(Authorization {
            task: task.to_owned(),
            run: self.id.clone(),
            stop,
            at: Timestamp::now(),
            consumed: false,
            generation: None,
            reviews: Vec::new(),
            gate: Vec::new(),
            gate_head: None,
            gate_ran: false,
        });
        Ok(())
    }

    pub(crate) fn authorize_base_sync_resumption(
        &mut self,
        task: &str,
        approval: &crate::queue::Resumption,
    ) -> Result<()> {
        let stop = self
            .base_sync_recovery
            .stop
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no recorded base-sync stop"))?;
        ensure!(
            approval.at > stop.at,
            "approval predates the base-sync stop"
        );
        ensure!(
            !self.base_sync_recovery.authorizations.iter().any(|a| {
                a.task == task && a.generation.is_some_and(|g| g >= approval.generation)
            }),
            "operator resumption generation already used"
        );
        self.authorize_base_sync(task)?;
        self.base_sync_recovery
            .authorizations
            .last_mut()
            .unwrap()
            .generation = Some(approval.generation);
        Ok(())
    }

    /// Reserve the one additional attempt and detach old verification atomically
    /// with consumption. The caller must save before running the rebase.
    pub(crate) fn consume_base_recovery(&mut self, head: &str) -> bool {
        if !self.base_recovery_pending() {
            return false;
        }
        let a = self.base_sync_recovery.authorizations.last_mut().unwrap();
        a.consumed = true;
        if a.stop.head != head {
            return false;
        }
        a.reviews = std::mem::take(&mut self.reviews);
        a.gate = std::mem::take(&mut self.gate);
        a.gate_head = self.gate_head.take();
        a.gate_ran = std::mem::replace(&mut self.gate_ran, false);
        self.base_sync.as_mut().unwrap().attempts += 1;
        true
    }
}

/// A generic task release cannot authorize a stopped base synchronization.
/// Use the queue's own home, including isolated triage stores.
pub(crate) fn held_task(queue: &crate::queue::Queue, task: &crate::queue::Task) -> bool {
    held_run(queue, task).is_some()
}

pub(crate) fn held_run(queue: &crate::queue::Queue, task: &crate::queue::Task) -> Option<RunState> {
    let home = queue.root().parent()?;
    task.runs
        .last()
        .and_then(|id| RunState::load_under(id, home).ok())
        .filter(|state| {
            state.base_sync_blocked()
                && !state.base_recovery_pending()
                && !state.base_sync_closure_pending()
        })
}

impl crate::queue::Task {
    /// Keep a producer stop stable across generic releases and driver passes.
    pub(crate) fn base_sync_notice(&mut self, state: &RunState) {
        let Some(sync) = state
            .base_sync
            .as_ref()
            .filter(|_| state.base_sync_blocked())
        else {
            return;
        };
        let stop = state.base_sync_recovery.stop.as_ref();
        self.held_at = Some(stop.map_or(state.updated_at, |s| s.at));
        self.notice_cause("base-sync-stop", Some(&state.id), None);
        if let Some(cause) = &mut self.hold_notice_cause {
            cause.target = Some(match stop {
                Some(s) => format!("{}:{}:{}", s.head, s.tip, s.at),
                None => format!("legacy:{}:{}", sync.tip, sync.attempts),
            });
        }
    }
}
