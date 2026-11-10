//! Durable bounded synchronization intervals and single-use owner recovery approvals.
use anyhow::{Result, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::run::{CommandOutcome, ReviewRound, RunState, RunStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Why synchronization stopped. Only exhaustion admits a new recovery interval.
pub enum StopKind {
    /// The ordinary moving-base budget was spent.
    Exhausted,
    /// Replaying the retained changes failed.
    Conflict,
    /// A remote ownership or publication operation failed.
    Publication,
    /// The remote branch contains foreign work.
    Divergence,
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
    /// A synchronized HEAD had not reached its gate before the base advanced.
    #[serde(default)]
    pub unverified_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// One explicit owner request, consumed durably before rebasing.
pub struct Authorization {
    #[serde(default)]
    pub scope: Option<Scope>,
    /// Older approvals authorized one attempt only.
    #[serde(default)]
    pub interval: bool,
    #[serde(default)]
    pub invalidated: Option<String>,
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

impl Authorization {
    /// Consume a refused approval without granting recovery, retaining the reason.
    pub(crate) fn refuse(&mut self, reason: impl Into<String>) {
        self.invalidated = Some(reason.into());
        self.consumed = true;
    }
}

/// A bounded active interval. Lifetime attempts remain in `BaseSync`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    /// Monotonic interval identity.
    pub id: usize,
    /// Why this interval was opened.
    pub reason: String,
    /// Reservations, including interrupted operations.
    pub used: usize,
    /// Last successful synchronization or actually executed verification in this interval.
    pub synced_head: Option<String>,
    /// Last successful synchronization, distinct from verification progress.
    #[serde(default)]
    pub last_sync_head: Option<String>,
    /// An interruption after progress; consumed at most once.
    pub boundary: Option<Boundary>,
    /// Earlier interruption evidence superseded by actual progress.
    #[serde(default)]
    pub prior_boundaries: Vec<Boundary>,
}

/// Durable interruption evidence, never inferred from elapsed time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boundary {
    pub pass: usize,
    pub at: Timestamp,
    pub head: String,
    pub consumed: bool,
}

/// Write-ahead reservation. A restart continues this exact operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: usize,
    pub episode: usize,
    pub head: String,
    pub tip: String,
    pub finished: bool,
    /// Exact result accepted by the rebase validator, saved before scratch cleanup.
    #[serde(default)]
    pub validated_head: Option<String>,
}

/// Consumed owner approval for an infrastructure boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Renewal {
    pub task: String,
    pub run: String,
    pub generation: u64,
    pub instruction: String,
    pub episode: usize,
    pub boundary_pass: usize,
    pub head: String,
    pub tip: String,
}

/// Approval scope additional to the immutable stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub tip: String,
    pub instruction: String,
    pub episode: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
/// Persistent stop and authorization history, additive for legacy runs.
pub struct Recovery {
    /// Append-only interval and operation evidence, absent in old records.
    #[serde(default)]
    pub episodes: Vec<Episode>,
    #[serde(default)]
    pub operations: Vec<Operation>,
    /// Stops remain evidence after successful recovery.
    #[serde(default)]
    pub stops: Vec<Stop>,
    #[serde(default)]
    pub renewals: Vec<Renewal>,
    /// Current stop, if any.
    pub stop: Option<Stop>,
    /// Requests and archived evidence in authorization order.
    pub authorizations: Vec<Authorization>,
}

impl RunState {
    /// Shared CLI, notice and web instruction, based on the typed checkpoint.
    pub fn base_sync_next_action(&self) -> String {
        let episode = self.base_sync_recovery.episodes.last().map_or(0, |e| e.id);
        let lifetime = self.base_sync.as_ref().map_or(0, |s| s.attempts);
        format!(
            "{} ({}/{} in episode {}, {} lifetime synchronizations)",
            self.base_sync_action(),
            self.base_episode_used(),
            crate::graph::BASE_SYNC_ROUNDS,
            episode,
            lifetime
        )
    }

