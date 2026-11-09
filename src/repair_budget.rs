//! Explicit, head-bound repair grants. The task ledger and exact-run queue pin
//! are committed together; a driver imports the receipt only under its lease.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

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
    /// Exact recorded base tip, required for base-conflict repair.
    #[arg(long)]
    #[serde(default)]
    pub tip: Option<String>,
    /// Narrow owner-approved conflict resolution instruction, separate from review criteria.
    #[arg(long)]
    #[serde(default)]
    pub instruction: Option<String>,
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

fn contract(task: &Task, kind: Kind) -> Result<String> {
    let original = serde_json::to_string(&(
        &task.instruction,
        &task.sources,
        &task.overrides,
        &task.repo,
        &task.pr_recovery,
        &task.replacement,
    ))?;
    if kind == Kind::BaseConflict {
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

fn controls(home: &Path, task: &Task, state: &RunState, kind: Kind) -> Result<()> {
    ensure!(
        task.runs.last() == Some(&state.id)
            && state.origin.as_ref().and_then(|o| o.task.as_deref()) == Some(task.id.as_str()),
        "repair grant refused: run task ownership changed"
    );
    if kind == Kind::BaseConflict {
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
                || (kind == Kind::BaseConflict
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
        !questions.iter().any(|q| (q.run == state.id
            || q.routed_task.as_deref() == Some(&task.id))
            && q.status.open()),
        "repair grant refused: unresolved question or approval"
    );
    Ok(())
}

async fn head_and_public(state: &RunState, head: &str) -> Result<()> {
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
    ensure!(
        state.passes.last().map(|p| p.index) == Some(request.expect_pass),
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
            let stop = state
                .base_sync_recovery
                .stop
                .as_ref()
                .context("no recorded textual stop")?;
            ensure!(
                stop.kind == crate::base_sync_recovery::StopKind::Conflict
                    && stop.head == request.head
                    && request.tip.as_ref() == Some(&stop.tip)
                    && state.base_sync.as_ref().is_some_and(
                        |s| s.tip == stop.tip && s.conflict.as_ref() == Some(&stop.detail)
                    ),
                "base-conflict repair refused: textual stop, head or --tip changed"
            );
        }
        Kind::Gate => {
            ensure!(
                state.gate_fixes.len() >= state.config.graph.gate_fix_rounds,
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
                state.reviews.len() >= state.config.graph.review_rounds,
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
    let _outputs = output_leases(&state)?;
    ensure!(
        state.status.resumable(),
        "repair grant refused: run is final"
    );
    ensure!(
        !task
            .repair_grants
            .iter()
            .any(|g| g.request.run == state.id
                && !state.repair_grants.iter().any(|a| a.receipt == *g)),
        "repair grant refused: another grant is queued"
    );
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
    let receipt = Receipt {
        stop: (request.kind == Kind::BaseConflict)
            .then(|| state.base_sync_recovery.stop.clone())
            .flatten(),
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

fn output_leases(state: &RunState) -> Result<Vec<crate::retention::OutputLease>> {
    let mut paths: std::collections::BTreeSet<_> = state
        .candidates
        .iter()
        .filter(|c| c.worktree.exists())
        .map(|c| c.worktree.clone())
        .collect();
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
            task.operator_decided_at == active.receipt.decision
                && contract(&task, active.receipt.request.kind)? == active.receipt.contract,
            "repair grant held: task contract or owner decision changed after acceptance"
        );
        let _outputs = output_leases(disk)?;
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
        if state.repair_grants.iter().any(|g| g.receipt == *receipt) {
            continue;
        }
        controls(home, &task, disk, receipt.request.kind)?;
        ensure!(
            task.operator_decided_at == receipt.decision
                && contract(&task, receipt.request.kind)? == receipt.contract,
            "repair grant held: owner decision changed after acceptance"
        );
        evidence(disk, &receipt.request)?;
        conflict_target(disk, &receipt.request, receipt.stop.as_ref()).await?;
        let _outputs = output_leases(disk)?;
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
    task.runs.last() == Some(&state.id)
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
        task.operator_decided_at == receipt.decision
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
    if request.kind != Kind::BaseConflict {
        return Ok(());
    }
    if let Some(stop) = stop {
        ensure!(
            state.base_sync_recovery.stop.as_ref() == Some(stop),
            "base-conflict repair held: stopped synchronization changed"
        );
    }
    let tracking = format!(
        "refs/remotes/{}/{}",
        state.config.merge.remote, state.base_branch
    );
    ensure!(
        Some(git::rev_parse(&state.repo, &tracking).await?) == request.tip,
        "base-conflict repair held: recorded base tip moved; inspect the new target and authorize again"
    );
    if let Some(checkout) = &state.foreign_checkout {
        checkout.validate(&state.repo).await?;
    }
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
    if task.runs.last() == Some(&state.id)
        && !pending(task, state)
        && state.repair_grants.last().is_some_and(|g| {
            g.receipt.request.kind == Kind::BaseConflict
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
