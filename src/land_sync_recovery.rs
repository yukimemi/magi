//! Explicit, bounded same-run recovery for exhausted landing base synchronization.
//!
//! Landing base synchronization (`land.rs::rebase_landing_worktree`) uses its own
//! `land_sync_attempts` counter against `config.graph.land_rounds`, separate from
//! pre-publication `base_sync_recovery`. Recovery grants at most one extra landing
//! sync attempt per explicit owner approval, capped over the run's lifetime, without
//! resetting `land_sync_attempts` or bypassing fresh head-bound verification or merge
//! approval.
use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    git,
    queue::{Queue, Task, TaskStatus},
    run::{
        CommandOutcome, GateRepairE2e, MergeRepairPhase, RunState, RunStatus, VerificationEvidence,
    },
};

/// Single-attempt allowance granted per explicit owner recovery approval.
pub const GRANT_ATTEMPTS: usize = 1;

/// Maximum lifetime extra landing base-sync attempts granted to one run.
pub const MAX_GRANTS: usize = 4;

/// Canonical diagnostic written when landing base synchronization exhausts its budget.
pub const EXHAUSTED_DETAIL: &str = "landing base synchronization budget exhausted";

/// Immutable snapshot of a landing base-sync exhaustion stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    /// Cumulative `land_sync_attempts` when stopped.
    pub attempts: usize,
    /// Effective landing base-sync limit (`land_rounds + extra_attempts`) when stopped.
    pub limit: usize,
    /// Retained winner worktree and branch HEAD at the stop.
    pub head: String,
    /// Published PR HEAD observed and recorded at the stop.
    pub pr_head: String,
    /// Existing pull request URL.
    pub pr_url: String,
    /// Existing pull request number.
    pub pr_number: u64,
    /// Fetched base tip observed at the stop (or verified at legacy admission).
    pub tip: String,
    /// Canonical diagnostic detail.
    pub detail: String,
    /// Stable timestamp of the stop.
    pub at: Timestamp,
}

/// Admission bindings revalidated at dispatch and execution time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Fetched base tip observed at owner authorization; ordinary fast-forward is allowed.
    pub tip: String,
    /// Owning task instruction at authorization time.
    pub instruction: String,
    /// Owning task assignment count (`reassignments.len()`) at authorization time.
    pub assignment: usize,
    /// Open PR HEAD observed at authorization time.
    pub pr_head: String,
    /// Pull request URL at authorization time.
    pub pr_url: String,
    /// Pull request number at authorization time.
    pub pr_number: u64,
    /// Monotonic count of prior consumed grants (`extra_attempts`) at admission.
    pub grant_index: usize,
}

/// Prior verification evidence detached and preserved before running recovered landing sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedVerification {
    /// When the prior verification evidence was archived.
    pub archived_at: Timestamp,
    /// Head that previously held the archived verification evidence.
    pub head: String,
    /// Previous `gate_head`.
    pub gate_head: Option<String>,
    /// Previous `gate_ran`.
    pub gate_ran: bool,
    /// Previous gate command outcomes.
    pub gate: Vec<CommandOutcome>,
    /// Previous current-head E2E record.
    pub head_e2e: Option<GateRepairE2e>,
    /// Previous post-repair E2E record.
    pub gate_repair_e2e: Option<GateRepairE2e>,
    /// Previous `(round, verified_head)` pairs detached from review rounds.
    #[serde(default)]
    pub review_verified_heads: Vec<(usize, Option<String>)>,
}

/// One explicit owner recovery authorization, consumed durably before any Git mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Authorization {
    /// Durable request key (`<actor>/<request>`).
    pub request: String,
    /// Owning task id.
    pub task: String,
    /// Exact saved run id authorized to recover.
    pub run: String,
    /// Immutable stop snapshot authorized by this request.
    pub stop: Stop,
    /// Verified admission scope rechecked on dispatch and execution.
    pub scope: Scope,
    /// Owner decision timestamp committed on the owning task.
    #[serde(default)]
    pub decision: Option<Timestamp>,
    /// Queue resumption generation committed on the owning task, if any.
    #[serde(default)]
    pub generation: Option<u64>,
    /// Extra landing sync attempts granted upon consumption (always `GRANT_ATTEMPTS`).
    pub granted_attempts: usize,
    /// When this authorization was recorded.
    pub at: Timestamp,
    /// Persisted `true` before any Git mutation or budget increment.
    pub consumed: bool,
    /// When consumed or refused.
    #[serde(default)]
    pub consumed_at: Option<Timestamp>,
    /// Refusal reason when invalidated before execution; `consumed` is also set `true`.
    #[serde(default)]
    pub invalidated: Option<String>,
    /// Prior verification evidence archived when this authorization was consumed.
    #[serde(default)]
    pub archived_verification: Option<ArchivedVerification>,
}

