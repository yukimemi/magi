//! Explicit, head-bound repair grants. The task ledger and exact-run queue pin
//! are committed together; a driver imports the receipt only under its lease.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod legacy;
pub mod reconcile;

pub use crate::base_sync_recovery::{Stop, StopKind, ensure_generic_release};

use crate::{
    git,
    queue::{Queue, Task, TaskStatus},
    run::{Liveness, RunState},
};

/// The exhausted repair path the operator authorizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Repair ordinary verification gate failures, including landing gates.
    Gate,
    /// Repair the last exhausted review's findings or failed E2E.
    Review,
    /// Resolve one retained textual base conflict without rewriting history.
    BaseConflict,
}

/// Immutable request identity, also retained in the task's durable receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
pub struct Request {
    /// Owning task id (full or unambiguous prefix).
    pub task: String,
    /// Exact stopped run id.
    #[arg(long)]
    pub run: String,
    /// Exhausted repair budget.
    #[arg(long, value_enum)]
    pub kind: Kind,
    /// Additional fixer attempts, from 1 through 3.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=3))]
    pub rounds: u8,
    /// Full retained winner HEAD observed by the operator.
    #[arg(long)]
    pub head: String,
    /// Last recorded pass index (zero based).
    #[arg(long)]
    pub expect_pass: usize,
    /// Durable idempotency key; reuse with identical arguments on retry.
    #[arg(long)]
    pub request: String,
    /// Operator's reason, retained verbatim in the receipt.
    #[arg(long)]
    pub reason: String,
    /// Exact owner-approved base tip, required for base-conflict repair.
    #[arg(long)]
    #[serde(default)]
    pub tip: Option<String>,
    /// Narrow owner-approved conflict resolution instruction, separate from review criteria.
    #[arg(long)]
    #[serde(default)]
    pub instruction: Option<String>,
    /// Explicitly supersede this unimported base-conflict request after its tip moved.
    #[arg(long)]
    #[serde(default)]
    pub replace_request: Option<String>,
    /// Evidence digest from inspect-base-stop, required for legacy textual stops.
    #[arg(long)]
    #[serde(default)]
    pub evidence_digest: Option<String>,
}

/// Durably proven single-commit mechanical `verify.pre_gate` transition from
/// the reviewed commit to the verified retained head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTransition {
    /// 1-based review round index whose findings or failed E2E are being repaired.
    pub review_round: usize,
    /// Commit SHA reviewed by the panel (`ReviewRound::head`).
    pub reviewed_head: String,
    /// Retained commit SHA created by `verify.pre_gate` and verified by E2E (`ReviewRound::verified_head`).
    pub verified_head: String,
    /// Timestamp recorded when E2E ran against `verified_head` (`ReviewRound::verified_at`).
    pub verified_at: jiff::Timestamp,
    /// Digest of the exact review, verification configuration, outcomes and commit events.
    #[serde(default)]
    pub evidence_digest: String,
}

/// Accepted grant and the control state against which it was authorized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Exact normalized request.
    pub request: Request,
    /// Acceptance time.
    pub at: jiff::Timestamp,
    /// Owner decision generation at acceptance.
    pub decision: Option<jiff::Timestamp>,
    /// Task instruction, sources and configuration selection at acceptance.
    pub contract: String,
    /// Immutable textual stop; no synchronization budget is renewed.
    #[serde(default)]
    pub stop: Option<crate::base_sync_recovery::Stop>,
    /// Diagnostic-only provenance; never a fabricated sequencer checkpoint.
    #[serde(default)]
    pub legacy: Option<legacy::Evidence>,
    /// Proven mechanical `pre_gate` transition pinned at review-grant acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_transition: Option<ReviewTransition>,
}

/// Durable execution checkpoint; consumed attempts never reset historical counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applied {
    /// Original authorization.
    pub receipt: Receipt,
    /// Attempts reserved before dispatch (including interrupted attempts).
    pub spent: u8,
    /// Current recovery stage.
    pub phase: Phase,
    /// Number of review records before the required fresh review.
    pub review_start: usize,
    /// Last head checkpoint outside the potentially interrupted fixer.
    pub head: String,
    /// Fresh review, E2E and final gate all cleared the repaired head.
    #[serde(default)]
    pub succeeded: bool,
    /// Write-ahead merge checkpoint, retained across interrupted delivery.
    #[serde(default)]
    pub conflict: Option<crate::base_conflict::Operation>,
}

impl Applied {
    /// Pin the observed E2E head for exact resume admission without accepting
    /// the transition. Keep the completed round available for re-proving it.
    pub(crate) fn checkpoint_provenance_retry(
        &mut self,
        head: &str,
        error: &anyhow::Error,
    ) -> bool {
        if !error.is::<ProvenanceUnavailable>() {
            return false;
        }
        self.head = head.to_owned();
        true
    }
}

/// Checkpoint saved before each potentially interrupted operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Ready to reserve the next attempt.
    Pending,
    /// Fixer may have changed files; never dispatch it again for this reservation.
    Fixing,
    /// Fresh review and E2E are owed.
    Reviewing,
    /// Fresh final gate is owed.
    Gating,
    /// No remaining work under this grant.
    Complete,
}

/// Refuse self-grants from implementation/review seats. Chats use their existing bridge.
pub fn authorize_terminal() -> Result<()> {
    ensure!(
        std::env::var_os("MAGI_RUN").is_none() && std::env::var_os("MAGI_NODE").is_none(),
        "repair grants require the operator terminal or authorized task control bridge"
    );
    Ok(())
}

pub(crate) fn contract(task: &Task, kind: Kind) -> Result<String> {
    let original = serde_json::to_string(&(
        &task.instruction,
        &task.sources,
        &task.overrides,
        &task.repo,
        &task.pr_recovery,
        &task.replacement,
    ))?;
    if kind == Kind::BaseConflict || task.review_branch.is_some() {
        Ok(serde_json::to_string(&(
            original,
            &task.review_of,
            &task.review_branch,
            &task.review_instruction,
            &task.resumption,
        ))?)
    } else {
        Ok(original)
    }
}

/// Admission-only fence: execution must remain able to consume its own decisions.
/// Caller holds the task claim, shared with answer recording and effect writers.
pub(crate) fn owner_effects_settled(home: &Path, task: &Task, state: &RunState) -> Result<()> {
    let queue = Queue::at(home.join("queue"));
    for q in crate::ask::Questions::at(home.join("questions")).snapshot()? {
        if (q.run == task.id || q.run == state.id || q.routed_task.as_deref() == Some(&task.id))
            && q.status == crate::ask::QuestionStatus::Answered
            && crate::task_chat::awaits_decision(&q)
        {
            ensure!(
                crate::task_chat::effect_receipt(home, &queue, &q, Some(&task.id))?.is_some(),
                "repair grant refused: owner effect {} is pending; wait for its durable application receipt, then retry the same request",
                q.id
            );
        }
    }
    for inquiry in crate::task_inquiry::Outbox::at(home).list()? {
        if inquiry.task == task.id
            && let Some(d) = inquiry.decision
        {
            ensure!(
                (d.request.is_none() && d.status == crate::task_decision::Status::AwaitingOwner)
                    || d.status == crate::task_decision::Status::Resolved,
                "repair grant refused: owner effect {} is {:?}; reconcile its receipt before retrying the same request",
                inquiry.id,
                d.status
            );
        }
    }
    Ok(())
}

