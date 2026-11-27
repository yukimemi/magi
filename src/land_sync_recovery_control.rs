//! Two-phase owner authorization and exact-run queueing for landing base-sync recovery.
//!
//! Each request persists:
//! 1. Phase 1 (`prepared`): immutable request, stop, scope, and task snapshot before any state mutation.
//! 2. Phase 2a (`authorized`): unconsumed single-attempt authorization saved in `run.json`.
//! 3. Phase 2b (`applied`): atomic exact-run queue pin and durable request marker on the owning `Task`.
//!
//! Interrupted acceptances reconcile using the recorded snapshot without replaying or double-granting.
//! Uncertain or corrupt receipts refuse without inventing a second grant.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    git,
    land_sync_recovery::{
        Authorization, GRANT_ATTEMPTS, GhPrObserver, MAX_GRANTS, PrObserver, Scope, Stop,
    },
    queue::{OperatorResume, Queue, Task, TaskStatus},
    run::{Liveness, RunState},
};

/// Error returned when admission refused before writing any durable state or receipt.
#[derive(Debug, thiserror::Error)]
#[error("{0:#}")]
pub struct BeforeWrite(pub anyhow::Error);

fn before_write<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.is::<BeforeWrite>() {
            error
        } else {
            anyhow::Error::new(BeforeWrite(error))
        }
    })
}

/// Validated landing base-sync recovery request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Full retained run ID.
    pub run: String,
    /// Full 40-character HEAD SHA inspected on both the retained worktree and open PR.
    pub head: String,
    /// Actor namespace (`"cli"` or chat talk ID).
    pub actor: String,
    /// Durable idempotency key; reuse only with identical arguments after interruption.
    pub request: String,
}

/// Result of a landing base-sync recovery admission request.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Owning task record after admission or idempotent reconciliation.
    pub task: Task,
    /// `true` when this exact request had already been applied.
    pub repeated: bool,
    /// `true` when an interrupted prior acceptance was reconciled to completion.
    pub reconciled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Authorized,
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    task: serde_json::Value,
    stop: Stop,
    scope: Scope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    request: Request,
    phase: Phase,
    binding: Binding,
    queued: Task,
    created_at: Timestamp,
    #[serde(default)]
    applied_at: Option<Timestamp>,
}

fn save_receipt(path: &Path, receipt: &Receipt) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(receipt)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Receipt directory path for a given actor under `home`.
pub fn receipt_path(home: &Path, actor: &str, request: &str) -> PathBuf {
    home.join("land-sync-recovery-controls")
        .join(actor)
        .join(format!("{request}.json"))
}

/// Authorize one bounded landing base-sync recovery attempt using `gh pr view`.
pub async fn recover(queue: &Queue, home: &Path, request: Request) -> Result<Outcome> {
    recover_with(queue, home, request, &GhPrObserver).await
}

