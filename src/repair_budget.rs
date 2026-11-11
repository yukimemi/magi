//! Explicit, head-bound repair grants. The task ledger and exact-run queue pin
//! are committed together; a driver imports the receipt only under its lease.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

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
fn owner_effects_settled(home: &Path, task: &Task, state: &RunState) -> Result<()> {
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

fn controls(home: &Path, task: &Task, state: &RunState, kind: Kind) -> Result<()> {
    ensure!(
        task.execution_runs().last() == Some(&state.id)
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
            && task.replacement.is_none(),
        "repair grant refused: task has a different continuation selected"
    );
    ensure!(
        !task.operator_held() && task.blocked_by.is_empty(),
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
        let (ok, body) = crate::land::gh(
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
        ensure!(ok, "repair grant refused: cannot inspect PR: {body}");
        let live: serde_json::Value = serde_json::from_str(&body)?;
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

fn evidence(state: &RunState, request: &Request) -> Result<()> {
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
                    || failed_conflict_repair_at(state, &request.head),
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
                state.reviews.len() >= state.config.graph.review_rounds
                    || failed_conflict_repair_at(state, &request.head),
                "repair grant refused: review budget is not exhausted"
            );
            ensure!(
                state.reviews.last().is_some_and(|r| r.head == request.head
                    && !r.clean
                    && r.fix.is_none()
                    && (r.blocking > 0 || r.e2e_status() == crate::run::E2eStatus::Failed)),
                "repair grant refused: no unfixed review failure at the expected head"
            );
        }
    }
    Ok(())
}

/// Atomically append authorization and queue this exact run. No run JSON is
/// changed here, so a crash cannot leave an unqueued budget mutation.
pub async fn grant(queue: &Queue, home: &Path, mut request: Request) -> Result<Receipt> {
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
    let _claim = queue
        .claim(&request.task)
        .context("repair grant refused: task queue lease is held")?;
    let mut task = queue.get(&request.task)?;
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
    let _owner = crate::pr_head::owner_lock(home, &request.run)?;
    let state = RunState::load_under(&request.run, home)?;
    controls(home, &task, &state, request.kind)?;
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
    evidence(&state, &request)?;
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
        request,
        at: jiff::Timestamp::now(),
        decision: task.operator_decided_at,
        contract: task_contract,
    };
    task.repair_grants.push(receipt.clone());
    task.status = TaskStatus::Queued;
    task.hold_source = None;
    task.hold_reason = None;
    task.held_at = None;
    task.resume_override = Some(crate::queue::OperatorResume {
        question_id: String::new(),
        at: receipt.at,
        conductor_rehold: None,
        forced: true,
        pinned_run: Some(state.id),
    });
    queue.put(&mut task)?;
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
    if pending(&task, disk)
        && task
            .repair_grants
            .last()
            .is_some_and(|r| r.request.kind == Kind::BaseConflict)
    {
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
        controls(home, &task, disk, active.receipt.request.kind)?;
        ensure!(
            reconcile::decision_matches(home, &task, &active.receipt)?
                && contract(&task, active.receipt.request.kind)? == active.receipt.contract,
            "repair grant held: task contract or owner decision changed after acceptance"
        );
        let _outputs = output_leases(home, disk)?;
        if matches!(active.phase, Phase::Pending | Phase::Fixing) {
            conflict_target(disk, &active.receipt.request, active.receipt.stop.as_ref()).await?;
        }
        if active.phase != Phase::Fixing {
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
        controls(home, &task, disk, receipt.request.kind)?;
        ensure!(
            reconcile::decision_matches(home, &task, receipt)?
                && contract(&task, receipt.request.kind)? == receipt.contract,
            "repair grant held: owner decision changed after acceptance"
        );
        evidence(disk, &receipt.request)?;
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
    if task.repair_grants.is_empty() {
        return Ok(false);
    }
    let home = queue.root().parent().context("queue has no home")?;
    let state = RunState::load_under(run, home)?;
    if !pending(task, &state) {
        return Ok(false);
    }
    let receipt = task.repair_grants.last().expect("pending receipt");
    controls(home, task, &state, receipt.request.kind)?;
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
    controls(&home, &task, state, request.kind)?;
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

fn failed_conflict_repair_at(state: &RunState, head: &str) -> bool {
    conflict_lineage(state)
        && state.repair_grants.last().is_some_and(|grant| {
            grant.phase == Phase::Complete && !grant.succeeded && grant.head == head
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