fn controls(
    home: &Path,
    task: &Task,
    state: &RunState,
    kind: Kind,
    explicit_budget_override: bool,
) -> Result<()> {
    ensure!(
        task.execution_runs().last() == Some(&state.id)
            && task.assignment_accepts(state)
            && state.origin.as_ref().and_then(|o| o.task.as_deref()) == Some(task.id.as_str()),
        "repair grant refused: run task ownership changed"
    );
    if kind == Kind::BaseConflict || conflict_lineage(state) {
        ensure!(
            crate::daemon::compatible_run_instruction(state, task),
            "base-conflict repair refused: task instructions differ from retained review criteria {:?}; restore those criteria with task edit, then put only the narrow repair instruction in grant-repair --instruction",
            state
                .task_instruction
                .as_ref()
                .unwrap_or(&state.instruction)
        );
    }
    ensure!(
        !task.fresh_start
            && (task.review_branch.is_none()
                || ((kind == Kind::BaseConflict || conflict_lineage(state))
                    && crate::daemon::compatible_run_instruction(state, task)
                    && task
                        .review_instruction
                        .as_ref()
                        .is_none_or(|criteria| criteria == &task.instruction)
                    && state.reviewed_commits.is_some()
                    && task.review_branch.as_ref().is_some_and(|branch| state
                        .foreign_checkout
                        .as_ref()
                        .is_some_and(|checkout| &checkout.branch == branch)
                        || state
                            .winner()
                            .is_some_and(|winner| &winner.branch == branch))))
            && task.replacement.is_none()
            && task
                .resume_override
                .as_ref()
                .and_then(|pin| pin.pinned_run.as_ref())
                .is_none_or(|run| run == &state.id),
        "repair grant refused: task has a different continuation selected"
    );
    ensure!(
        (!task.operator_held() || explicit_budget_override) && task.blocked_by.is_empty(),
        "repair grant refused: unresolved owner hold or dependency; resolve it explicitly first"
    );
    ensure!(
        task.status != TaskStatus::Done && state.status.resumable(),
        "repair grant refused: task or run is final"
    );
    crate::pr_head::ensure_owner_record(state, Some(state))?;
    ensure!(
        state.released_to.is_none()
            && state.merge_decision.is_none()
            && (!state.base_sync_blocked() || kind == Kind::BaseConflict)
            && state.github_text.as_ref().is_none_or(|t| t.resolved)
            && state
                .merge_repairs
                .last()
                .is_none_or(|r| r.phase == crate::run::MergeRepairPhase::Complete),
        "repair grant refused: unresolved ownership, continuation or owner decision"
    );
    let questions = crate::ask::Questions::at(home.join("questions"))
        .snapshot()
        .context("repair grant refused: cannot validate pending questions")?;
    if let Some(binding) = &state.land_approval {
        let question = questions
            .iter()
            .find(|q| q.id == binding.question)
            .context("repair grant refused: approval evidence is missing")?;
        ensure!(
            question.status == crate::ask::QuestionStatus::Abandoned
                || crate::land::approval(question.resolution().as_deref())
                    == crate::land::Approval::Merge,
            "repair grant refused: unresolved owner approval or decision"
        );
    }
    ensure!(
        !questions.iter().any(|q| (q.run == task.id
            || q.run == state.id
            || q.routed_task.as_deref() == Some(&task.id))
            && q.status.open()
            && !reconcile::superseded_question(task, q)),
        "repair grant refused: unresolved question or approval"
    );
    Ok(())
}

async fn head_and_public(state: &RunState, head: &str) -> Result<()> {
    if let Some(checkout) = &state.foreign_checkout {
        checkout.validate(&state.repo).await?;
    }
    let winner = state
        .winner()
        .context("repair grant refused: no retained winner")?;
    ensure!(
        git::is_clean(&winner.worktree).await?,
        "repair grant refused: uncommitted work retained; commit it before requesting repair"
    );
    ensure!(
        git::current_branch(&winner.worktree).await?.as_deref() == Some(winner.branch.as_str()),
        "repair grant refused: retained worktree branch changed"
    );
    ensure!(
        git::rev_parse(&winner.worktree, "HEAD").await? == head
            && git::rev_parse(&state.repo, &format!("refs/heads/{}", winner.branch)).await? == head,
        "repair grant refused: winner branch/head changed"
    );
    public_lease(state).await
}