impl Authorization {
    /// Consume a refused approval without granting recovery, retaining the reason.
    pub(crate) fn refuse(&mut self, reason: impl Into<String>) {
        self.invalidated = Some(reason.into());
        self.consumed = true;
        if self.consumed_at.is_none() {
            self.consumed_at = Some(Timestamp::now());
        }
    }
}

/// Persistent landing base-sync exhaustion stop and authorization history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recovery {
    /// Total extra landing sync attempts granted by consumed authorizations.
    #[serde(default)]
    pub extra_attempts: usize,
    /// Append-only history of landing sync exhaustion stops.
    #[serde(default)]
    pub stops: Vec<Stop>,
    /// Active exhaustion stop awaiting explicit owner recovery, if any.
    #[serde(default)]
    pub stop: Option<Stop>,
    /// Append-only history of explicit owner recovery authorizations.
    #[serde(default)]
    pub authorizations: Vec<Authorization>,
}

/// Minimal live pull request metadata verified at admission, dispatch, and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPr {
    /// Pull request URL.
    pub url: String,
    /// Pull request number.
    pub number: u64,
    /// Whether the pull request is currently open.
    pub open: bool,
    /// Current pull request head SHA reported by the forge.
    pub head: String,
    /// Pull request base branch reported by the forge, when readable.
    pub base_branch: Option<String>,
}

/// Injectable forge observer for pull request head and lifecycle verification.
pub trait PrObserver {
    /// Observe the current pull request state and head SHA.
    fn observe_pr(
        &self,
        repo: &Path,
        pr_url: &str,
    ) -> impl std::future::Future<Output = Result<ObservedPr>> + Send;
}

/// Production forge observer backed by `gh pr view`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GhPrObserver;

impl PrObserver for GhPrObserver {
    async fn observe_pr(&self, repo: &Path, pr_url: &str) -> Result<ObservedPr> {
        crate::land::observe_pr_summary(repo, pr_url).await
    }
}

fn matches_exhausted_detail(detail: &str) -> bool {
    let trimmed = detail.trim();
    trimmed == EXHAUSTED_DETAIL
        || trimmed == format!("could not synchronize landing base: {EXHAUSTED_DETAIL}")
}

fn matches_exhausted_event(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed == format!("stopped: could not synchronize landing base: {EXHAUSTED_DETAIL}")
        || trimmed == format!("stopped: {EXHAUSTED_DETAIL}")
        || trimmed == format!("gave up: {EXHAUSTED_DETAIL}")
        || trimmed == format!("gave up: could not synchronize landing base: {EXHAUSTED_DETAIL}")
}

fn legacy_exhausted_diagnostic(state: &RunState) -> bool {
    if let Some(last_land) = state.events.iter().rev().find(|e| e.node == "land") {
        matches_exhausted_event(&last_land.message)
    } else {
        state
            .merge
            .as_ref()
            .is_some_and(|m| !m.ok && matches_exhausted_detail(&m.detail))
    }
}

fn exhausted_stop_timestamp(state: &RunState) -> Timestamp {
    state
        .events
        .iter()
        .rev()
        .find(|e| e.node == "land" && matches_exhausted_event(&e.message))
        .map_or(state.updated_at, |e| e.at)
}

impl RunState {
    /// Effective landing base-sync attempt limit including consumed recovery grants.
    pub fn land_sync_budget(&self) -> usize {
        self.config
            .graph
            .land_rounds
            .saturating_add(self.land_sync_recovery.extra_attempts)
            .saturating_sub(
                self.land_sync_recovery
                    .authorizations
                    .iter()
                    .filter(|a| a.invalidated.is_some() && a.archived_verification.is_some())
                    .count(),
            )
    }

    /// Remaining lifetime recovery grants available for this run.
    pub fn land_sync_grants_remaining(&self) -> usize {
        MAX_GRANTS.saturating_sub(self.land_sync_recovery.extra_attempts)
    }