    fn base_sync_action(&self) -> String {
        if let Some(reason) = self
            .base_sync_recovery
            .authorizations
            .last()
            .and_then(|a| a.invalidated.as_ref())
        {
            return format!("Approval invalidated: {reason}");
        }
        if self.resource_block.is_some() {
            return "Restore the required resource, then explicitly resume the retained run; automatic retries retain the synchronization budget".into();
        }
        if self.base_sync_recovery.stop.is_none()
            && let Some(op) = self
                .base_sync_recovery
                .operations
                .last()
                .filter(|o| !o.finished)
        {
            return format!(
                "Resume reserved synchronization {} against base {}; inspect retained Git state if reconciliation is refused",
                op.id, op.tip
            );
        }
        match self.base_sync_recovery.stop.as_ref().map(|s| &s.kind) {
            Some(StopKind::Exhausted) => {
                let why = if self.base_sync_recovery.stop.as_ref().is_some_and(|s| s.unverified_sync) {
                    "The synchronized HEAD never reached its gate before the base moved again. "
                } else {
                    ""
                };
                if self.base_recovery_pending() {
                    format!("{why}Approval recorded; resume the retained run to open the approved bounded synchronization interval")
                } else {
                    format!("{why}Authorize a new bounded synchronization interval with `magi run --resume {} --recover-base`, or a new owner resumption approval", self.id)
                }
            },
            Some(StopKind::Conflict) => {
                let stop = self.base_sync_recovery.stop.as_ref().unwrap();
                let task = self.origin.as_ref().and_then(|origin| origin.task.as_deref()).unwrap_or("<task>");
                format!("Authorize a narrow history-preserving repair with `magi task grant-repair {task} --run {} --kind base-conflict --rounds 1 --head {} --tip {} --expect-pass {} --request <unique-id> --reason <reason> --instruction <approved-resolution>`; retain the original review criteria. Base-budget recovery cannot resolve a textual conflict", self.id, stop.head, stop.tip, self.passes.last().map_or(0, |p| p.index))
            },
            Some(StopKind::Divergence) => "Reconcile foreign remote branch commits before resuming; do not overwrite them".into(),
            Some(StopKind::Publication) => "Inspect the retained publication failure and remote lease before resuming".into(),
            Some(StopKind::ClosingIntegratedPr) => "Resume to retry closing the already integrated pull request".into(),
            Some(StopKind::Unknown) => "Inspect the retained synchronization state before resuming; unknown evidence grants no new budget".into(),
            None => "Continue verification on the current synchronized HEAD".into(),
        }
    }

    pub(crate) fn ensure_base_episode(&mut self) {
        if let Some(stop) = self.base_sync_recovery.stop.clone()
            && !self.base_sync_recovery.stops.contains(&stop)
        {
            self.base_sync_recovery.stops.push(stop);
        }
        if self.base_sync_recovery.episodes.is_empty() {
            let used = self.base_sync.as_ref().map_or(0, |s| s.attempts);
            self.base_sync_recovery.episodes.push(Episode {
                id: 0,
                reason: "initial or conservative legacy migration".into(),
                used,
                synced_head: None,
                last_sync_head: None,
                boundary: None,
                prior_boundaries: Vec::new(),
            });
        }
    }

    fn open_base_episode(&mut self, reason: &str) {
        self.ensure_base_episode();
        let id = self.base_sync_recovery.episodes.len();
        self.base_sync_recovery.episodes.push(Episode {
            id,
            reason: reason.into(),
            used: 0,
            synced_head: None,
            last_sync_head: None,
            boundary: None,
            prior_boundaries: Vec::new(),
        });
    }

    pub(crate) fn base_episode_used(&self) -> usize {
        self.base_sync_recovery.episodes.last().map_or_else(
            || self.base_sync.as_ref().map_or(0, |s| s.attempts),
            |e| e.used,
        )
    }

    pub(crate) fn record_base_progress(&mut self, head: &str) {
        self.ensure_base_episode();
        let episode = self.base_sync_recovery.episodes.last_mut().unwrap();
        if let Some(boundary) = episode.boundary.take() {
            episode.prior_boundaries.push(boundary);
        }
        episode.synced_head = Some(head.into());
    }

    /// Called by the same atomic state save that retains the resource stop.
    pub(crate) fn record_base_interruption(&mut self) {
        let Some(block) = &self.resource_block else {
            return;
        };
        if matches!(block.cause, crate::run::ResourceBlockCause::Unrecorded) {
            return;
        }
        let Some(episode) = self.base_sync_recovery.episodes.last_mut() else {
            return;
        };
        let Some(head) = episode.synced_head.clone() else {
            return;
        };
        if episode.boundary.is_none() {
            episode.boundary = Some(Boundary {
                pass: self.passes.last().map_or(0, |p| p.index),
                at: block.at,
                head,
                consumed: false,
            });
        }
    }

    pub(crate) fn renew_base_episode(
        &mut self,
        task: &str,
        approval: &crate::queue::Resumption,
        head: &str,
        tip: &str,
    ) -> bool {
        if self
            .base_sync_recovery
            .renewals
            .iter()
            .any(|r| r.task == task && r.generation >= approval.generation)
            || self
                .base_sync_recovery
                .authorizations
                .iter()
                .any(|a| a.task == task && a.generation.is_some_and(|g| g >= approval.generation))
        {
            return false;
        }
        let Some(e) = self.base_sync_recovery.episodes.last_mut() else {
            return false;
        };
        let Some(b) = &mut e.boundary else {
            return false;
        };
        if b.consumed || b.head != head || approval.at <= b.at {
            return false;
        }
        b.consumed = true;
        self.base_sync_recovery.renewals.push(Renewal {
            task: task.into(),
            run: self.id.clone(),
            generation: approval.generation,
            instruction: approval.instruction.clone(),
            episode: e.id,
            boundary_pass: b.pass,
            head: head.into(),
            tip: tip.into(),
        });
        self.open_base_episode("owner resumption after recorded infrastructure interruption");
        true
    }