async fn public_lease(state: &RunState) -> Result<()> {
    let winner = state.winner().context("repair winner missing")?;
    if let Some(pr) = &state.pr {
        let view = crate::land::gh(
            &winner.worktree,
            &[
                "pr".into(),
                "view".into(),
                pr.url.clone(),
                "--json".into(),
                "state,headRefOid,headRefName,baseRefName,isCrossRepository,autoMergeRequest"
                    .into(),
            ],
        )
        .await?;
        ensure!(
            view.ok,
            "repair grant refused: cannot inspect PR: {}",
            view.combined()
        );
        let live: serde_json::Value = serde_json::from_str(&view.stdout)?;
        ensure!(
            live["state"] == "OPEN"
                && live["isCrossRepository"] == false
                && live["headRefName"] == state.pr_branch().unwrap_or(&winner.branch)
                && live["baseRefName"] == state.base_branch
                && live["headRefOid"].as_str() == state.pr_pushed_head.as_deref(),
            "repair grant refused: PR ownership or public head changed"
        );
        ensure!(
            live.get("autoMergeRequest")
                .is_some_and(serde_json::Value::is_null),
            "repair grant refused: disable PR auto-merge before requesting repair"
        );
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("repair grant refused: cannot inspect mechanical pre-gate commit provenance")]
pub(crate) struct ProvenanceUnavailable;

/// Prove that `head` is a single-commit mechanical `verify.pre_gate` transition
/// from `round.head` verified by completed E2E on `head`.
pub(crate) async fn prove_mechanical_pre_gate_commit(
    state: &RunState,
    round: &crate::run::ReviewRound,
    head: &str,
) -> Result<ReviewTransition> {
    ensure!(
        round.head != head
            && round.verified_head.as_deref() == Some(head)
            && state.pre_gate_commit.as_deref() == Some(head),
        "repair grant refused: no unfixed review failure or proven mechanical pre-gate commit at the expected head"
    );
    ensure!(
        matches!(
            round.e2e_status(),
            crate::run::E2eStatus::Passed | crate::run::E2eStatus::Failed
        ),
        "repair grant refused: mechanical pre-gate review target requires completed E2E verification"
    );
    let verified_at = round.verified_at.context(
        "repair grant refused: mechanical pre-gate review target is missing verification timestamp",
    )?;
    ensure!(
        !state.config.verify.e2e.is_empty()
            && round.e2e.len() == state.config.verify.e2e.len()
            && round
                .e2e
                .iter()
                .zip(&state.config.verify.e2e)
                .all(|(outcome, configured)| {
                    !outcome.resource_blocked && outcome.command == *configured
                }),
        "repair grant refused: recorded E2E verification is inconsistent with configured commands"
    );
    ensure!(
        !state.config.verify.pre_gate.is_empty()
            && state.pre_gate.len() == state.config.verify.pre_gate.len()
            && state
                .pre_gate
                .iter()
                .zip(&state.config.verify.pre_gate)
                .all(|(outcome, configured)| {
                    !outcome.resource_blocked && outcome.command == *configured
                }),
        "repair grant refused: recorded pre-gate outcomes are missing, resource-blocked, or inconsistent with configuration"
    );
    let commit_event = crate::graph::pre_gate_commit_event(head);
    let events: Vec<_> = state
        .events
        .iter()
        .filter(|ev| ev.node == crate::graph::PRE_GATE_EVENT_NODE)
        .collect();
    ensure!(
        events
            .iter()
            .any(|ev| ev.message == commit_event && ev.at <= verified_at),
        "repair grant refused: missing pre-verification mechanical pre-gate commit event"
    );
    let winner = state
        .winner()
        .context("repair grant refused: no retained winner")?;
    let provenance = git::commit_provenance(&winner.worktree, head)
        .await
        .context(ProvenanceUnavailable)?;
    ensure!(
        provenance.commit == head
            && provenance.parents.len() == 1
            && provenance.parents[0] == round.head
            && provenance.message == crate::graph::PRE_GATE_COMMIT_MESSAGE
            && provenance.author_name == git::NEUTRAL_IDENTITY_NAME
            && provenance.author_email == git::NEUTRAL_IDENTITY_EMAIL
            && provenance.committer_name == git::NEUTRAL_IDENTITY_NAME
            && provenance.committer_email == git::NEUTRAL_IDENTITY_EMAIL,
        "repair grant refused: retained head is not a single-commit mechanical pre-gate transition from the reviewed head"
    );
    Ok(ReviewTransition {
        review_round: round.round,
        reviewed_head: round.head.clone(),
        verified_head: head.to_owned(),
        verified_at,
        evidence_digest: Sha256::digest(serde_json::to_vec(&(
            round,
            &state.config.verify.e2e,
            &state.config.verify.pre_gate,
            &state.pre_gate,
            events,
            head,
        ))?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect(),
    })
}

/// Prove that `head` is either the exact head of the last unfixed review failure
/// or the durably proven single-commit mechanical `verify.pre_gate` descendant
/// that mandatory E2E verified for that review round.
///
/// Returns `Ok(None)` when `review.head == head` and `Ok(Some(ReviewTransition))`
/// only when every durable state, event, and Git provenance requirement holds.
pub async fn prove_review_target(state: &RunState, head: &str) -> Result<Option<ReviewTransition>> {
    let last = state
        .reviews
        .last()
        .context("repair grant refused: no unfixed review failure at the expected head")?;
    ensure!(
        !last.clean
            && last.fix.is_none()
            && (last.blocking > 0 || last.e2e_status() == crate::run::E2eStatus::Failed),
        "repair grant refused: no unfixed review failure at the expected head"
    );
    if last.head == head {
        return Ok(None);
    }
    prove_mechanical_pre_gate_commit(state, last, head)
        .await
        .map(Some)
}

/// Revalidate an initial unspent review grant immediately before reserving budget
/// or entering `Phase::Fixing`.
pub(crate) async fn verify_initial_review_grant(state: &RunState, grant: &Applied) -> Result<()> {
    ensure!(
        grant.receipt.request.kind == Kind::Review
            && grant.phase == Phase::Pending
            && grant.spent == 0,
        "repair grant refused: unexpected initial review grant state"
    );
    ensure!(
        grant.head == grant.receipt.request.head,
        "repair grant refused: review grant head checkpoint changed"
    );
    let prior_grants = match state.repair_grants.split_last() {
        Some((last, prior)) if last.receipt == grant.receipt => prior,
        _ => &state.repair_grants,
    };
    let transition = prove_review_target(state, &grant.receipt.request.head).await?;
    ensure!(
        transition == grant.receipt.review_transition,
        "repair grant refused: review transition evidence changed after acceptance"
    );
    ensure!(
        state.reviews.len() >= state.config.graph.review_rounds
            || failed_conflict_repair_at(
                prior_grants,
                &grant.receipt.request.head,
                transition.as_ref(),
            ),
        "repair grant refused: review budget is not exhausted"
    );
    head_and_public(state, &grant.receipt.request.head).await
}

async fn evidence(state: &RunState, request: &Request) -> Result<Option<ReviewTransition>> {
    let pass = state.passes.last().context("repair grant refused: missing stopped pass checkpoint; inspect retained run provenance before recovery")?;
    ensure!(
        pass.index == request.expect_pass,
        "repair grant refused: stopped pass changed"
    );
    ensure!(
        !state
            .repair_grants
            .iter()
            .any(|g| g.phase != Phase::Complete),
        "repair grant refused: another repair grant is unfinished"
    );
    match request.kind {
        Kind::BaseConflict => {
            ensure!(
                request.rounds == 1,
                "base-conflict repair permits exactly one attempt"
            );
            ensure!(
                request
                    .instruction
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty() && s.len() <= 16384),
                "base-conflict repair requires --instruction with the narrow owner-approved repair (1..=16384 bytes)"
            );
            let (_, legacy) = legacy::textual_stop(
                state,
                &request.head,
                request
                    .tip
                    .as_deref()
                    .context("base-conflict repair requires --tip")?,
            )?;
            ensure!(
                legacy.as_ref().map(|e| e.digest.as_str()) == request.evidence_digest.as_deref(),
                "base-conflict repair refused: missing or stale --evidence-digest; run task inspect-base-stop {} --run {} and authorize the inspected evidence",
                request.task,
                request.run
            );
        }
        Kind::Gate => {
            ensure!(
                state.gate_fixes.len() >= state.config.graph.gate_fix_rounds
                    || failed_conflict_repair_at(&state.repair_grants, &request.head, None),
                "repair grant refused: gate repair budget is not exhausted"
            );
            ensure!(
                state.gate_ran
                    && state.gate_head.as_deref() == Some(&request.head)
                    && crate::graph::gate_fixable(&state.gate),
                "repair grant refused: no repairable gate failure at the expected head"
            );
        }
        Kind::Review => {
            ensure!(
                state.reviews.len() >= state.config.graph.review_rounds || conflict_lineage(state),
                "repair grant refused: review budget is not exhausted"
            );
            let transition = prove_review_target(state, &request.head).await?;
            ensure!(
                state.reviews.len() >= state.config.graph.review_rounds
                    || failed_conflict_repair_at(
                        &state.repair_grants,
                        &request.head,
                        transition.as_ref(),
                    ),
                "repair grant refused: review budget is not exhausted"
            );
            return Ok(transition);
        }
    }
    Ok(None)
}

#[derive(Debug, thiserror::Error)]
#[error(
    "repair grant publication could not be confirmed; check the task or repeat the identical request ID and arguments"
)]
pub(crate) struct PublicationUnknown;

/// Atomically append authorization and queue this exact run. No run JSON is
/// changed here, so a crash cannot leave an unqueued budget mutation.
pub async fn grant(queue: &Queue, home: &Path, request: Request) -> Result<Receipt> {
    grant_with_claim(queue, home, request, None, Some(jiff::Timestamp::now())).await
}