    /// Centralized predicate recognizing both structured and legacy landing
    /// base-sync exhaustion stops.
    pub fn land_sync_exhausted(&self) -> bool {
        if self.status != RunStatus::Blocked {
            return false;
        }
        if self.released_to.is_some()
            || self.retired_to.is_some()
            || self.pr_taken_over_by.is_some()
        {
            return false;
        }
        if !crate::handover::landing_checkpoint(self)
            || self.base_sync_blocked()
            || self.pr_publication_pending
        {
            return false;
        }
        if self.merge_decision.is_some()
            || self.resource_block.is_some()
            || self.sequencer_stop.is_some()
            || self.land_rebase_from.is_some()
            || self.land_sync_target.is_some()
        {
            return false;
        }
        if self.merge_repairs.last().is_some_and(|r| {
            matches!(
                r.phase,
                MergeRepairPhase::Requested
                    | MergeRepairPhase::Fixing
                    | MergeRepairPhase::Reviewing
            )
        }) || self
            .repair_grants
            .last()
            .is_some_and(|g| g.phase != crate::repair_budget::Phase::Complete)
            || self
                .rebase_fixes
                .iter()
                .any(|r| r.stop.is_none() && !r.finished)
        {
            return false;
        }
        if !self.land_sync_pending
            || self.land_sync_attempts == 0
            || self.land_sync_attempts < self.land_sync_budget()
        {
            return false;
        }
        if let Some(stop) = &self.land_sync_recovery.stop {
            stop.detail == EXHAUSTED_DETAIL
                && stop.attempts == self.land_sync_attempts
                && stop.limit == self.land_sync_budget()
                && self
                    .pr
                    .as_ref()
                    .is_some_and(|pr| pr.url == stop.pr_url && pr.number == stop.pr_number)
                && self.land_sync_recovery.stops.contains(stop)
        } else {
            self.land_sync_recovery
                .authorizations
                .last()
                .is_some_and(|a| {
                    a.invalidated.is_some()
                        && a.archived_verification.is_some()
                        && a.stop.attempts == self.land_sync_attempts
                })
                || legacy_exhausted_diagnostic(self)
        }
    }

    /// Whether any landing sync recovery authorization is still unconsumed.
    pub fn has_unconsumed_land_sync_authorization(&self) -> bool {
        self.land_sync_recovery
            .authorizations
            .iter()
            .any(|a| !a.consumed && a.invalidated.is_none())
    }

    /// Whether the active landing exhaustion stop has a matching unconsumed authorization.
    pub fn land_sync_recovery_pending(&self) -> bool {
        self.land_sync_exhausted()
            && self
                .land_sync_recovery
                .authorizations
                .last()
                .is_some_and(|a| {
                    !a.consumed
                        && a.invalidated.is_none()
                        && a.run == self.id
                        && a.granted_attempts == GRANT_ATTEMPTS
                        && a.scope.grant_index == self.land_sync_recovery.extra_attempts
                        && self.land_sync_recovery.stop.as_ref() == Some(&a.stop)
                })
    }

    /// A grant was saved but its synchronization attempt has not been charged.
    /// A refused grant no longer contributes to the effective attempt budget.
    pub(crate) fn land_sync_unstarted_grant(&self) -> bool {
        self.status.resumable()
            && self
                .land_sync_recovery
                .authorizations
                .last()
                .is_some_and(|a| {
                    a.consumed
                        && a.invalidated.is_none()
                        && a.archived_verification.is_some()
                        && self.land_sync_attempts == a.stop.attempts
                        && self.land_sync_recovery.extra_attempts
                            == a.scope.grant_index.saturating_add(GRANT_ATTEMPTS)
                })
    }