    /// Reserve once before Git; retrying an unfinished operation spends nothing more.
    pub(crate) fn reserve_base_operation(&mut self, head: &str, tip: &str) {
        self.ensure_base_episode();
        if self
            .base_sync_recovery
            .operations
            .last()
            .is_some_and(|o| !o.finished)
        {
            return;
        }
        let e = self.base_sync_recovery.episodes.last_mut().unwrap();
        e.used += 1;
        let id = self.base_sync_recovery.operations.len();
        self.base_sync_recovery.operations.push(Operation {
            id,
            episode: e.id,
            head: head.into(),
            tip: tip.into(),
            finished: false,
            validated_head: None,
        });
        if let Some(sync) = &mut self.base_sync {
            sync.attempts += 1;
        }
    }

    pub(crate) fn finish_base_operation(&mut self, head: Option<&str>) {
        if let Some(op) = self.base_sync_recovery.operations.last_mut() {
            op.finished = true;
        }
        if let Some(head) = head {
            let episode = self.base_sync_recovery.episodes.last_mut().unwrap();
            if let Some(boundary) = episode.boundary.take() {
                episode.prior_boundaries.push(boundary);
            }
            episode.synced_head = Some(head.into());
            episode.last_sync_head = Some(head.into());
            self.reconcile_verification_head(head);
        }
    }

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
        let exhausted = self.winner().is_some_and(|w| {
            [
                ("rebase(s)", "rebasing"),
                ("synchronization(s)", "synchronizing"),
            ].iter().any(|(noun, verb)| detail == format!(
                "{} moved {} commit(s) ahead of {} after {} {}; {} again would only race it",
                self.base_branch, sync.behind, w.branch, crate::graph::BASE_SYNC_ROUNDS, noun, verb
            ))
        });
        let kind = if self.base_sync_closure_pending() {
            StopKind::ClosingIntegratedPr
        } else if sync.attempts >= crate::graph::BASE_SYNC_ROUNDS && exhausted {
            StopKind::Exhausted
        } else if detail.contains("not rebasing over") {
            StopKind::Divergence
        } else if detail.contains("refused the push")
            || detail.contains("could not push")
            || detail.contains("could not be pushed")
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
        let unverified_sync = sync.behind > 0
            && self
                .base_sync_recovery
                .episodes
                .last()
                .is_some_and(|e| e.last_sync_head.as_deref() == Some(head))
            && !(self.gate_ran && self.gate_head.as_deref() == Some(head));
        let stop = Stop {
            unverified_sync,
            kind,
            head: head.to_owned(),
            tip: sync.tip.clone(),
            attempts: sync.attempts,
            detail,
            at,
        };
        self.base_sync_recovery.stops.push(stop.clone());
        self.base_sync_recovery.stop = Some(stop);
        self.status = RunStatus::Blocked;
    }

    pub(crate) fn base_recovery_pending(&self) -> bool {
        self.base_sync_recovery
            .authorizations
            .last()
            .is_some_and(|a| {
                !a.consumed
                    && a.invalidated.is_none()
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
            scope: None,
            interval: true,
            invalidated: None,
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
            }) && !self
                .base_sync_recovery
                .renewals
                .iter()
                .any(|r| r.task == task && r.generation >= approval.generation),
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

    /// Open the approved bounded interval and detach old gate evidence atomically
    /// with consumption. The caller must save before reserving Git work.
    pub(crate) fn consume_base_recovery(&mut self, head: &str) -> bool {
        if !self.base_recovery_pending() {
            return false;
        }
        let a = self.base_sync_recovery.authorizations.last_mut().unwrap();
        a.consumed = true;
        if a.stop.head != head {
            a.refuse(format!(
                "retained HEAD changed from {} to {head}; inspect it and authorize again with --recover-base",
                a.stop.head
            ));
            return false;
        }
        a.reviews = self.reviews.clone();
        a.gate = std::mem::take(&mut self.gate);
        a.gate_head = self.gate_head.take();
        a.gate_ran = std::mem::replace(&mut self.gate_ran, false);
        let interval = a.interval;
        self.base_sync.as_mut().unwrap().conflict = None;
        self.open_base_episode("explicit exhausted-base recovery");
        self.base_sync_recovery.stop = None;
        if !interval {
            self.base_sync_recovery.episodes.last_mut().unwrap().used =
                crate::graph::BASE_SYNC_ROUNDS - 1;
        }
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
    task.execution_runs()
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

/// Refuse a generic release at a textual stop before changing queue state.
pub fn ensure_generic_release(
    queue: &crate::queue::Queue,
    task: &crate::queue::Task,
) -> Result<()> {
    if let Some(state) = task
        .execution_runs()
        .last()
        .and_then(|id| RunState::load_under(id, queue.root().parent()?).ok())
        && let Some(reason) = crate::repair_budget::conflict_failure(task, &state)
    {
        anyhow::bail!(reason);
    }
    if let Some(state) = held_run(queue, task)
        && state
            .base_sync_recovery
            .stop
            .as_ref()
            .is_some_and(|s| s.kind == StopKind::Conflict)
        && !crate::repair_budget::conflict_pending(task, &state)
    {
        anyhow::bail!(
            "generic release cannot authorize textual conflict editing. {}",
            state.base_sync_next_action()
        );
    }
    Ok(())
}