/// Only a current owner request carries a timestamp. Standing recovery and
/// goal wakes cannot use the automatic budget hold exception.
pub(crate) async fn grant_with_claim(
    queue: &Queue,
    home: &Path,
    mut request: Request,
    claimed: Option<(crate::queue::Claim, Task)>,
    owner_requested_at: Option<jiff::Timestamp>,
) -> Result<Receipt> {
    ensure!((1..=3).contains(&request.rounds), "rounds must be 1..=3");
    ensure!(
        request.replace_request.is_none() || request.kind == Kind::BaseConflict,
        "only base-conflict grants support --replace-request"
    );
    ensure!(
        !request.reason.trim().is_empty() && request.reason.len() <= 4096,
        "reason must contain 1..=4096 bytes"
    );
    ensure!(
        !request.request.is_empty()
            && request.request.len() <= 128
            && request
                .request
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "request id must contain 1..=128 letters, digits, dots, underscores or hyphens"
    );
    ensure!(
        request.head.len() == 40 && request.head.bytes().all(|b| b.is_ascii_hexdigit()),
        "head must be a full SHA"
    );
    ensure!(crate::run::is_run_id(&request.run), "invalid run id");
    request.task = queue.resolve_id(&request.task)?;
    if let Some(receipt) = queue
        .get(&request.task)?
        .repair_grants
        .iter()
        .find(|g| g.request.request == request.request)
    {
        ensure!(
            receipt.request == request,
            "repair grant refused: request id already has different arguments"
        );
        return Ok(receipt.clone());
    }
    let (_claim, mut task) = match claimed {
        Some(claimed) => {
            ensure!(claimed.1.id == request.task, "repair task changed");
            claimed
        }
        None => {
            let claim = queue
                .claim(&request.task)
                .context("repair grant refused: task queue lease is held")?;
            (claim, queue.get(&request.task)?)
        }
    };
    if let Some(receipt) = task
        .repair_grants
        .iter()
        .find(|g| g.request.request == request.request)
    {
        ensure!(
            receipt.request == request,
            "repair grant refused: request id already has different arguments"
        );
        return Ok(receipt.clone());
    }
    let expected = serde_json::to_value(&task)?;
    let _owner = crate::pr_head::owner_lock(home, &request.run)?;
    let state = RunState::load_under(&request.run, home)?;
    let explicit_budget_override = task.operator_held()
        && matches!(request.kind, Kind::Review | Kind::Gate)
        && owner_requested_at.is_some_and(|at| task.held_at.is_some_and(|held| at >= held))
        && task.held_for_recovery_budget(&state);
    controls(home, &task, &state, request.kind, explicit_budget_override)?;
    owner_effects_settled(home, &task, &state)?;
    unique_owner(queue, &task, &state)?;
    ensure!(
        task.status != TaskStatus::Running,
        "repair grant refused: task driver is running"
    );
    let claimed = crate::daemon::is_working_on_other(home, &state.id, jiff::Timestamp::now())?;
    ensure!(
        state.liveness_under(home, claimed) == Liveness::Dead,
        "repair grant refused: driver is live or unknown"
    );
    let _execution = crate::driver::maintenance(home, &state.id)?;
    let _outputs = output_leases(home, &state)?;
    ensure!(
        state.status.resumable(),
        "repair grant refused: run is final"
    );
    let queued: Vec<_> = task
        .repair_grants
        .iter()
        .filter(|receipt| {
            receipt.request.run == state.id
                && !superseded(&task, receipt)
                && !state
                    .repair_grants
                    .iter()
                    .any(|applied| applied.receipt == **receipt)
        })
        .collect();
    if let Some(replaced) = &request.replace_request {
        let old = task
            .repair_grants
            .last()
            .context("replacement refused: no queued grant")?;
        ensure!(
            queued.len() == 1
                && queued[0] == old
                && old.request.request == *replaced
                && old.request.kind == Kind::BaseConflict,
            "replacement refused: --replace-request must name the latest unimported base-conflict grant; imported or dispatched grants cannot be replaced"
        );
        ensure!(
            old.request.head == request.head
                && old.stop.as_ref()
                    == Some(
                        &legacy::textual_stop(
                            &state,
                            &request.head,
                            request
                                .tip
                                .as_deref()
                                .context("replacement requires --tip")?
                        )?
                        .0
                    )
                && old.request.tip != request.tip,
            "replacement refused: retained stop/head changed or the approved tip has not moved"
        );
        let old_tip = old
            .request
            .tip
            .as_deref()
            .context("replacement refused: old approval has no base tip")?;
        let new_tip = request
            .tip
            .as_deref()
            .context("replacement requires --tip")?;
        ensure!(
            git::is_ancestor(&state.repo, old_tip, new_tip).await,
            "replacement refused: new base tip must descend from the old approved tip; reconcile rewritten upstream history explicitly"
        );
    } else {
        ensure!(
            queued.is_empty(),
            "repair grant refused: another grant is queued; a stale unimported base-conflict approval requires explicit --replace-request <old-request-id>"
        );
    }
    let review_transition = evidence(&state, &request).await?;
    head_and_public(&state, &request.head).await?;
    conflict_target(&state, &request, None).await?;
    if request.kind == Kind::BaseConflict {
        ensure!(
            git::toplevel(&task.repo).await?.canonicalize()?
                == git::toplevel(&state.repo).await?.canonicalize()?,
            "base-conflict repair refused: task repository ownership changed"
        );
    }
    let task_contract = contract(&task, request.kind)?;
    let (stop, legacy) = if request.kind == Kind::BaseConflict {
        let (stop, legacy) = legacy::textual_stop(
            &state,
            &request.head,
            request.tip.as_deref().context("missing --tip")?,
        )?;
        (Some(stop), legacy)
    } else {
        (None, None)
    };
    let receipt = Receipt {
        stop,
        legacy,
        review_transition,
        request,
        at: jiff::Timestamp::now(),
        decision: task.operator_decided_at,
        contract: task_contract,
    };
    task.queue_repair(&receipt);
    queue.commit_repair_grant(&mut task, &expected)?;
    // The receipt proves publication, not that a driver or fixer started.
    Ok(receipt)
}

fn output_leases(home: &Path, state: &RunState) -> Result<Vec<crate::retention::OutputLease>> {
    let mut paths: std::collections::BTreeSet<_> = state
        .candidates
        .iter()
        .filter(|c| c.worktree.exists())
        .map(|c| c.worktree.clone())
        .collect();
    // Legacy rebase scratch lives beside the run record, outside seat roots.
    // Its descendants still share this repository even when a new scratch is used.
    let legacy_scratch = home.join("runs").join(&state.id).join("base-sync");
    if legacy_scratch.join(".git").is_file() {
        paths.insert(legacy_scratch);
    }
    // Reviewers and other retained seats are reset on reuse too. A stopped
    // driver does not imply their tool descendants have released the tree.
    match std::fs::read_dir(state.worktree_root()) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if path.join(".git").is_file() {
                    paths.insert(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context("repair grant refused: cannot inspect seat worktrees");
        }
    }
    paths
        .into_iter()
        .map(|path| {
            crate::retention::OutputLease::take(&path, true)
                .context("repair grant refused: seat or descendant still owns the worktree")
        })
        .collect()
}