    /// Single-line actionable guidance for CLI, notices, and reports.
    pub fn land_sync_next_action(&self) -> String {
        let attempts = self.land_sync_attempts;
        let limit = self.land_sync_budget();
        let grants = self.land_sync_recovery.extra_attempts;
        let head = self
            .land_sync_recovery
            .stop
            .as_ref()
            .map(|s| s.head.as_str())
            .or(self.pr_pushed_head.as_deref())
            .unwrap_or("<full-head-sha>");
        if let Some(reason) = self
            .land_sync_recovery
            .authorizations
            .last()
            .and_then(|a| a.invalidated.as_ref())
            && !self.land_sync_recovery_pending()
        {
            if self.land_sync_grants_remaining() == 0 {
                return format!(
                    "Landing base-sync recovery approval invalidated: {reason}; lifetime grant limit reached ({grants}/{MAX_GRANTS})"
                );
            }
            return format!(
                "Landing base-sync recovery approval invalidated: {reason}. Authorize a new bounded attempt with `magi task control recover-land-sync {} --request <unique-id> --head {head}` ({attempts}/{limit} attempts, {grants}/{MAX_GRANTS} grants used)",
                self.id
            );
        }
        if self.land_sync_recovery_pending() || self.land_sync_unstarted_grant() {
            return format!(
                "Landing base-sync recovery approval recorded; resume the retained run to spend {GRANT_ATTEMPTS} approved synchronization attempt ({attempts}/{limit} attempts, {grants}/{MAX_GRANTS} recovery grants used)"
            );
        }
        if self.land_sync_grants_remaining() == 0 {
            return format!(
                "Lifetime landing base-sync recovery limit reached ({grants}/{MAX_GRANTS} grants used, {attempts}/{limit} attempts); inspect the retained branch and PR before manual intervention"
            );
        }
        format!(
            "Authorize {GRANT_ATTEMPTS} bounded landing base-sync attempt with `magi task control recover-land-sync {} --request <unique-id> --head {head}` ({attempts}/{limit} attempts, {grants}/{MAX_GRANTS} recovery grants used)",
            self.id
        )
    }

    /// Persist a stable structured stop snapshot on landing sync exhaustion.
    pub(crate) fn record_land_sync_exhausted_stop(&mut self, head: &str, pr_head: &str, tip: &str) {
        let Some(pr) = self.pr.as_ref() else {
            return;
        };
        let limit = self.land_sync_budget();
        if self.land_sync_recovery.stop.as_ref().is_some_and(|s| {
            s.attempts == self.land_sync_attempts
                && s.limit == limit
                && s.head.eq_ignore_ascii_case(head)
                && s.pr_head.eq_ignore_ascii_case(pr_head)
                && s.pr_url == pr.url
                && s.pr_number == pr.number
                && s.detail == EXHAUSTED_DETAIL
        }) {
            return;
        }
        let at = exhausted_stop_timestamp(self);
        let stop = Stop {
            attempts: self.land_sync_attempts,
            limit,
            head: head.to_ascii_lowercase(),
            pr_head: pr_head.to_ascii_lowercase(),
            pr_url: pr.url.clone(),
            pr_number: pr.number,
            tip: tip.to_ascii_lowercase(),
            detail: EXHAUSTED_DETAIL.to_owned(),
            at,
        };
        if !self.land_sync_recovery.stops.contains(&stop) {
            self.land_sync_recovery.stops.push(stop.clone());
        }
        self.land_sync_recovery.stop = Some(stop);
    }