/// Authorize one bounded landing base-sync recovery attempt with an injectable PR observer.
pub async fn recover_with<O: PrObserver>(
    queue: &Queue,
    home: &Path,
    mut request: Request,
    observer: &O,
) -> Result<Outcome> {
    before_write((|| -> Result<()> {
        ensure!(
            crate::run::is_run_id(&request.run),
            "landing sync recovery needs a full run id"
        );
        ensure!(
            crate::ask::valid_asset_name(&request.actor)
                && crate::ask::valid_asset_name(&request.request),
            "recovery actor and request must be safe filenames of at most 64 bytes"
        );
        ensure!(
            request.head.len() == 40 && request.head.bytes().all(|c| c.is_ascii_hexdigit()),
            "landing sync recovery needs a full 40-character HEAD SHA"
        );
        Ok(())
    })())?;
    request.head = request.head.to_ascii_lowercase();

    let dir = home
        .join("land-sync-recovery-controls")
        .join(&request.actor);
    before_write(std::fs::create_dir_all(&dir).map_err(Into::into))?;
    let lock = before_write(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{}.lock", request.request)))
            .map_err(Into::into),
    )?;
    before_write(crate::pr_head::try_lock_file(&lock).map_err(Into::into))?;

    let path = dir.join(format!("{}.json", request.request));
    let key = format!("{}/{}", request.actor, request.request);

    let prior_receipt: Option<Receipt> = if path.exists() {
        let bytes = std::fs::read(&path).with_context(|| {
            format!(
                "uncertain landing sync recovery receipt: could not read {}; inspect before retrying",
                path.display()
            )
        })?;
        let parsed: Receipt = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "uncertain landing sync recovery receipt: corrupt JSON in {}; inspect before retrying",
                path.display()
            )
        })?;
        Some(parsed)
    } else {
        None
    };

    if let Some(prior) = &prior_receipt {
        if prior.request != request {
            return Err(anyhow::Error::new(BeforeWrite(anyhow!(
                "request ID already binds a different operation"
            ))));
        }
        if prior.phase == Phase::Applied {
            let current = queue
                .get(&prior.queued.id)
                .unwrap_or_else(|_| prior.queued.clone());
            return Ok(Outcome {
                task: current,
                repeated: true,
                reconciled: false,
            });
        }
        // Check whether Phase 2b already committed to the queue before a crash.
        let task = queue.get(&prior.queued.id).with_context(|| {
            format!(
                "uncertain landing sync recovery receipt: owning task {} unreadable",
                prior.queued.id
            )
        })?;
        if task.land_sync_recovery_requests.contains(&key) {
            let mut completed = prior.clone();
            completed.phase = Phase::Applied;
            completed.applied_at = Some(Timestamp::now());
            save_receipt(&path, &completed)?;
            return Ok(Outcome {
                task,
                repeated: true,
                reconciled: true,
            });
        }
    }

    let is_fresh = prior_receipt.is_none();
    let wrap_err = |err: anyhow::Error| -> anyhow::Error {
        if is_fresh {
            anyhow::Error::new(BeforeWrite(err))
        } else {
            err
        }
    };

    crate::task_reassign::ensure_current(queue, &request.run).map_err(&wrap_err)?;
    let mut owners = queue
        .list()
        .into_iter()
        .filter(|t| t.execution_runs().last() == Some(&request.run));
    let listed = owners
        .next()
        .context("run has no eligible latest task owner")
        .map_err(&wrap_err)?;
    if owners.next().is_some() {
        return Err(wrap_err(anyhow!("run has ambiguous task ownership")));
    }
    if let Some(prior) = &prior_receipt
        && prior.queued.id != listed.id
    {
        return Err(anyhow!(
            "uncertain landing sync recovery receipt: receipt targets task {} but current owner is {}",
            prior.queued.id,
            listed.id
        ));
    }

    let _claim = queue
        .claim(&listed.id)
        .context("task is claimed by a running daemon or another control")
        .map_err(&wrap_err)?;
    let task = queue.get(&listed.id).map_err(&wrap_err)?;
    if let Some(prior) = &prior_receipt
        && task.land_sync_recovery_requests.contains(&key)
    {
        let mut completed = prior.clone();
        completed.phase = Phase::Applied;
        completed.applied_at = Some(Timestamp::now());
        save_receipt(&path, &completed)?;
        return Ok(Outcome {
            task,
            repeated: true,
            reconciled: true,
        });
    }

    let _owner = crate::pr_head::owner_lock(home, &request.run).map_err(&wrap_err)?;
    let mut state = RunState::load_under(&request.run, home).map_err(&wrap_err)?;
    crate::pr_head::ensure_owner(&state).map_err(&wrap_err)?;

    let check_ownership = (|| -> Result<()> {
        ensure!(
            task.execution_runs().last().is_some_and(|r| r == &state.id)
                && task.assignment_accepts(&state)
                && !matches!(task.status, TaskStatus::Running | TaskStatus::Done)
                && !task.fresh_start
                && task.review_branch.is_none()
                && task.pr_recovery.is_none(),
            "run task ownership or queue status changed"
        );
        ensure!(
            state
                .origin
                .as_ref()
                .and_then(|o| o.task.as_deref())
                .is_none_or(|owner| owner == task.id),
            "run belongs to another task"
        );
        ensure!(
            crate::daemon::compatible_run_instruction(&state, &task),
            "task instruction does not match the retained run"
        );
        let claimed = crate::daemon::is_working_on_other(home, &state.id, Timestamp::now())?;
        ensure!(
            state.liveness_under(home, claimed) == Liveness::Dead,
            "run is live or its ownership is unknown"
        );
        Ok(())
    })();
    check_ownership.map_err(&wrap_err)?;
    let _execution = crate::driver::maintenance(home, &state.id).map_err(&wrap_err)?;

    let existing_auth = state
        .land_sync_recovery
        .authorizations
        .iter()
        .find(|a| a.request == key)
        .cloned();

    match (&prior_receipt, &existing_auth) {
        (None, Some(_)) => {
            return Err(anyhow!(
                "uncertain landing sync recovery state: authorization for request `{key}` already exists in run state without a receipt; inspect before retrying"
            ));
        }
        (Some(prior), None) if prior.phase == Phase::Authorized => {
            return Err(anyhow!(
                "uncertain landing sync recovery receipt: receipt recorded authorization phase but run state has no matching authorization for `{key}`; inspect before retrying"
            ));
        }
        (Some(_), Some(auth)) => {
            ensure!(
                !auth.consumed && auth.invalidated.is_none() && state.land_sync_recovery_pending(),
                "landing sync recovery request was already consumed or invalidated; it cannot queue again"
            );
        }
        _ => {}
    }

    let check_eligibility = (|| -> Result<()> {
        ensure!(
            state.status.resumable()
                && state.released_to.is_none()
                && state.retired_to.is_none()
                && state.pr_taken_over_by.is_none(),
            "run ownership is final"
        );
        ensure!(
            !state.base_sync_blocked(),
            "run has a pre-publication base-sync stop; use recover-base, not recover-land-sync"
        );
        ensure!(
            state.land_sync_exhausted(),
            "run has no eligible landing base-sync exhaustion stop"
        );
        ensure!(
            state.land_sync_grants_remaining() >= GRANT_ATTEMPTS,
            "lifetime landing base-sync recovery limit reached ({}/{MAX_GRANTS})",
            state.land_sync_recovery.extra_attempts
        );
        if existing_auth.is_none() {
            ensure!(
                !state.has_unconsumed_land_sync_authorization(),
                "this landing base-sync stop already has a pending authorization; resume the existing request"
            );
        }
        Ok(())
    })();
    check_eligibility.map_err(&wrap_err)?;

    let winner = state
        .winner()
        .cloned()
        .context("run has no retained winner")
        .map_err(&wrap_err)?;
    let clean = git::is_clean(&winner.worktree).await.map_err(&wrap_err)?;
    if !clean {
        return Err(wrap_err(anyhow!(
            "retained worktree has uncommitted changes"
        )));
    }
    let scratch = home.join("runs").join(&state.id).join("rebase");
    let in_progress = git::checked_rebase_in_progress(&scratch)
        .await
        .map_err(&wrap_err)?;
    if in_progress {
        return Err(wrap_err(anyhow!(
            "landing rebase is already in progress in scratch worktree"
        )));
    }
    let local_head = git::rev_parse(&winner.worktree, "HEAD")
        .await
        .map_err(&wrap_err)?;
    let branch_head = git::rev_parse(&state.repo, &format!("refs/heads/{}", winner.branch))
        .await
        .map_err(&wrap_err)?;
    if local_head != branch_head {
        return Err(wrap_err(anyhow!(
            "retained worktree does not match the winner branch"
        )));
    }
    if !local_head.eq_ignore_ascii_case(&request.head) {
        return Err(wrap_err(anyhow!(
            "retained HEAD changed since the owner request (expected {}, actual {})",
            request.head,
            local_head
        )));
    }
    let pushed_head = state
        .pr_pushed_head
        .as_deref()
        .context("run has no recorded published PR head")
        .map_err(&wrap_err)?;
    if !pushed_head.eq_ignore_ascii_case(&local_head) {
        return Err(wrap_err(anyhow!(
            "retained HEAD ({local_head}) does not match recorded published PR head ({pushed_head})"
        )));
    }
    let pr_record = state
        .pr
        .as_ref()
        .context("run has no recorded pull request")
        .map_err(&wrap_err)?;
    if pr_record.state != "open" {
        return Err(wrap_err(anyhow!("recorded pull request is not open")));
    }

    let observed_pr = observer
        .observe_pr(&state.repo, &pr_record.url)
        .await
        .map_err(&wrap_err)?;
    let check_pr = (|| -> Result<()> {
        ensure!(observed_pr.open, "pull request is no longer open");
        ensure!(
            observed_pr.url == pr_record.url && observed_pr.number == pr_record.number,
            "observed pull request does not match the run's recorded pull request"
        );
        ensure!(
            observed_pr.head.len() == 40 && observed_pr.head.bytes().all(|b| b.is_ascii_hexdigit()),
            "observed pull request head is unreadable"
        );
        ensure!(
            observed_pr.head.eq_ignore_ascii_case(&local_head),
            "published PR head ({}) does not match retained HEAD ({local_head})",
            observed_pr.head
        );
        if let Some(base) = &observed_pr.base_branch
            && !base.is_empty()
        {
            ensure!(
                base == &state.base_branch,
                "pull request base branch changed (expected {}, observed {base})",
                state.base_branch
            );
        }
        Ok(())
    })();
    check_pr.map_err(&wrap_err)?;

    let fetched = git::fetch(&state.repo, &state.config.merge.remote, &state.base_branch)
        .await
        .map_err(&wrap_err)?;
    if !fetched.ok() {
        return Err(wrap_err(anyhow!(
            "cannot authorize landing sync recovery without a fetched base: {}",
            fetched.stderr
        )));
    }
    let approved_tip = git::rev_parse(
        &state.repo,
        &format!("{}/{}", state.config.merge.remote, state.base_branch),
    )
    .await
    .map_err(&wrap_err)?;

    if let Some(stop) = &state.land_sync_recovery.stop {
        if !stop.head.eq_ignore_ascii_case(&local_head)
            || !stop.pr_head.eq_ignore_ascii_case(&observed_pr.head)
        {
            return Err(wrap_err(anyhow!(
                "retained or published HEAD differs from the recorded stop"
            )));
        }
        let descends = git::ancestry(&state.repo, &stop.tip, &approved_tip)
            .await
            .map_err(&wrap_err)?;
        if !descends {
            return Err(wrap_err(anyhow!(
                "fetched base tip {approved_tip} does not fast-forward from stopped base tip {}",
                stop.tip
            )));
        }
    } else {
        let prior_base = state
            .base_sync
            .as_ref()
            .map(|s| s.tip.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&state.base_commit);
        if !prior_base.is_empty() {
            let descends = git::ancestry(&state.repo, prior_base, &approved_tip)
                .await
                .map_err(&wrap_err)?;
            if !descends {
                return Err(wrap_err(anyhow!(
                    "fetched base tip {approved_tip} does not fast-forward from prior base commit {prior_base}"
                )));
            }
        }
        state.record_land_sync_exhausted_stop(&local_head, &observed_pr.head, &approved_tip);
    }

    let stop = state
        .land_sync_recovery
        .stop
        .clone()
        .context("landing sync stop missing after recording")
        .map_err(&wrap_err)?;
    let scope = Scope {
        tip: approved_tip.to_ascii_lowercase(),
        instruction: task.instruction.clone(),
        assignment: task.reassignments.len(),
        pr_head: observed_pr.head.to_ascii_lowercase(),
        pr_url: observed_pr.url.clone(),
        pr_number: observed_pr.number,
        grant_index: state.land_sync_recovery.extra_attempts,
    };
    let task_snapshot = serde_json::to_value(&task)
        .map_err(anyhow::Error::from)
        .map_err(&wrap_err)?;
    let binding = Binding {
        task: task_snapshot,
        stop: stop.clone(),
        scope: scope.clone(),
    };

    let reconciled = prior_receipt.is_some();
    let mut receipt = if let Some(prior) = prior_receipt {
        if let Some(auth) = &existing_auth {
            ensure!(
                auth.stop == prior.binding.stop && auth.scope == prior.binding.scope,
                "interrupted landing sync recovery authorization does not match receipt binding"
            );
            ensure!(
                prior.binding.task == binding.task,
                "interrupted landing sync recovery request is stale: owning task changed"
            );
            // Allow base tip fast-forward between interrupted authorization and queue completion,
            // while refusing rewritten history or changed stop/HEAD/PR/instruction/assignment.
            let descends =
                git::ancestry(&state.repo, &prior.binding.scope.tip, &binding.scope.tip).await?;
            ensure!(
                descends
                    && prior.binding.stop == binding.stop
                    && prior.binding.scope.instruction == binding.scope.instruction
                    && prior.binding.scope.assignment == binding.scope.assignment
                    && prior.binding.scope.pr_head == binding.scope.pr_head
                    && prior.binding.scope.pr_url == binding.scope.pr_url
                    && prior.binding.scope.pr_number == binding.scope.pr_number
                    && prior.binding.scope.grant_index == binding.scope.grant_index,
                "interrupted landing sync recovery request is stale: stop, HEAD, PR, instruction, or base lineage changed"
            );
        } else {
            ensure!(
                prior.binding == binding,
                "interrupted landing sync recovery request is stale: task, stop, HEAD, PR, or fetched base changed"
            );
        }
        prior
    } else {
        let mut queued = task.clone();
        if !crate::repair_budget::queue_resumption(queue, &mut queued, &state.id)
            .map_err(&wrap_err)?
        {
            queued.release_operator("land-sync-recovery", None);
        }
        queued.resume_override = Some(OperatorResume {
            question_id: String::new(),
            at: Timestamp::now(),
            conductor_rehold: None,
            forced: true,
            pinned_run: Some(state.id.clone()),
        });
        queued.land_sync_recovery_requests.push(key.clone());
        let prepared = Receipt {
            request: request.clone(),
            phase: Phase::Prepared,
            binding,
            queued,
            created_at: Timestamp::now(),
            applied_at: None,
        };
        save_receipt(&path, &prepared)?;
        prepared
    };

    // Phase 2a: persist the unconsumed authorization on RunState before queue commit.
    if existing_auth.is_none() {
        let auth = Authorization {
            request: key.clone(),
            task: task.id.clone(),
            run: state.id.clone(),
            stop,
            scope,
            decision: receipt.queued.operator_decided_at,
            generation: receipt.queued.resumption.as_ref().map(|r| r.generation),
            granted_attempts: GRANT_ATTEMPTS,
            at: Timestamp::now(),
            consumed: false,
            consumed_at: None,
            invalidated: None,
            archived_verification: None,
        };
        state.land_sync_recovery.authorizations.push(auth);
        state.event(
            "land-sync-recovery",
            format!(
                "owner authorized {GRANT_ATTEMPTS} bounded landing base-sync attempt ({}/{MAX_GRANTS} grants); fresh E2E and gate required before publication",
                state.land_sync_recovery.extra_attempts + GRANT_ATTEMPTS
            ),
        );
        state.save_under(home)?;
    }
    if receipt.phase == Phase::Prepared {
        receipt.phase = Phase::Authorized;
        save_receipt(&path, &receipt)?;
    }

    // Phase 2b: atomically commit the exact-run queue pin and durable request marker.
    queue.commit_land_sync_recovery(&mut receipt.queued, &receipt.binding.task)?;
    receipt.phase = Phase::Applied;
    receipt.applied_at = Some(Timestamp::now());
    save_receipt(&path, &receipt)?;

    Ok(Outcome {
        task: queue.get(&task.id)?,
        repeated: false,
        reconciled,
    })
}