/// Import receipts after driver lease acquisition, before beginning a new pass.
/// Caller holds the run owner lock; a stale loaded runner must never overwrite
/// a grant or restart an already reserved attempt.
pub(crate) async fn apply(home: &Path, state: &mut RunState, disk: &RunState) -> Result<()> {
    ensure!(
        serde_json::to_value(&state.repair_grants)? == serde_json::to_value(&disk.repair_grants)?,
        "repair grant checkpoint changed; reload the saved run"
    );
    let Some(owner) = state.origin.as_ref().and_then(|o| o.task.as_ref()) else {
        return Ok(());
    };
    let queue = Queue::at(home.join("queue"));
    let task = match queue.get(owner) {
        Ok(task) => task,
        Err(_) if state.repair_grants.is_empty() && !queue.path_of(owner).exists() => return Ok(()),
        Err(error) => return Err(error).context("repair task record unavailable"),
    };
    if pending(&task, disk) {
        unique_owner(&queue, &task, disk)?;
    }
    if !state.repair_grants.is_empty() || pending(&task, disk) {
        ensure!(
            state.updated_at == disk.updated_at,
            "repair grant state changed since this runner was loaded; reload the saved run"
        );
    }
    if let Some(active) = state
        .repair_grants
        .last()
        .filter(|g| g.phase != Phase::Complete)
    {
        controls(home, &task, disk, active.receipt.request.kind, false)?;
        ensure!(
            reconcile::decision_matches(home, &task, &active.receipt)?
                && contract(&task, active.receipt.request.kind)? == active.receipt.contract,
            "repair grant held: task contract or owner decision changed after acceptance"
        );
        let _outputs = output_leases(home, disk)?;
        if matches!(active.phase, Phase::Pending | Phase::Fixing) {
            conflict_target(disk, &active.receipt.request, active.receipt.stop.as_ref()).await?;
        }
        if active.phase == Phase::Pending
            && active.spent == 0
            && active.receipt.request.kind == Kind::Review
        {
            verify_initial_review_grant(disk, active).await?;
        } else if active.phase != Phase::Fixing {
            head_and_public(disk, &active.head).await?;
        } else {
            let winner = disk.winner().context("repair winner missing")?;
            ensure!(
                git::current_branch(&winner.worktree).await?.as_deref()
                    == Some(winner.branch.as_str())
                    && git::rev_parse(&winner.worktree, "HEAD").await?
                        == git::rev_parse(&disk.repo, &winner.branch).await?,
                "repair grant held: interrupted fixer branch ownership changed"
            );
            public_lease(disk).await?;
        }
    }
    let id = state.id.clone();
    for receipt in task.repair_grants.iter().filter(|r| r.request.run == id) {
        if superseded(&task, receipt) || state.repair_grants.iter().any(|g| g.receipt == *receipt) {
            continue;
        }
        controls(home, &task, disk, receipt.request.kind, false)?;
        ensure!(
            reconcile::decision_matches(home, &task, receipt)?
                && contract(&task, receipt.request.kind)? == receipt.contract,
            "repair grant held: owner decision changed after acceptance"
        );
        let transition = evidence(disk, &receipt.request).await?;
        ensure!(
            transition == receipt.review_transition,
            "repair grant held: review transition evidence changed after acceptance"
        );
        if receipt.request.kind == Kind::BaseConflict {
            ensure!(
                legacy::textual_stop(
                    disk,
                    &receipt.request.head,
                    receipt
                        .request
                        .tip
                        .as_deref()
                        .context("missing repair tip")?
                )?
                .1 == receipt.legacy,
                "legacy repair evidence changed after authorization"
            );
        }
        conflict_target(disk, &receipt.request, receipt.stop.as_ref()).await?;
        let _outputs = output_leases(home, disk)?;
        head_and_public(disk, &receipt.request.head).await?;
        if receipt.legacy.is_some() && state.base_sync_recovery.stop.is_none() {
            // Import only after re-deriving the approved evidence under the owner
            // and execution locks. Grant acceptance itself never edits the run.
            let stop = receipt
                .stop
                .clone()
                .context("legacy receipt missing stop")?;
            if !state.base_sync_recovery.stops.contains(&stop) {
                state.base_sync_recovery.stops.push(stop.clone());
            }
            state.base_sync_recovery.stop = Some(stop);
        }
        state.repair_grants.push(Applied {
            receipt: receipt.clone(),
            spent: 0,
            phase: Phase::Pending,
            review_start: state.reviews.len(),
            head: receipt.request.head.clone(),
            succeeded: false,
            conflict: None,
        });
        state.event(
            "repair-grant",
            format!(
                "request {}: {:?} +{}: {}",
                receipt.request.request,
                receipt.request.kind,
                receipt.request.rounds,
                receipt.request.reason
            ),
        );
    }
    Ok(())
}

/// An exact-run reservation keeps exhausted runs in the existing worker path.
/// Admission still validates all control state under the owner/execution locks.
pub(crate) fn pending(task: &Task, state: &RunState) -> bool {
    task.execution_runs().last() == Some(&state.id)
        && task.repair_grants.last().is_some_and(|receipt| {
            receipt.request.run == state.id
                && !state
                    .repair_grants
                    .iter()
                    .any(|g| g.receipt == *receipt && g.phase == Phase::Complete)
        })
}

/// Ordinary resume must not turn a pending grant into a new owner decision.
/// Retain its exact authorization generation, and never release a newer hold.
pub fn queue_resumption(queue: &Queue, task: &mut Task, run: &str) -> Result<bool> {
    if crate::land_sync_recovery::queue_existing_authorization(queue, task, run) {
        return Ok(true);
    }
    if task.repair_grants.is_empty() {
        return Ok(false);
    }
    let home = queue.root().parent().context("queue has no home")?;
    let state = RunState::load_under(run, home)?;
    if !pending(task, &state) {
        return Ok(false);
    }
    let receipt = task.repair_grants.last().expect("pending receipt");
    controls(home, task, &state, receipt.request.kind, false)?;
    ensure!(
        reconcile::decision_matches(home, task, receipt)?
            && contract(task, receipt.request.kind)? == receipt.contract,
        "repair grant held: owner decision or task contract changed after acceptance"
    );
    task.status = TaskStatus::Queued;
    task.hold_source = None;
    task.hold_reason = None;
    task.held_at = None;
    Ok(true)
}