    /// Atomically consume a validated pending authorization, add its bounded
    /// extra attempt, archive prior verification evidence, and require fresh
    /// head-bound E2E and gate before publication.
    ///
    /// Callers MUST save `RunState` immediately before running any Git mutation.
    pub(crate) fn consume_land_sync_recovery(
        &mut self,
        head: &str,
        pr_head: &str,
    ) -> std::result::Result<(), String> {
        if !self.land_sync_recovery_pending() {
            return Err("no matching pending landing base-sync recovery authorization".into());
        }
        let now = Timestamp::now();
        let granted = self
            .land_sync_recovery
            .authorizations
            .last()
            .map(|a| a.granted_attempts)
            .unwrap_or(0);
        let Some(next_extra) = self.land_sync_recovery.extra_attempts.checked_add(granted) else {
            return Err("landing base-sync grant counter overflow".into());
        };
        if granted != GRANT_ATTEMPTS || next_extra > MAX_GRANTS {
            if let Some(auth) = self.land_sync_recovery.authorizations.last_mut() {
                auth.refuse(format!(
                    "lifetime landing base-sync recovery limit reached ({}/{MAX_GRANTS})",
                    self.land_sync_recovery.extra_attempts
                ));
            }
            return Err("lifetime landing base-sync recovery limit reached".into());
        }
        let head_mismatch = self
            .land_sync_recovery
            .authorizations
            .last()
            .is_some_and(|a| {
                !a.stop.head.eq_ignore_ascii_case(head)
                    || !a.stop.pr_head.eq_ignore_ascii_case(pr_head)
            });
        if head_mismatch {
            if let Some(auth) = self.land_sync_recovery.authorizations.last_mut() {
                auth.refuse(format!(
                    "retained/published HEAD changed from {}/{} to {head}/{pr_head}",
                    auth.stop.head, auth.stop.pr_head
                ));
            }
            return Err("retained or published HEAD changed before consumption".into());
        }

        // Detach and preserve all prior verification evidence so fresh gate and
        // current-head E2E MUST run even if HEAD stays unchanged.
        let gate = std::mem::take(&mut self.gate);
        let gate_head = self.gate_head.take();
        let gate_ran = std::mem::replace(&mut self.gate_ran, false);
        let head_e2e = self.head_e2e.take();
        let gate_repair_e2e = self.gate_repair_e2e.take();
        self.pre_gate.clear();
        self.pre_gate_commit = None;

        let mut review_verified_heads = Vec::with_capacity(self.reviews.len());
        for review in &mut self.reviews {
            review_verified_heads.push((review.round, review.verified_head.take()));
        }

        if gate_ran || !gate.is_empty() || head_e2e.is_some() {
            self.verification_history.push(VerificationEvidence {
                superseded_by: None,
                archived_at: Some(now),
                fixes_spent: Some(self.gate_fixes.len()),
                gate_head: gate_head.clone(),
                gate_ran,
                gate: gate.clone(),
                e2e: head_e2e.clone(),
            });
        }
        if let Some(repair_e2e) = &gate_repair_e2e {
            self.verification_history.push(VerificationEvidence {
                superseded_by: None,
                archived_at: Some(now),
                fixes_spent: Some(self.gate_fixes.len()),
                gate_head: None,
                gate_ran: false,
                gate: Vec::new(),
                e2e: Some(repair_e2e.clone()),
            });
        }

        let archived = ArchivedVerification {
            archived_at: now,
            head: head.to_ascii_lowercase(),
            gate_head,
            gate_ran,
            gate,
            head_e2e,
            gate_repair_e2e,
            review_verified_heads,
        };

        let auth = self
            .land_sync_recovery
            .authorizations
            .last_mut()
            .expect("pending authorization exists");
        auth.consumed = true;
        auth.consumed_at = Some(now);
        auth.archived_verification = Some(archived);

        self.land_sync_recovery.extra_attempts = next_extra;
        self.land_sync_recovery.stop = None;
        Ok(())
    }
}

/// Retry an observation failure without changing the accepted owner generation.
pub(crate) fn queue_existing_authorization(queue: &Queue, task: &mut Task, run: &str) -> bool {
    let Some(state) = latest_exhausted_run(queue, task) else {
        return false;
    };
    if state.id != run || !(state.land_sync_recovery_pending() || state.land_sync_unstarted_grant())
    {
        return false;
    }
    let Some(auth) = state.land_sync_recovery.authorizations.last() else {
        return false;
    };
    if task.id != auth.task
        || !task.assignment_accepts(&state)
        || task.instruction != auth.scope.instruction
        || task.operator_decided_at != auth.decision
        || auth
            .generation
            .is_some_and(|g| task.resumption.as_ref().map(|r| r.generation) != Some(g))
        || !task.land_sync_recovery_requests.contains(&auth.request)
        || task.fresh_start
        || task.review_branch.is_some()
        || task.pr_recovery.is_some()
        || matches!(task.status, TaskStatus::Running | TaskStatus::Done)
    {
        return false;
    }
    task.status = TaskStatus::Queued;
    task.hold_source = None;
    task.hold_reason = None;
    task.held_at = None;
    true
}

/// Whether `task`'s latest run is held by landing base-sync exhaustion without
/// an active pending recovery authorization.
pub(crate) fn held_task(queue: &Queue, task: &Task) -> bool {
    held_run(queue, task).is_some()
}

/// Load `task`'s latest run when it is blocked on landing base-sync exhaustion
/// without an active pending recovery authorization.
pub(crate) fn held_run(queue: &Queue, task: &Task) -> Option<RunState> {
    latest_exhausted_run(queue, task).filter(|state| !state.land_sync_recovery_pending())
}

/// Load `task`'s latest run whenever it is currently in landing base-sync exhaustion
/// (with or without a pending recovery authorization).
pub(crate) fn latest_exhausted_run(queue: &Queue, task: &Task) -> Option<RunState> {
    let home = queue.root().parent()?;
    task.execution_runs()
        .last()
        .and_then(|id| RunState::load_under(id, home).ok())
        .filter(|state| {
            state.land_sync_exhausted()
                || state.has_unconsumed_land_sync_authorization()
                || state.land_sync_unstarted_grant()
        })
}

/// Revalidate all ownership, task, HEAD, open-PR, and base-lineage invariants
/// for a pending landing sync recovery authorization.
///
/// Used both at daemon dispatch time (`task_override = Some(task)`) and after
/// acquiring the execution lease inside `land_with` (`task_override = None`,
/// `observed_override = Some(&observed_pr)`).
#[derive(Debug)]
pub(crate) struct ValidationFailure {
    pub reason: String,
    pub observation_failed: bool,
}

impl ValidationFailure {
    fn observation(reason: String) -> Self {
        Self {
            reason,
            observation_failed: true,
        }
    }
}

impl From<String> for ValidationFailure {
    fn from(reason: String) -> Self {
        Self {
            reason,
            observation_failed: false,
        }
    }
}

impl From<&str> for ValidationFailure {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}

pub(crate) async fn validate_pending_authorization<O: PrObserver>(
    home: &Path,
    state: &RunState,
    task_override: Option<&Task>,
    observed_override: Option<&ObservedPr>,
    observer: &O,
) -> std::result::Result<(String, String), ValidationFailure> {
    let unstarted = state.land_sync_unstarted_grant();
    if !state.land_sync_recovery_pending() && !unstarted {
        return Err("landing base-sync stop or pending authorization state changed".into());
    }
    if let Err(error) = crate::pr_head::ensure_owner(state) {
        return Err(format!("run ownership check failed: {error:#}").into());
    }
    let auth = state
        .land_sync_recovery
        .authorizations
        .last()
        .ok_or_else(|| "missing pending landing base-sync authorization".to_owned())?;
    if auth.invalidated.is_some() {
        return Err("landing base-sync recovery authorization was invalidated".into());
    }
    if auth.granted_attempts != GRANT_ATTEMPTS
        || (!unstarted && state.land_sync_grants_remaining() < auth.granted_attempts)
    {
        return Err(format!(
            "lifetime landing base-sync recovery limit reached ({}/{MAX_GRANTS})",
            state.land_sync_recovery.extra_attempts
        )
        .into());
    }

    if !auth.task.is_empty() {
        let loaded_task;
        let task = match task_override {
            Some(task) => task,
            None => {
                loaded_task = Queue::at(home.join("queue")).get(&auth.task).map_err(|e| {
                    ValidationFailure::observation(format!(
                        "owning task {} unreadable: {e:#}",
                        auth.task
                    ))
                })?;
                &loaded_task
            }
        };
        if task.id != auth.task {
            return Err("authorization targets a different task".into());
        }
        if state
            .origin
            .as_ref()
            .and_then(|o| o.task.as_deref())
            .is_some_and(|owner| owner != task.id)
        {
            return Err("run belongs to another task".into());
        }
        if task.execution_runs().last().map(String::as_str) != Some(state.id.as_str()) {
            return Err("run is no longer the task's latest execution".into());
        }
        if !task.assignment_accepts(state) || task.reassignments.len() != auth.scope.assignment {
            return Err("task repository assignment changed since authorization".into());
        }
        if task.status == TaskStatus::Done
            || task.fresh_start
            || task.review_branch.is_some()
            || task.pr_recovery.is_some()
        {
            return Err("task queue state changed since authorization".into());
        }
        if task.instruction != auth.scope.instruction
            || !crate::daemon::compatible_run_instruction(state, task)
        {
            return Err("task instruction changed since authorization".into());
        }
        if task.operator_decided_at != auth.decision {
            return Err("a newer owner decision superseded this recovery authorization".into());
        }
        if let Some(generation) = auth.generation
            && task.resumption.as_ref().map(|r| r.generation) != Some(generation)
        {
            return Err("task resumption generation changed since authorization".into());
        }
        if task_override.is_some()
            && !task
                .resume_override
                .as_ref()
                .is_some_and(|pin| pin.pinned_run.as_deref() == Some(state.id.as_str()))
            && !(task.resume_override.is_none()
                && task.land_sync_recovery_requests.contains(&auth.request))
        {
            return Err("task exact-run pin is missing or targets another run".into());
        }
    }

    let winner = state
        .winner()
        .ok_or_else(|| "run has no retained winner".to_owned())?;
    let clean = git::is_clean(&winner.worktree).await.map_err(|e| {
        ValidationFailure::observation(format!(
            "could not inspect retained worktree cleanliness: {e:#}"
        ))
    })?;
    if !clean {
        return Err("retained worktree has uncommitted changes".into());
    }
    let scratch = home.join("runs").join(&state.id).join("rebase");
    let rebase_in_progress = git::checked_rebase_in_progress(&scratch)
        .await
        .map_err(|e| {
            ValidationFailure::observation(format!("could not inspect scratch rebase state: {e:#}"))
        })?;
    if rebase_in_progress {
        return Err("landing rebase is already in progress in scratch worktree".into());
    }
    let local_head = git::rev_parse(&winner.worktree, "HEAD")
        .await
        .map_err(|e| {
            ValidationFailure::observation(format!("could not read retained worktree HEAD: {e:#}"))
        })?;
    let branch_head = git::rev_parse(&state.repo, &format!("refs/heads/{}", winner.branch))
        .await
        .map_err(|e| {
            ValidationFailure::observation(format!("could not read winner branch ref: {e:#}"))
        })?;
    if local_head != branch_head {
        return Err("retained worktree HEAD does not match the winner branch ref".into());
    }
    if !local_head.eq_ignore_ascii_case(&auth.stop.head) {
        return Err(format!(
            "retained HEAD changed from {} to {local_head}",
            auth.stop.head
        )
        .into());
    }
    if !state
        .pr_pushed_head
        .as_deref()
        .is_some_and(|h| h.eq_ignore_ascii_case(&local_head))
    {
        return Err("saved pr_pushed_head no longer matches the retained HEAD".into());
    }
    if !auth.stop.pr_head.eq_ignore_ascii_case(&local_head)
        || !auth.scope.pr_head.eq_ignore_ascii_case(&local_head)
    {
        return Err("authorized PR head does not match the retained HEAD".into());
    }
    if !state.pr.as_ref().is_some_and(|pr| {
        pr.state == "open" && pr.url == auth.scope.pr_url && pr.number == auth.scope.pr_number
    }) {
        return Err("recorded pull request metadata changed since authorization".into());
    }

    let fetched_pr;
    let observed = match observed_override {
        Some(observed) => observed,
        None => {
            fetched_pr = observer
                .observe_pr(&state.repo, &auth.scope.pr_url)
                .await
                .map_err(|e| {
                    ValidationFailure::observation(format!(
                        "could not verify open pull request state: {e:#}"
                    ))
                })?;
            &fetched_pr
        }
    };
    if !observed.open {
        return Err("pull request is no longer open".into());
    }
    if observed.url != auth.scope.pr_url || observed.number != auth.scope.pr_number {
        return Err("observed pull request does not match authorized pull request".into());
    }
    if observed.head.len() != 40 || !observed.head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ValidationFailure::observation(
            "observed pull request head is unreadable".into(),
        ));
    }
    if !observed.head.eq_ignore_ascii_case(&local_head) {
        return Err(format!(
            "published PR head changed from {} to {}",
            auth.scope.pr_head, observed.head
        )
        .into());
    }
    if observed
        .base_branch
        .as_deref()
        .is_some_and(|b| !b.is_empty() && b != state.base_branch)
    {
        return Err("pull request base branch changed since authorization".into());
    }

    let fetched = git::fetch(&state.repo, &state.config.merge.remote, &state.base_branch)
        .await
        .map_err(|e| {
            ValidationFailure::observation(format!(
                "could not fetch base branch for revalidation: {e:#}"
            ))
        })?;
    if !fetched.ok() {
        return Err(ValidationFailure::observation(format!(
            "could not fetch base branch for revalidation: {}",
            fetched.stderr
        )));
    }
    let onto = format!("{}/{}", state.config.merge.remote, state.base_branch);
    let current_tip = git::rev_parse(&state.repo, &onto).await.map_err(|e| {
        ValidationFailure::observation(format!("could not resolve fetched base tip: {e:#}"))
    })?;
    let descends_scope = git::ancestry(&state.repo, &auth.scope.tip, &current_tip)
        .await
        .map_err(|e| {
            ValidationFailure::observation(format!(
                "could not verify base tip lineage against authorized tip: {e:#}"
            ))
        })?;
    if !descends_scope {
        return Err(format!(
            "fetched base tip {current_tip} does not fast-forward from authorized tip {}",
            auth.scope.tip
        )
        .into());
    }
    let descends_stop = git::ancestry(&state.repo, &auth.stop.tip, &current_tip)
        .await
        .map_err(|e| {
            ValidationFailure::observation(format!(
                "could not verify base tip lineage against stopped tip: {e:#}"
            ))
        })?;
    if !descends_stop {
        return Err(format!(
            "fetched base tip {current_tip} does not fast-forward from stopped tip {}",
            auth.stop.tip
        )
        .into());
    }

    Ok((local_head, observed.head.clone()))
}