/// Recheck the recorded target without fetching or renewing a synchronization interval.
async fn conflict_target(
    state: &RunState,
    request: &Request,
    stop: Option<&crate::base_sync_recovery::Stop>,
) -> Result<()> {
    if let Some(checkout) = &state.foreign_checkout {
        checkout.validate(&state.repo).await?;
    }
    if request.kind != Kind::BaseConflict {
        return Ok(());
    }
    let (derived, legacy) = legacy::textual_stop(
        state,
        &request.head,
        request.tip.as_deref().context("missing repair tip")?,
    )?;
    ensure!(
        legacy.as_ref().map(|e| e.digest.as_str()) == request.evidence_digest.as_deref(),
        "base-conflict repair held: legacy evidence digest changed; inspect-base-stop before authorizing"
    );
    if let Some(stop) = stop {
        ensure!(
            &derived == stop,
            "base-conflict repair held: stopped synchronization changed"
        );
    }
    let tracking = format!(
        "refs/remotes/{}/{}",
        state.config.merge.remote, state.base_branch
    );
    let live = git::rev_parse(&state.repo, &tracking).await?;
    if request.tip.as_ref() != Some(&live) {
        let imported = state
            .repair_grants
            .iter()
            .any(|grant| grant.receipt.request == *request);
        ensure!(
            !imported,
            "base-conflict repair held: recorded base tip moved to {live}; request {} is already imported. Its reserved operation cannot be retargeted with --replace-request; inspect the saved conflict checkpoint and retained scratch before manual reconciliation",
            request.request
        );
        if stop.is_none() {
            anyhow::bail!(
                "base-conflict repair refused: recorded base tip moved to {live}; inspect this target and explicitly request --tip {live}. An already queued approval additionally requires --replace-request <old-request-id>"
            );
        }
        let evidence_flag = if request.evidence_digest.is_some() {
            " --evidence-digest <fresh-inspected-digest>"
        } else {
            ""
        };
        anyhow::bail!(
            "base-conflict repair held: recorded base tip moved to {live}. Run `magi task inspect-base-stop {} --run {}` to inspect the new target, then explicitly replace the unimported approval with `magi task grant-repair {} --run {} --kind base-conflict --rounds 1 --head {} --tip {live} --expect-pass {}{evidence_flag} --replace-request {} --request <new-id> --reason <reason> --instruction <approved-resolution>`. No edit or synchronization budget was granted",
            request.task,
            request.run,
            request.task,
            request.run,
            request.head,
            state.passes.last().map_or(0, |pass| pass.index),
            request.request
        );
    }
    let (stopped, _) = legacy::textual_stop(state, &request.head, &live)?;
    ensure!(
        git::is_ancestor(&state.repo, &stopped.tip, &live).await,
        "base-conflict repair refused: current base {live} does not descend from recorded stop tip {}; reconcile the rewritten base explicitly before granting repair",
        stopped.tip
    );
    Ok(())
}

/// Only a typed exact-run grant can pass the textual-stop admission fence.
pub(crate) fn conflict_pending(task: &Task, state: &RunState) -> bool {
    pending(task, state)
        && task
            .repair_grants
            .last()
            .is_some_and(|r| r.request.kind == Kind::BaseConflict)
}

/// Repeat external ownership checks immediately before fast-forward delivery.
pub(crate) async fn recheck_conflict_delivery(state: &RunState, request: &Request) -> Result<()> {
    let home = crate::run::home();
    let task = Queue::at(home.join("queue")).get(&request.task)?;
    controls(&home, &task, state, request.kind, false)?;
    unique_owner(&Queue::at(home.join("queue")), &task, state)?;
    ensure!(
        git::toplevel(&task.repo).await?.canonicalize()?
            == git::toplevel(&state.repo).await?.canonicalize()?,
        "base-conflict repair refused: task repository ownership changed"
    );
    let receipt = task
        .repair_grants
        .iter()
        .find(|r| r.request == *request)
        .context("repair receipt disappeared")?;
    ensure!(
        task.operator_decided_at == receipt.decision
            && contract(&task, receipt.request.kind)? == receipt.contract,
        "base-conflict repair held: task contract or owner decision changed"
    );
    conflict_target(state, request, receipt.stop.as_ref()).await?;
    crate::pr_head::ensure_owner_record(state, Some(state))?;
    public_lease(state).await
}

fn unique_owner(queue: &Queue, task: &Task, state: &RunState) -> Result<()> {
    ensure!(
        queue.unreadable().is_empty(),
        "repair grant refused: unreadable task records prevent proving unique ownership"
    );
    ensure!(
        !queue
            .list()
            .iter()
            .any(|other| other.id != task.id && other.runs.contains(&state.id)),
        "repair grant refused: multiple tasks claim this run"
    );
    Ok(())
}

/// A settled red conflict repair remains owned by this run, not a successor review.
pub(crate) fn conflict_failure(task: &Task, state: &RunState) -> Option<String> {
    if task.execution_runs().last() == Some(&state.id)
        && !pending(task, state)
        && state.repair_grants.last().is_some_and(|g| {
            (g.receipt.request.kind == Kind::BaseConflict || conflict_lineage(state))
                && g.phase == Phase::Complete
                && !g.succeeded
        })
    {
        Some(format!(
            "Conflict repair on run {} completed without clearing fresh review or verification. Inspect its repair_grants, reviews and gate; authorize the specific remaining failure explicitly. A generic release cannot select a successor review or implementation.",
            state.id
        ))
    } else {
        None
    }
}

// Follow-up grants remain explicit and head-bound even when the conflict's
// confirmation failed before ordinary review/gate allowances were exhausted.
fn conflict_lineage(state: &RunState) -> bool {
    state.repair_grants.iter().any(|grant| {
        grant.receipt.request.kind == Kind::BaseConflict
            && grant.phase == Phase::Complete
            && grant
                .conflict
                .as_ref()
                .is_some_and(|operation| operation.result.is_some())
    })
}

fn failed_conflict_repair_at(
    grants: &[Applied],
    head: &str,
    transition: Option<&ReviewTransition>,
) -> bool {
    grants.iter().any(|grant| {
        grant.receipt.request.kind == Kind::BaseConflict
            && grant.phase == Phase::Complete
            && grant
                .conflict
                .as_ref()
                .is_some_and(|operation| operation.result.is_some())
    }) && grants.last().is_some_and(|grant| {
        grant.phase == Phase::Complete
            && !grant.succeeded
            && (grant.head == head
                || transition
                    .is_some_and(|t| t.verified_head == head && grant.head == t.reviewed_head))
    })
}