/// Result of dispatch-time revalidation for a landing base-sync exhaustion stop.
pub(crate) enum DispatchDecision {
    /// Proceed with ordinary execution or validated landing sync recovery.
    Proceed,
    /// Hold the task without spending an attempt or starting a replacement run.
    Hold {
        reason: String,
        state: Box<RunState>,
    },
}

/// Dispatch-time gate using `gh pr view` to revalidate pending recovery approvals.
pub(crate) async fn dispatch_check(queue: &Queue, home: &Path, task: &Task) -> DispatchDecision {
    dispatch_check_with(queue, home, task, &GhPrObserver).await
}

/// Injectable dispatch-time gate for unit and integration testing.
pub(crate) async fn dispatch_check_with<O: PrObserver>(
    queue: &Queue,
    home: &Path,
    task: &Task,
    observer: &O,
) -> DispatchDecision {
    let Some(state) = latest_exhausted_run(queue, task) else {
        return DispatchDecision::Proceed;
    };
    // Explicit exact-run resume may observe an externally closed PR without
    // authorizing any branch synchronization.
    if task
        .resume_override
        .as_ref()
        .is_some_and(|pin| pin.pinned_run.as_deref() == Some(state.id.as_str()))
        && let Some(pr) = &state.pr
        && let Ok(observed) = observer.observe_pr(&state.repo, &pr.url).await
        && !observed.open
        && observed.url == pr.url
        && observed.number == pr.number
    {
        return DispatchDecision::Proceed;
    }
    if !state.has_unconsumed_land_sync_authorization() && !state.land_sync_unstarted_grant() {
        return DispatchDecision::Hold {
            reason: format!(
                "Landing base synchronization remains stopped. {} Ordinary resume or release grants no additional synchronization budget and starts no replacement competition.",
                state.land_sync_next_action()
            ),
            state: Box::new(state),
        };
    }
    match validate_pending_authorization(home, &state, Some(task), None, observer).await {
        Ok(_) => DispatchDecision::Proceed,
        Err(failure) => {
            let reason = failure.reason;
            let mut updated = state;
            if !failure.observation_failed
                && let Ok(_owner) = crate::pr_head::owner_lock(home, &updated.id)
                && let Ok(mut reloaded) = RunState::load_under(&updated.id, home)
                && let Ok(claimed) =
                    crate::daemon::is_working_on_other(home, &reloaded.id, Timestamp::now())
                && reloaded.liveness_under(home, claimed) == crate::run::Liveness::Dead
                && let Ok(_exec) = crate::driver::maintenance(home, &reloaded.id)
                && (reloaded.land_sync_recovery_pending() || reloaded.land_sync_unstarted_grant())
                && let Some(auth) = reloaded.land_sync_recovery.authorizations.last_mut()
                && auth.invalidated.is_none()
                && updated
                    .land_sync_recovery
                    .authorizations
                    .last()
                    .is_some_and(|prior| prior.request == auth.request)
            {
                auth.refuse(&reason);
                reloaded.event(
                    "land-sync-recovery",
                    format!("dispatch revalidation refused: {reason}"),
                );
                let _ = reloaded.save_under(home);
                updated = reloaded;
            }
            DispatchDecision::Hold {
                reason: format!(
                    "Landing base-sync recovery refused at dispatch: {reason}. {}",
                    updated.land_sync_next_action()
                ),
                state: Box::new(updated),
            }
        }
    }
}

impl Task {
    /// Anchor a landing base-sync exhaustion hold stably across driver passes.
    pub(crate) fn land_sync_notice(&mut self, state: &RunState) {
        if !state.land_sync_exhausted() {
            return;
        }
        let stop = state.land_sync_recovery.stop.as_ref();
        self.held_at = Some(stop.map_or_else(|| exhausted_stop_timestamp(state), |s| s.at));
        self.notice_cause("land-sync-exhausted", Some(&state.id), None);
        if let Some(cause) = &mut self.hold_notice_cause {
            cause.target = Some(match stop {
                Some(s) => format!("{}:{}:{}:{}", s.head, s.attempts, s.limit, s.at),
                None => format!(
                    "legacy:{}:{}:{}",
                    state.pr_pushed_head.as_deref().unwrap_or("unknown"),
                    state.land_sync_attempts,
                    state.land_sync_budget()
                ),
            });
        }
    }
}