/// A replacement is an append-only task receipt; the old approval is never
/// imported or dispatched, including after a stale queue writer or restart.
fn superseded(task: &Task, receipt: &Receipt) -> bool {
    task.repair_grants.iter().any(|newer| {
        newer.request.run == receipt.request.run
            && newer.request.replace_request.as_ref() == Some(&receipt.request.request)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Candidate, CommandOutcome, ReviewRound};

    async fn git_ok(dir: &Path, args: &[&str]) {
        let out = git::git_raw(dir, args).await.unwrap();
        assert!(out.ok(), "git {:?} failed: {}", args, out.stderr);
    }

    async fn mechanical_fixture() -> (tempfile::TempDir, RunState, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-b", "main"]).await;
        git_ok(&repo, &["config", "user.name", "Test User"]).await;
        git_ok(&repo, &["config", "user.email", "test@example.com"]).await;
        git_ok(&repo, &["config", "commit.gpgsign", "false"]).await;
        std::fs::write(repo.join("note.txt"), "initial\n").unwrap();
        git_ok(&repo, &["add", "-A"]).await;
        git_ok(&repo, &["commit", "-m", "initial"]).await;
        let reviewed = git::rev_parse(&repo, "HEAD").await.unwrap();

        std::fs::write(repo.join("fmt.txt"), "formatted\n").unwrap();
        assert!(
            git::commit_all(&repo, crate::graph::PRE_GATE_COMMIT_MESSAGE)
                .await
                .unwrap()
        );
        let verified = git::rev_parse(&repo, "HEAD").await.unwrap();

        let mut config = crate::config::Config::default();
        config.verify.pre_gate = vec!["echo formatted > fmt.txt".into()];
        config.verify.e2e = vec!["test -f fixed.txt".into()];
        config.graph.review_rounds = 1;

        let mut state = RunState::new(
            repo.clone(),
            "main".into(),
            reviewed.clone(),
            "create note.txt".into(),
            config,
        );
        state.candidates.push(Candidate {
            index: 0,
            label: 'A',
            agent: "mock".into(),
            branch: "main".into(),
            worktree: repo,
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        });
        state.tally = Some(crate::run::Tally {
            first_choice: std::collections::BTreeMap::new(),
            borda: std::collections::BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 1,
            present: 1,
            quorum: 1,
            met_quorum: true,
            uncontested: None,
        });
        state.pre_gate = vec![CommandOutcome {
            command: "echo formatted > fmt.txt".into(),
            code: Some(0),
            output_tail: String::new(),
            duration_ms: 1,
            resource_blocked: false,
            ..Default::default()
        }];
        state.pre_gate_commit = Some(verified.clone());
        let pre_gate_at: jiff::Timestamp = "2026-06-01T00:00:01Z".parse().unwrap();
        let verified_at: jiff::Timestamp = "2026-06-01T00:00:02Z".parse().unwrap();
        state.events.push(crate::run::Event {
            at: pre_gate_at,
            node: crate::graph::PRE_GATE_EVENT_NODE.into(),
            message: crate::graph::pre_gate_commit_event(&verified),
        });
        state.reviews.push(ReviewRound {
            round: 1,
            head: reviewed.clone(),
            verified_head: Some(verified.clone()),
            verified_at: Some(verified_at),
            reviews: Vec::new(),
            e2e: vec![CommandOutcome {
                command: "test -f fixed.txt".into(),
                code: Some(1),
                output_tail: "missing fixed.txt".into(),
                duration_ms: 1,
                resource_blocked: false,
                ..Default::default()
            }],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 1,
            expected: 1,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        });
        (tmp, state, reviewed, verified)
    }

    #[test]
    fn receipt_review_transition_is_optional_and_backwards_compatible() {
        let mut receipt = Receipt {
            request: Request {
                task: "t1".into(),
                run: "20260601-000000-abcd".into(),
                kind: Kind::Review,
                rounds: 1,
                head: "a".repeat(40),
                expect_pass: 0,
                request: "req-1".into(),
                reason: "repair".into(),
                tip: None,
                instruction: None,
                replace_request: None,
                evidence_digest: None,
            },
            at: "2026-06-01T00:00:03Z".parse().unwrap(),
            decision: None,
            contract: "[]".into(),
            stop: None,
            legacy: None,
            review_transition: None,
        };
        let without = serde_json::to_value(&receipt).unwrap();
        assert!(without.get("review_transition").is_none());
        let decoded: Receipt = serde_json::from_value(without).unwrap();
        assert_eq!(decoded, receipt);

        receipt.review_transition = Some(ReviewTransition {
            review_round: 1,
            reviewed_head: "b".repeat(40),
            verified_head: "a".repeat(40),
            verified_at: "2026-06-01T00:00:02Z".parse().unwrap(),
            evidence_digest: "c".repeat(64),
        });
        let with = serde_json::to_value(&receipt).unwrap();
        assert_eq!(with["review_transition"]["review_round"], 1);
        let decoded: Receipt = serde_json::from_value(with).unwrap();
        assert_eq!(decoded, receipt);
    }

    #[tokio::test]
    async fn confirmation_provenance_retry_pins_exact_resume_head() {
        let (_tmp, mut state, reviewed, verified) = mechanical_fixture().await;
        state.reviews[0].clean = true;
        state.reviews[0].e2e[0].code = Some(0);
        let mut grant = Applied {
            receipt: Receipt {
                request: Request {
                    task: "t1".into(),
                    run: state.id.clone(),
                    kind: Kind::Review,
                    rounds: 2,
                    head: reviewed.clone(),
                    expect_pass: 0,
                    request: "req-1".into(),
                    reason: "repair".into(),
                    tip: None,
                    instruction: None,
                    replace_request: None,
                    evidence_digest: None,
                },
                at: "2026-06-01T00:00:00Z".parse().unwrap(),
                decision: None,
                contract: "[]".into(),
                stop: None,
                legacy: None,
                review_transition: None,
            },
            spent: 1,
            phase: Phase::Reviewing,
            review_start: 0,
            head: reviewed.clone(),
            succeeded: false,
            conflict: None,
        };
        let git_dir = state.repo.join(".git");
        let hidden = state.repo.join("hidden-git");
        std::fs::rename(&git_dir, &hidden).unwrap();
        let error = prove_mechanical_pre_gate_commit(&state, &state.reviews[0], &verified)
            .await
            .unwrap_err();
        assert!(error.is::<ProvenanceUnavailable>());
        assert!(grant.checkpoint_provenance_retry(&verified, &error));
        let mut grant: Applied =
            serde_json::from_slice(&serde_json::to_vec(&grant).unwrap()).unwrap();
        std::fs::rename(&hidden, &git_dir).unwrap();
        // Resume admission must still check the actual branch and exact HEAD.
        head_and_public(&state, &grant.head).await.unwrap();
        assert_eq!(grant.phase, Phase::Reviewing);
        assert_eq!(grant.review_start, 0);
        assert_eq!(grant.spent, 1);
        assert!(!grant.succeeded);
        assert_eq!(grant.receipt.request.head, reviewed);

        let transition = prove_mechanical_pre_gate_commit(&state, &state.reviews[0], &verified)
            .await
            .unwrap();
        assert_eq!(transition.reviewed_head, reviewed);
        state.reviews[0].head = verified.clone();
        let error = prove_mechanical_pre_gate_commit(&state, &state.reviews[0], &verified)
            .await
            .unwrap_err();
        assert!(!error.is::<ProvenanceUnavailable>());
        assert!(!grant.checkpoint_provenance_retry(&reviewed, &error));
        assert_eq!(grant.head, verified);
        git_ok(&state.repo, &["reset", "--hard", &reviewed]).await;
        assert!(head_and_public(&state, &grant.head).await.is_err());
    }

    #[tokio::test]
    async fn prove_review_target_accepts_exact_and_proven_mechanical_and_refuses_invalid() {
        let (_tmp, state, reviewed, verified) = mechanical_fixture().await;

        // Exact head match preserves existing behavior without transition metadata.
        assert_eq!(prove_review_target(&state, &reviewed).await.unwrap(), None);

        // Proven single-commit mechanical pre_gate transition succeeds.
        let transition = prove_review_target(&state, &verified)
            .await
            .unwrap()
            .expect("proven transition");
        assert_eq!(transition.review_round, 1);
        assert_eq!(transition.reviewed_head, reviewed);
        assert_eq!(transition.verified_head, verified);

        // Passed E2E with blocking findings is also valid.
        let mut passed_with_blocking = state.clone();
        passed_with_blocking.reviews[0].e2e[0].code = Some(0);
        passed_with_blocking.reviews[0].blocking = 1;
        let changed = prove_review_target(&passed_with_blocking, &verified)
            .await
            .unwrap()
            .expect("proven transition");
        assert_ne!(changed.evidence_digest, transition.evidence_digest);

        // Conflict lineage exhaustion check matches the reviewed head behind the proven transition.
        let conflict_grant = Applied {
            receipt: Receipt {
                request: Request {
                    task: "t1".into(),
                    run: state.id.clone(),
                    kind: Kind::BaseConflict,
                    rounds: 1,
                    head: reviewed.clone(),
                    expect_pass: 0,
                    request: "conflict-1".into(),
                    reason: "resolve conflict".into(),
                    tip: Some(reviewed.clone()),
                    instruction: Some("keep both".into()),
                    replace_request: None,
                    evidence_digest: None,
                },
                at: "2026-06-01T00:00:00Z".parse().unwrap(),
                decision: None,
                contract: "[]".into(),
                stop: None,
                legacy: None,
                review_transition: None,
            },
            spent: 1,
            phase: Phase::Complete,
            review_start: 0,
            head: reviewed.clone(),
            succeeded: false,
            conflict: Some(crate::base_conflict::Operation {
                paths: vec!["note.txt".into()],
                outside: String::new(),
                tree: None,
                result: Some(reviewed.clone()),
            }),
        };
        assert!(failed_conflict_repair_at(
            std::slice::from_ref(&conflict_grant),
            &verified,
            Some(&transition),
        ));
        assert!(!failed_conflict_repair_at(
            std::slice::from_ref(&conflict_grant),
            &verified,
            None,
        ));

        // Missing or mismatched verified_head / pre_gate_commit.
        let mut bad = state.clone();
        bad.reviews[0].verified_head = None;
        assert!(prove_review_target(&bad, &verified).await.is_err());

        let mut bad = state.clone();
        bad.pre_gate_commit = None;
        assert!(prove_review_target(&bad, &verified).await.is_err());

        // Incomplete E2E states and missing verified_at.
        let mut bad = state.clone();
        bad.reviews[0].e2e.clear();
        bad.reviews[0].e2e_deferred = true;
        bad.reviews[0].blocking = 1;
        assert!(prove_review_target(&bad, &verified).await.is_err());

        let mut bad = state.clone();
        bad.reviews[0].e2e[0].resource_blocked = true;
        bad.reviews[0].blocking = 1;
        assert!(prove_review_target(&bad, &verified).await.is_err());

        let mut bad = state.clone();
        bad.reviews[0].verified_at = None;
        assert!(prove_review_target(&bad, &verified).await.is_err());

        // Pre-gate config / outcomes mismatch or resource-blocked.
        let mut bad = state.clone();
        bad.config.verify.pre_gate.clear();
        assert!(prove_review_target(&bad, &verified).await.is_err());

        let mut bad = state.clone();
        bad.pre_gate[0].resource_blocked = true;
        assert!(prove_review_target(&bad, &verified).await.is_err());

        let mut bad = state.clone();
        bad.pre_gate[0].command = "different command".into();
        assert!(prove_review_target(&bad, &verified).await.is_err());

        // Missing or post-verification mechanical commit event.
        let mut bad = state.clone();
        bad.events.clear();
        assert!(prove_review_target(&bad, &verified).await.is_err());

        let mut bad = state.clone();
        bad.events[0].at = "2026-06-01T00:00:03Z".parse().unwrap();
        assert!(prove_review_target(&bad, &verified).await.is_err());

        // Arbitrary second descendant (even with identical mechanical commit message).
        std::fs::write(state.repo.join("fmt2.txt"), "second\n").unwrap();
        assert!(
            git::commit_all(&state.repo, crate::graph::PRE_GATE_COMMIT_MESSAGE)
                .await
                .unwrap()
        );
        let second = git::rev_parse(&state.repo, "HEAD").await.unwrap();
        let mut bad = state.clone();
        bad.pre_gate_commit = Some(second.clone());
        bad.reviews[0].verified_head = Some(second.clone());
        bad.events[0].message = crate::graph::pre_gate_commit_event(&second);
        assert!(prove_review_target(&bad, &second).await.is_err());

        // Wrong commit message directly on reviewed head.
        git_ok(&state.repo, &["reset", "--hard", &reviewed]).await;
        std::fs::write(state.repo.join("manual.txt"), "manual\n").unwrap();
        assert!(git::commit_all(&state.repo, "manual fix").await.unwrap());
        let manual = git::rev_parse(&state.repo, "HEAD").await.unwrap();
        let mut bad = state.clone();
        bad.pre_gate_commit = Some(manual.clone());
        bad.reviews[0].verified_head = Some(manual.clone());
        bad.events[0].message = crate::graph::pre_gate_commit_event(&manual);
        assert!(prove_review_target(&bad, &manual).await.is_err());

        // Non-neutral author directly on reviewed head with matching message.
        git_ok(&state.repo, &["reset", "--hard", &reviewed]).await;
        std::fs::write(state.repo.join("human.txt"), "human\n").unwrap();
        git_ok(&state.repo, &["add", "-A"]).await;
        let out = tokio::process::Command::new("git")
            .current_dir(&state.repo)
            .env("GIT_AUTHOR_NAME", "Human")
            .env("GIT_AUTHOR_EMAIL", "human@example.com")
            .env("GIT_COMMITTER_NAME", git::NEUTRAL_IDENTITY_NAME)
            .env("GIT_COMMITTER_EMAIL", git::NEUTRAL_IDENTITY_EMAIL)
            .args([
                "commit",
                "--no-verify",
                "-m",
                crate::graph::PRE_GATE_COMMIT_MESSAGE,
            ])
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
        let non_neutral = git::rev_parse(&state.repo, "HEAD").await.unwrap();
        let mut bad = state.clone();
        bad.pre_gate_commit = Some(non_neutral.clone());
        bad.reviews[0].verified_head = Some(non_neutral.clone());
        bad.events[0].message = crate::graph::pre_gate_commit_event(&non_neutral);
        assert!(prove_review_target(&bad, &non_neutral).await.is_err());

        // Multi-parent merge commit with neutral identity and matching message.
        git_ok(&state.repo, &["checkout", "-b", "side", &reviewed]).await;
        std::fs::write(state.repo.join("side.txt"), "side\n").unwrap();
        assert!(git::commit_all(&state.repo, "side commit").await.unwrap());
        let side = git::rev_parse(&state.repo, "HEAD").await.unwrap();
        git_ok(&state.repo, &["checkout", "main"]).await;
        git_ok(&state.repo, &["reset", "--hard", &reviewed]).await;
        git_ok(
            &state.repo,
            &[
                "-c",
                &format!("user.name={}", git::NEUTRAL_IDENTITY_NAME),
                "-c",
                &format!("user.email={}", git::NEUTRAL_IDENTITY_EMAIL),
                "merge",
                "--no-ff",
                "-m",
                crate::graph::PRE_GATE_COMMIT_MESSAGE,
                &side,
            ],
        )
        .await;
        let merged = git::rev_parse(&state.repo, "HEAD").await.unwrap();
        let mut bad = state.clone();
        bad.pre_gate_commit = Some(merged.clone());
        bad.reviews[0].verified_head = Some(merged.clone());
        bad.events[0].message = crate::graph::pre_gate_commit_event(&merged);
        assert!(prove_review_target(&bad, &merged).await.is_err());
    }
}
