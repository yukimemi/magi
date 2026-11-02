//! Direct task and run control for the standing chat: `magi task control ...`,
//! and a chat seat's `magi answer --reply` / `--reply-other`.
//!
//! A chat agent carries out the owner's requests by command, with no question
//! or offered choice in between: resume a run, hold or release a task, retry a
//! task with a changed instruction, rerun the failed checks of a pull request,
//! abandon a dead run, and record the owner's choice for a decision the
//! machinery waits on (a merge included). The owner's request in the chat is
//! the authority: no quote, fixed phrase or "only tasks this chat filed" scope
//! applies, including to direct PR merges. Every command goes
//! through the transitions the rest of magi already uses ([`crate::resume`],
//! [`Task::hold_manual`], [`Task::release_operator`], [`Task::edit`],
//! [`crate::daemon::rerun_checks_once`], [`crate::driver::abandon_proven_dead`],
//! [`crate::ask::Question::answer`]); this module only checks *who is asking*
//! and that the target is consistent. A chat seat's answers go through the
//! same bridge ([`Operation::Answer`], [`Operation::ReplyOther`]) because a
//! read-only turn's sandbox may refuse magi's stores; the launcher records
//! them with exactly the terminal's functions.
//!
//! - **Authorization.** `MAGI_NODE=chat` and `MAGI_RUN` equal to the exact id
//!   of an open conversation ([`crate::chat_watch::authorize`]).
//! - **Targets.** Any task (full or unambiguous-prefix id) and any recorded
//!   run (full run id). Ids are checked against the queue, and a task is
//!   re-read under its claim, never trusted from a lookup made before it.
//! - **Resume starts nothing here.** It pins the owning task to the run and
//!   queues it; the loop's ordinary start logic resumes it under the graph's
//!   execution lease, so a second driver cannot appear.
//! - **Abandon needs a proven-dead driver.** Liveness is judged by the shared
//!   judge under the owner lock; an unknown or live driver is refused.
//! - **A merge waits for the owner.** It is recorded only when the owner asks
//!   for it; the land driver then merges the saved head with
//!   `--match-head-commit`.
//! - **Direct PR merging is head-bound, not phrase-bound.** The chat reads the
//!   head from the forge and merges on the owner's plain request. Pending land
//!   approvals retain their existing decision path.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::queue::{Queue, Task, TaskStatus};
use crate::talk::{Talk, Talks};

#[cfg(any(unix, windows))]
pub(crate) mod bridge;

/// Only these operations cross the standing turn's inherited capability.
/// No shell commands or calling conversation identities are input.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
pub enum Operation {
    /// Reassign an idle task to a different repository without migrating runs.
    Reassign {
        /// Task id or unambiguous prefix.
        task: String,
        /// Repository path, owner/repo, or unique short name.
        repo: std::path::PathBuf,
    },
    /// Authorize bounded repair through the same transaction as the terminal.
    GrantRepair {
        /// Head-bound operator request.
        request: crate::repair_budget::Request,
    },
    /// Reconcile a task-local uncertain effect against its durable end condition.
    ReconcileDecision {
        /// Stable inquiry id.
        inquiry: String,
    },
    /// Bind a command to its durable inquiry; acceptance is not resolution.
    ForDecision {
        /// Stable inquiry id.
        inquiry: String,
        /// Existing typed task/run command.
        command: Box<Operation>,
    },
    /// Explicitly confirm no effect remains, including accepted legacy choices.
    SettleDecision {
        /// Stable inquiry id.
        inquiry: String,
        /// Owner's reason for settlement.
        reason: String,
        /// Replacement active inquiry in the same task/chat.
        replacement: Option<String>,
    },
    /// File a new task through the launcher, including from read-only chats.
    Add {
        /// Validated filing inputs, without task state or a caller-selected id.
        filing: Box<Filing>,
    },
    /// Mark a task complete using the terminal's transition.
    Done {
        /// Task id or unambiguous prefix.
        task: String,
    },
    /// Change a task's queue priority.
    Priority {
        /// Task id or unambiguous prefix.
        task: String,
        /// New priority.
        priority: i32,
    },
    /// Merge a PR the owner asked for, once per request.
    MergePr {
        /// Exact owner/repository name.
        repo: String,
        /// Pull request number.
        number: u64,
        /// Full head SHA the chat just read from the forge.
        head: String,
        /// Idempotency key minted by the chat agent.
        request: String,
    },
    /// 既存 run の再開を queue に登録する。
    Resume {
        /// 完全な run ID。
        run: String,
    },
    /// task を手動保留する。
    Hold {
        /// task ID または一意な接頭辞。
        task: String,
        /// 保留理由。
        reason: String,
    },
    /// task を保留から解放する。
    Release {
        /// task ID または一意な接頭辞。
        task: String,
    },
    /// 指示を変更して再試行する。
    Retry {
        /// task ID または一意な接頭辞。
        task: String,
        /// 新しいタイトル、省略時は指示から生成する。
        title: Option<String>,
        /// 新しい指示全文。
        instruction: String,
    },
    /// PR の失敗した workflow を再実行する。
    RerunChecks {
        /// 完全な run ID。
        run: String,
        /// 冪等性を保証するリクエスト ID。
        request: String,
        /// 観測した PR head。
        head: String,
        /// 再実行対象の workflow run ID。
        workflows: Vec<String>,
    },
    /// タスクチャットで所有者の判断を記録する（マージを含む）。
    Decide {
        /// 質問 ID（完全または一意な接頭辞）。
        question: String,
        /// 記録された選択肢のラベル。
        choice: String,
        /// 「use my text」で公開する置換タイトルそのもの。
        #[serde(default)]
        title: Option<String>,
    },
    /// `magi answer --reply`: the owner's answer to a question, as an ordinary
    /// answer ([`crate::ask::record_answer`]).
    Answer {
        /// Full question id, resolved by the CLI.
        question: String,
        /// An offered choice (label or alias), or the free-text answer.
        reply: String,
        /// The literal replacement title for the posting gate's `use my text`.
        #[serde(default)]
        title: Option<String>,
    },
    /// `magi answer --reply-other`: the owner's decision outside the offered
    /// choices, saved as a note that ends the consultation
    /// ([`crate::consult::reply_other`]).
    ReplyOther {
        /// Full question id, resolved by the CLI.
        question: String,
        /// The owner's outcome.
        text: String,
    },
    /// 保存された計画を作成、または適用する。
    ReconcileRetention {
        /// 対象タスク ID。
        task: String,
        /// 保護する実装の完全な run ID。
        authority: String,
        /// 適用する保存済み計画 ID（省略時は計画のみ）。
        apply: Option<String>,
    },
    /// 生存が否定された run を破棄する。
    Abandon {
        /// 完全な run ID。
        run: String,
    },
}

/// Inputs prepared by `magi task add` before saving a fresh queue record.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filing {
    /// Task title.
    pub title: String,
    /// Complete task instruction.
    pub instruction: String,
    /// Validated absolute repository path.
    pub repo: std::path::PathBuf,
    /// Filing provenance prepared by the CLI.
    pub source: crate::queue::Source,
    /// Resolved implementation imports.
    pub sources: crate::refs::Sources,
    /// Optional initial hold reason.
    pub hold: Option<String>,
    /// Queue priority.
    pub priority: i32,
    /// Use one implementer.
    pub solo: bool,
    /// Prioritize execution.
    pub urgent: bool,
    /// Optional implementer count.
    pub implementers: Option<usize>,
    /// Absolute attachment paths to copy into the queue.
    pub attach: Vec<std::path::PathBuf>,
}

/// Whether this process inherited a launcher control channel.
/// Provenance-only CLI calls without a channel retain their ordinary local route.
pub fn has_channel() -> bool {
    #[cfg(any(unix, windows))]
    {
        std::env::var_os(bridge::CHANNEL_ENV).is_some()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// The CLI sends only a typed operation when it inherited a turn capability.
/// Outside a launched turn the existing, authorized local route remains usable.
pub async fn dispatch(
    queue: &Queue,
    home: &Path,
    talks: &Talks,
    talk: &str,
    operation: Operation,
) -> Result<String> {
    if matches!(&operation, Operation::Reassign { .. }) {
        #[cfg(unix)]
        {
            return bridge::request(&operation).context("repository reassignment requires the authenticated chat control channel; restart this chat turn through the launcher");
        }
        #[cfg(windows)]
        {
            return bridge::request(&operation).await.context("repository reassignment requires the authenticated chat control channel; restart this chat turn through the launcher");
        }
        #[cfg(not(any(unix, windows)))]
        {
            bail!("authenticated chat control channel is unavailable on this platform");
        }
    }
    #[cfg(unix)]
    if std::env::var_os(bridge::CHANNEL_ENV).is_some() {
        // The launcher sets this flag for writable seats, including effective
        // unsandboxed chat turns. execute() still enforces talk authorization.
        match bridge::request(&operation) {
            Err(error)
                if error.is::<bridge::Unavailable>()
                    && std::env::var("MAGI_ALLOW_WRITE").as_deref() == Ok("1") => {}
            result => return result,
        }
    }
    #[cfg(windows)]
    if std::env::var_os(bridge::CHANNEL_ENV).is_some() {
        return bridge::request(&operation).await;
    }
    execute(queue, home, talks, talk, operation).await
}

async fn execute(
    queue: &Queue,
    home: &Path,
    talks: &Talks,
    talk: &str,
    operation: Operation,
) -> Result<String> {
    authorize(talks, talk, crate::queue::CHAT_NODE)?;
    Ok(match operation {
        Operation::Reassign { task, repo } => {
            let cwd = &talks.get(talk)?.repo;
            let task = crate::task_reassign::reassign(
                queue,
                home,
                cwd,
                &task,
                &repo,
                &format!("chat:{talk}"),
            )
            .await?;
            let mut message = describe(&task, "reassigned");
            for note in crate::task_reassign::advisory_notes(&task, home) {
                message.push_str(&format!("\n  {note}"));
            }
            message
        }
        Operation::ReconcileDecision { inquiry } => {
            reconcile_task_decision(queue, home, talk, &inquiry)?;
            format!("decision {inquiry} reconciled from the task's durable state")
        }
        Operation::ForDecision { inquiry, command } => {
            execute_decision(queue, home, talks, talk, &inquiry, *command).await?
        }
        Operation::SettleDecision {
            inquiry,
            reason,
            replacement,
        } => {
            let outbox = crate::task_inquiry::Outbox::at(home);
            let record = outbox.get(&inquiry)?;
            // Task-backed legacy consumers run under the task claim. Hold it
            // through settlement; ownerless release consumers use the inquiry
            // execution fence inside Outbox::settle instead.
            let _task_claim = if !record.task.is_empty()
                && record
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.request.is_some())
            {
                Some(
                    queue
                        .claim(&record.task)
                        .context("decision task is executing")?,
                )
            } else {
                None
            };
            if record
                .decision
                .as_ref()
                .is_some_and(|d| d.request.is_none())
            {
                let questions = crate::ask::Questions::at(home.join("questions"));
                if let Ok(source) = questions.get(&inquiry)
                    && !(record.task.is_empty() && record.delivered_to.is_none())
                {
                    ensure!(
                        !crate::task_chat::awaits_decision(&source),
                        "a legacy effect still awaits consumer reconciliation; settlement executes no action"
                    );
                }
            }
            outbox.settle(&inquiry, talk, &reason, replacement.as_deref())?;
            format!("settled decision {inquiry}: {reason}")
        }
        Operation::Add { filing } => {
            let mut task = Task::new(filing.title, filing.instruction, filing.repo, filing.source);
            task.sources = filing.sources;
            if let Some(reason) = filing.hold {
                task.hold_manual(Some(reason));
            }
            task.priority = filing.priority;
            task.solo = filing.solo;
            task.urgent = filing.urgent;
            if filing.implementers.is_some() {
                task.overrides = Some(crate::queue::RunOverrides {
                    candidates: filing.implementers,
                    ..Default::default()
                });
            }
            if filing.attach.is_empty() {
                queue.put(&mut task)?;
            } else {
                queue.attach_and_put(&mut task, &filing.attach)?;
            }
            serde_json::to_string(&task)?
        }
        Operation::Done { task } => {
            let t = done(queue, home, &task)?;
            format!("done {} {}", t.short(), t.title)
        }
        Operation::Priority { task, priority } => {
            let t = set_priority(queue, &task, priority)?;
            format!("{} priority now {} - {}", t.short(), t.priority, t.title)
        }
        Operation::MergePr {
            repo,
            number,
            head,
            request,
        } => crate::chat_merge::merge(
            home,
            talks,
            talk,
            crate::chat_merge::Request {
                repo,
                number,
                head,
                request,
            },
        )
        .await
        .map_err(|error| {
            if error.is::<crate::chat_merge::BeforeMergeWrite>() {
                anyhow::Error::new(BeforeEffect(error))
            } else {
                error
            }
        })?,
        Operation::GrantRepair { request } => {
            // Keep run-state admission out of the shared control dispatcher
            // future, which is also embedded in ordinary CLI task operations.
            serde_json::to_string(
                &Box::pin(crate::repair_budget::grant(queue, home, request)).await?,
            )?
        }
        Operation::Resume { run } => {
            let t = resume(queue, home, &run)?;
            format!(
                "{}; the loop (`magi serve`) resumes the run under its execution lease",
                describe(&t, "resume queued for")
            )
        }
        Operation::Hold { task, reason } => describe(&hold(queue, &task, &reason)?, "held"),
        Operation::Release { task } => describe(&release(queue, &task)?, "released"),
        Operation::Retry {
            task,
            title,
            instruction,
        } => describe(
            &retry(queue, &task, title, &instruction)?,
            "retry queued for",
        ),
        Operation::RerunChecks {
            run,
            request,
            head,
            workflows,
        } => {
            let ids = rerun_checks(queue, home, talk, &run, &request, &head, &workflows).await?;
            format!("requested a rerun of workflow run(s) {}", ids.join(", "))
        }
        Operation::Decide {
            question,
            choice,
            title,
        } => {
            let q = record_decision(home, &question, &choice, title.as_deref())?;
            format!("recorded `{choice}` for decision {}", q.short())
        }
        Operation::Answer {
            question,
            reply,
            title,
        } => crate::ask::record_answer(
            &crate::ask::Questions::at(home.join("questions")),
            &question,
            &reply,
            title.as_deref(),
        )?,
        Operation::ReplyOther { question, text } => crate::consult::reply_other(
            &crate::ask::Questions::at(home.join("questions")),
            &question,
            &text,
        )?,
        Operation::ReconcileRetention {
            task,
            authority,
            apply,
        } => {
            let path = Box::pin(crate::retention::reconcile::run_chat(
                home,
                &task,
                &authority,
                apply.as_deref(),
                crate::retention::reconcile::ChatRequest {
                    talks,
                    conversation: talk,
                },
            ))
            .await?;
            if apply.is_none() {
                // Return the complete saved evidence through the capability bridge.
                crate::retention::reconcile::chat_summary(&path)?
            } else {
                format!(
                    "Applied saved plan {}",
                    apply.as_deref().unwrap_or_default()
                )
            }
        }
        Operation::Abandon { run } => {
            abandon(queue, home, &run)?;
            format!("abandoned run {run}")
        }
    })
}

/// Run one typed operation on behalf of the owner, retaining failures and
/// uncertainty. A repeated command never blindly replays a possibly applied
/// external effect. The underlying operation retains its existing claims and
/// receipts; this journal is its user-visible lifecycle, not a second worker.
async fn execute_decision(
    queue: &Queue,
    home: &Path,
    talks: &Talks,
    chat: &str,
    id: &str,
    command: Operation,
) -> Result<String> {
    use crate::task_decision::Status;
    let outbox = crate::task_inquiry::Outbox::at(home);
    let _execution = outbox.execution_claim(id)?;
    let inquiry = outbox.get(id)?;
    ensure!(
        inquiry.delivered_to.as_deref() == Some(chat),
        "decision belongs to another chat"
    );
    let task = queue.get(&inquiry.task)?;
    ensure!(task.repo == inquiry.repo, "decision repository changed");
    let deferred = matches!(
        command,
        Operation::Resume { .. } | Operation::MergePr { .. }
    );
    let merge = match &command {
        Operation::MergePr {
            repo,
            number,
            head,
            request,
        } => Some(crate::chat_merge::Request {
            repo: repo.clone(),
            number: *number,
            head: head.clone(),
            request: request.clone(),
        }),
        _ => None,
    };
    match &command {
        Operation::Done { task: target }
        | Operation::Hold { task: target, .. }
        | Operation::Release { task: target }
        | Operation::Retry { task: target, .. } => {
            ensure!(
                queue.resolve_id(target)? == task.id,
                "command targets another task"
            );
        }
        Operation::Resume { run }
        | Operation::Abandon { run }
        | Operation::RerunChecks { run, .. } => {
            ensure!(
                task.runs.contains(run) && inquiry.run.as_ref().is_none_or(|own| own == run),
                "command targets another execution"
            );
            ensure!(
                queue
                    .list()
                    .iter()
                    .filter(|owner| owner.runs.contains(run))
                    .count()
                    == 1,
                "execution ownership is ambiguous"
            );
        }
        Operation::MergePr { repo, number, .. } => {
            let run = inquiry
                .run
                .as_ref()
                .or_else(|| task.runs.last())
                .context("task has no execution for this merge")?;
            let state = crate::run::RunState::load_under(run, home)?;
            let expected = format!("https://github.com/{repo}/pull/{number}");
            ensure!(
                state.repo == task.repo
                    && state
                        .pr
                        .as_ref()
                        .is_some_and(|pr| pr.url.eq_ignore_ascii_case(&expected)),
                "merge targets another task or repository"
            );
        }
        _ => bail!("use a task/run effect command with --decision; routed approvals use decide"),
    }
    let request = serde_json::to_string(&command)?;
    let decision = inquiry.decision.context("inquiry is informational")?;
    if decision.status == Status::Resolved {
        ensure!(
            decision.request.as_deref() == Some(&request),
            "decision resolved for a different request"
        );
        return Ok(format!(
            "decision {id} is already resolved; inspect its outcome history"
        ));
    }
    if let Operation::RerunChecks { run, request, .. } = &command {
        // Reject correctable input before binding the decision to this exact
        // command. Execution revalidates these reads before taking its lock.
        rerun_preflight(queue, home, run, request)?;
    }
    if let Some(action) = &merge {
        // Malformed input must not durably bind this decision to a command
        // that can never reach its effect service.
        action.validate()?;
    }
    // Keep admission across acceptance and execution: contention and request
    // collisions must not bind a fresh decision to an unstartable operation.
    let merge_admission = merge
        .as_ref()
        .map(|action| crate::chat_merge::admit(home, talks, chat, action))
        .transpose()?;
    if decision.request.is_none() && decision.status == Status::RetryNeeded {
        if let Ok(source) = crate::ask::Questions::at(home.join("questions")).get(id) {
            ensure!(
                !crate::task_chat::awaits_decision(&source),
                "a legacy effect still needs its receipt reconciled before another effect is accepted"
            );
        }
    }
    outbox.update_decision(id, |d| d.accept(&request))?;
    if decision.request.is_some()
        && matches!(decision.status, Status::InProgress | Status::RetryNeeded)
    {
        if decision.retry_safe {
            outbox.update_decision(id, |d| {
                d.retry_unapplied()?;
                d.start()
            })?;
        } else {
            let action = merge
                .as_ref()
                .context("previous effect needs receipt reconciliation; it will not be replayed")?;
            ensure!(
                merge_receipt(home, chat, action)?.is_some(),
                "previous merge has no receipt; inspect the operation before retrying"
            );
        }
        // The shared merge service only checks the forge once this exact
        // request has a receipt; it never issues the merge command twice.
    } else {
        outbox.update_decision(id, |d| d.start())?;
    }
    let local = apply_local_decision(
        queue,
        home,
        id,
        &request,
        &inquiry.task,
        &inquiry.repo,
        &command,
    );
    let result = if let Err(error) = local {
        Err(error)
    } else if let Some(message) = local? {
        Ok(message)
    } else if let Operation::Resume { run } = &command {
        // Validation precedes any write: a refusal means nothing began.
        crate::resume::validate(home, run)
            .map(|_| ())
            .map_err(|e| anyhow::Error::new(BeforeEffect(e)))
            .and_then(|()| -> Result<String> {
                let pinned =
                    crate::resume::pin_queued(queue, run, None, Some(id)).map_err(|error| {
                        if error.is::<crate::resume::BeforeQueueWrite>() {
                            anyhow::Error::new(BeforeEffect(error))
                        } else {
                            error
                        }
                    })?;
                if !pinned {
                    return Err(anyhow::Error::new(BeforeEffect(anyhow::anyhow!(
                        "no queued task owns this run"
                    ))));
                }
                Ok(format!(
                    "resume queued for {run}; execution start remains unconfirmed"
                ))
            })
    } else if let Operation::Abandon { run } = &command {
        crate::driver::abandon_decision(home, run, id, &request)
            .map_err(|error| {
                if error.is::<crate::driver::BeforeAbandonWrite>() {
                    anyhow::Error::new(BeforeEffect(error))
                } else {
                    error
                }
            })
            .map(|()| format!("abandoned run {run}"))
    } else if let Some(admission) = merge_admission {
        crate::chat_merge::merge_admitted(home, merge.as_ref().unwrap().clone(), admission)
            .await
            .map_err(|error| {
                if error.is::<crate::chat_merge::BeforeMergeWrite>() {
                    anyhow::Error::new(BeforeEffect(error))
                } else {
                    error
                }
            })
    } else {
        Box::pin(execute(queue, home, talks, chat, command)).await
    };
    match result {
        Ok(message) => {
            if let Some(action) = &merge {
                if let Some(receipt) = merge_receipt(home, chat, action)? {
                    if receipt["status"] == "merged" {
                        outbox.update_decision(id, |d| d.applied(&message))?;
                    } else if receipt["status"] != "pending" {
                        outbox.update_decision(id, |d| {
                            d.fail(&format!(
                                "{message}; reuse this same request ID to reconcile only"
                            ))
                        })?;
                    }
                }
            } else if !deferred {
                outbox.update_decision(id, |d| {
                    d.applied(&format!("typed command completed: {message}"))
                })?;
            }
            if deferred
                && outbox
                    .get(id)?
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.status == Status::InProgress)
            {
                outbox.update_decision(id, |d| d.queued())?;
            }
            // Resume has only been queued. Its execution is confirmed by a
            // later scheduler projection, never by this command's return.
            Ok(message)
        }
        Err(error) => {
            let message = if error.is::<BeforeEffect>() {
                format!(
                    "{error:#}. No effect began; retry this same request after resolving the cause, or explicitly settle it."
                )
            } else {
                format!(
                    "Operation failed or has an uncertain result: {error:#}. Inspect its receipt before retrying."
                )
            };
            outbox.update_decision(id, |d| {
                if error.is::<BeforeEffect>() {
                    d.fail_before_effect(&message)
                } else {
                    d.fail(&message)
                }
            })?;
            Err(error)
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("effect did not start: {0:#}")]
struct BeforeEffect(anyhow::Error);

/// Commit a task-local effect and its exact receipt under one queue claim.
/// The inquiry projection may lag this write, but restart never has to infer
/// completion from the task's current status or repeat the operation.
fn apply_local_decision(
    queue: &Queue,
    home: &Path,
    id: &str,
    request: &str,
    owner: &str,
    repo: &Path,
    command: &Operation,
) -> Result<Option<String>> {
    let target = match command {
        Operation::Done { task }
        | Operation::Hold { task, .. }
        | Operation::Release { task }
        | Operation::Retry { task, .. } => task,
        _ => return Ok(None),
    };
    let prepare = || -> Result<_> {
        let (_claim, mut task) = claimed(queue, target)?;
        ensure!(
            task.id == owner && task.repo == repo,
            "decision ownership changed"
        );
        if let Some(prior) = task.decision_effects.get(id) {
            ensure!(prior == request, "decision receipt binds another command");
            let message = format!("task {}: decision {id} already applied", task.id);
            return Ok((_claim, task, message, true));
        }
        let message = match command {
            Operation::Done { .. } => {
                task.succeed();
                describe(&task, "done")
            }
            Operation::Hold { reason, .. } => {
                check_hold_release(&task, "hold")?;
                let reason = reason.trim();
                task.hold_manual((!reason.is_empty()).then(|| reason.to_owned()));
                describe(&task, "held")
            }
            Operation::Release { .. } => {
                check_hold_release(&task, "release")?;
                task.release_operator("chat", Some(id));
                describe(&task, "released")
            }
            Operation::Retry {
                title, instruction, ..
            } => {
                ensure!(
                    !instruction.trim().is_empty(),
                    "the new instruction is empty"
                );
                let title = title
                    .clone()
                    .unwrap_or_else(|| crate::queue::title_from(instruction, 72));
                task.edit(title, instruction.clone())?;
                task.release_operator("chat", Some(id));
                describe(&task, "retry queued for")
            }
            _ => unreachable!("task-local operation checked above"),
        };
        task.decision_effects
            .insert(id.to_owned(), request.to_owned());
        Ok((_claim, task, message, false))
    };
    let (_claim, mut task, message, already_applied) = prepare().map_err(BeforeEffect)?;
    if !already_applied {
        queue.put(&mut task)?;
    }
    if matches!(command, Operation::Done { .. }) {
        crate::daemon::supersede_prior_runs(&task, home);
    }
    Ok(Some(message))
}

/// Explicit owner-requested reconciliation executes nothing. Under the task
/// claim, prove that the exact intended condition already holds; otherwise keep
/// uncertainty visible. External effects require their own receipts instead.
fn reconcile_task_decision(queue: &Queue, home: &Path, chat: &str, id: &str) -> Result<()> {
    let outbox = crate::task_inquiry::Outbox::at(home);
    let inquiry = outbox.get(id)?;
    ensure!(
        inquiry.delivered_to.as_deref() == Some(chat),
        "decision belongs to another chat"
    );
    let decision = inquiry.decision.context("inquiry is informational")?;
    let request = decision
        .request
        .context("decision has no accepted operation to reconcile")?;
    if request.starts_with("decision: ") {
        let source = crate::ask::Questions::at(home.join("questions")).read_history(id)?;
        crate::task_chat::sync_effect_receipt(home, queue, &source);
        ensure!(
            outbox
                .get(id)?
                .decision
                .is_some_and(|d| d.status == crate::task_decision::Status::Resolved),
            "source effect remains unconfirmed; no operation was replayed"
        );
        return Ok(());
    }
    let command: Operation = serde_json::from_str(&request)?;
    if let Operation::Resume { run } = &command {
        reconcile_resume_decision(queue, home, &outbox, id, run)?;
        ensure!(
            outbox.get(id)?.decision.is_some_and(|d| {
                d.status == crate::task_decision::Status::Resolved || d.retry_safe
            }),
            "resume remains queued or executing; no operation was replayed"
        );
        return Ok(());
    }
    let (_claim, task) = claimed(queue, &inquiry.task)?;
    ensure!(task.repo == inquiry.repo, "decision repository changed");
    if let Some(receipt) = task.decision_effects.get(id) {
        ensure!(
            receipt == &request,
            "decision receipt binds another command"
        );
        return outbox.update_decision(id, |d| {
            d.applied("exact task command receipt confirmed; no operation replayed")
        });
    }
    let matches = match command {
        Operation::Hold {
            task: target,
            reason,
        } => {
            queue.resolve_id(&target)? == task.id
                && task.status == TaskStatus::Held
                && task.hold_reason.as_deref() == Some(reason.trim())
        }
        Operation::Release { task: target } => {
            queue.resolve_id(&target)? == task.id
                && task.status == TaskStatus::Queued
                && !task.operator_held()
        }
        Operation::Done { task: target } => {
            queue.resolve_id(&target)? == task.id && task.status == TaskStatus::Done
        }
        Operation::Retry {
            task: target,
            title,
            instruction,
        } => {
            queue.resolve_id(&target)? == task.id
                && task.instruction == instruction
                && title.as_ref().is_none_or(|title| *title == task.title)
                && task.status == TaskStatus::Queued
        }
        Operation::Abandon { run } => abandonment_receipt(home, &task, id, &run, &request)?,
        _ => anyhow::bail!(
            "external effects require their authoritative receipt; no operation was replayed"
        ),
    };
    ensure!(
        matches,
        "intended task condition is not confirmed; no operation was replayed"
    );
    outbox.update_decision(id, |d| d.applied("owner-requested reconciliation: exact intended task condition confirmed under its claim; no effect remains necessary"))
}

/// Read the exact request's committed execution pass or durable pin supersession.
/// A transient task status alone never proves either outcome.
pub(crate) fn resume_started(home: &Path, task: &Task, inquiry: &str, run: &str) -> Result<bool> {
    ensure!(
        crate::run::is_run_id(run) && task.runs.iter().any(|id| id == run),
        "resume run ownership changed"
    );
    let state = crate::run::RunState::load_under(run, home)?;
    ensure!(state.repo == task.repo, "resume run repository changed");
    if task
        .superseded_resumes
        .get(inquiry)
        .is_some_and(|target| target == run)
    {
        return Ok(true);
    }
    Ok(state.decision_starts.get(inquiry).is_some_and(|index| {
        state
            .passes
            .get(*index)
            .is_some_and(|pass| pass.index == *index)
    }))
}

/// Publication writes the pin and actions_applied marker atomically. Fence the
/// publisher and scheduler before treating their absence as pre-effect proof.
/// A published pin may already be consumed, so its absence alone is not proof.
fn reconcile_resume_decision(
    queue: &Queue,
    home: &Path,
    outbox: &crate::task_inquiry::Outbox,
    id: &str,
    run: &str,
) -> Result<()> {
    let Ok(_execution) = outbox.execution_claim(id) else {
        return Ok(());
    };
    let inquiry = outbox.get(id)?;
    let Ok(_claim) = queue.claim(&inquiry.task) else {
        return Ok(());
    };
    let task = queue.get(&inquiry.task)?;
    ensure!(task.repo == inquiry.repo, "resume task repository changed");
    let decision = inquiry.decision.context("inquiry is informational")?;
    if decision.status == crate::task_decision::Status::Resolved {
        return Ok(());
    }
    if resume_started(home, &task, id, run)? {
        outbox.update_decision(id, |d| {
            d.applied("exact resume execution-start or supersession receipt confirmed")
        })?;
    } else if !task.action_applied(id)
        && !task.dispatched_resumes.contains_key(id)
        && !task.superseded_resumes.contains_key(id)
        && !task
            .resume_override
            .as_ref()
            .is_some_and(|pin| pin.question_id == id)
    {
        outbox.update_decision(id, |d| {
            d.fail_before_effect("Resume was not published to the queue; the owner may retry the same request or cancel it. No operation was replayed.")
        })?;
    }
    Ok(())
}

fn abandonment_receipt(
    home: &Path,
    task: &Task,
    id: &str,
    run: &str,
    request: &str,
) -> Result<bool> {
    ensure!(
        task.runs.iter().any(|own| own == run),
        "abandon run ownership changed"
    );
    let state = crate::run::RunState::load_under(run, home)?;
    ensure!(state.repo == task.repo, "abandon run repository changed");
    Ok(state
        .abandonment_receipts
        .get(id)
        .is_some_and(|receipt| receipt == request))
}

fn merge_receipt(
    home: &Path,
    chat: &str,
    action: &crate::chat_merge::Request,
) -> Result<Option<serde_json::Value>> {
    ensure!(
        crate::ask::valid_asset_name(chat) && crate::ask::valid_asset_name(&action.request),
        "invalid merge receipt identity"
    );
    let path = home
        .join("chat-controls")
        .join(chat)
        .join(format!("{}.json", action.request));
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let receipt: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt["action"] == serde_json::to_value(action)?,
        "merge receipt names another operation"
    );
    Ok(Some(receipt))
}

/// Repair the decision projection from independently durable command receipts.
/// This starts no workers and retries no operation.
pub fn reconcile_decisions(queue: &Queue, home: &Path) -> Result<()> {
    let outbox = crate::task_inquiry::Outbox::at(home);
    let (inquiries, unreadable) = outbox.list_readable()?;
    let mut errors = Vec::new();
    if unreadable != 0 {
        errors.push(format!("{unreadable} unreadable inquiry record(s)"));
    }
    for inquiry in inquiries {
        let id = inquiry.id.clone();
        if let Err(error) = reconcile_inquiry(queue, home, &outbox, inquiry) {
            let message = format!(
                "Decision receipt reconciliation failed: {error:#}. Inspect this inquiry's receipt before retrying."
            );
            tracing::warn!("decision {id}: {message}");
            if let Err(error) = outbox.update_decision(&id, |d| d.fail(&message)) {
                tracing::warn!("decision {id} error could not be persisted: {error:#}");
            }
            errors.push(format!("{id}: {message}"));
        }
    }
    ensure!(
        errors.is_empty(),
        "{} decision reconciliation problem(s): {}",
        errors.len(),
        errors.join("; ")
    );
    Ok(())
}

fn reconcile_inquiry(
    queue: &Queue,
    home: &Path,
    outbox: &crate::task_inquiry::Outbox,
    inquiry: crate::task_inquiry::Inquiry,
) -> Result<()> {
    use crate::task_decision::Status;
    let Some(decision) = inquiry.decision else {
        return Ok(());
    };
    if decision.status == Status::Resolved {
        return Ok(());
    }
    let Some(request) = decision.request else {
        return Ok(());
    };
    let Ok(command) = serde_json::from_str::<Operation>(&request) else {
        return Ok(());
    };
    if let Operation::Resume { run } = &command {
        return reconcile_resume_decision(queue, home, outbox, &inquiry.id, run);
    }
    let task = queue.get(&inquiry.task)?;
    ensure!(task.repo == inquiry.repo, "decision repository changed");
    if let Some(receipt) = task.decision_effects.get(&inquiry.id) {
        ensure!(
            receipt == &request,
            "decision receipt binds another command"
        );
        outbox.update_decision(&inquiry.id, |d| {
            d.applied("exact task command receipt confirmed after restart")
        })?;
        return Ok(());
    }
    let mut pending_external = false;
    let receipt = match command {
        Operation::Abandon { run } => {
            abandonment_receipt(home, &task, &inquiry.id, &run, &request)?
                .then(|| format!("run {run}: exact abandonment receipt committed"))
        }
        Operation::RerunChecks {
            request,
            run,
            head,
            workflows,
        } => {
            let chat = inquiry.delivered_to.as_deref().unwrap_or_default();
            let path = home
                .join("chat-controls")
                .join(chat)
                .join(format!("{request}.json"));
            let state = crate::run::RunState::load_under(&run, home)?;
            let action = serde_json::json!({
                "run": run, "repo": state.repo,
                "pr": state.pr.as_ref().map(|pr| pr.url.as_str()),
                "head": head, "runs": workflows,
            });
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .filter(|receipt| receipt["status"] == "applied" && receipt["action"] == action)
                .map(|_| format!("chat-controls/{chat}/{request}.json: applied"))
        }
        Operation::MergePr {
            repo,
            number,
            head,
            request,
        } => {
            let action = crate::chat_merge::Request {
                repo,
                number,
                head,
                request,
            };
            let chat = inquiry
                .delivered_to
                .as_deref()
                .context("merge decision has no chat")?;
            let receipt = merge_receipt(home, chat, &action)?;
            pending_external = receipt
                .as_ref()
                .is_some_and(|receipt| receipt["status"] == "pending");
            if let Some(receipt) = &receipt
                && receipt["status"] != "merged"
                && receipt["status"] != "pending"
            {
                outbox.update_decision(&inquiry.id, |d| d.fail(&format!("Merge outcome requires reconciliation: {}. Reuse the same request ID to check only.", receipt["detail"])))?;
            }
            receipt
                .filter(|receipt| receipt["status"] == "merged")
                .map(|receipt| format!("confirmed merge receipt: {}", receipt["detail"]))
        }
        _ => None,
    };
    if let Some(receipt) = receipt {
        outbox.update_decision(&inquiry.id, |d| d.applied(&receipt))?;
    } else if decision.status == Status::InProgress
        && !pending_external
        && outbox
            .get(&inquiry.id)?
            .decision
            .as_ref()
            .is_some_and(|d| d.status == Status::InProgress)
    {
        let talks = Talks::at(home.join("talks"));
        let chat = inquiry
            .delivered_to
            .as_deref()
            .context("executing decision has no chat")?;
        if !talks.turn_held(chat) && !crate::chat_worker::turn_worker_active(&talks, chat) {
            outbox.update_decision(&inquiry.id, |d| d.fail("Execution ended without a confirmed effect receipt. Inspect the operation before retrying; restart did not replay it."))?;
        }
    }
    Ok(())
}

/// Record the owner's choice for a decision from an open standing chat. The
/// chat agent interprets the owner's request directly, a merge included; no
/// quote, consent phrase or task ownership is required. See [`record_decision`].
pub fn decide(
    talks: &Talks,
    home: &Path,
    talk: &str,
    question: &str,
    choice: &str,
) -> Result<crate::ask::Question> {
    authorize(talks, talk, crate::queue::CHAT_NODE)?;
    record_decision(home, question, choice, None)
}

/// Answer an open decision whose answer the machinery consumes
/// ([`crate::task_chat::awaits_decision`]: a merge or release approval, the
/// posting gate, triage, a divergence reconcile, a structured choice action).
/// The question must still be open and `choice` one of its offered choices;
/// the consumer behind it keeps its own state and transition checks.
fn record_decision(
    home: &Path,
    question: &str,
    choice: &str,
    title: Option<&str>,
) -> Result<crate::ask::Question> {
    use crate::ask::{Answer, Questions};
    let questions = Questions::at(home.join("questions"));
    let id = questions.resolve_id(question)?;
    let q = questions.get(&id)?;
    let queue = Queue::at(home.join("queue"));
    for task in queue.list() {
        if q.run == task.id
            || task.runs.contains(&q.run)
            || q.routed_task.as_deref() == Some(&task.id)
        {
            ensure!(
                !task.stale_question(&q),
                "question predates task repository reassignment; inspect the current task and release it for fresh execution"
            );
        }
    }
    ensure!(
        crate::task_chat::awaits_decision(&q),
        "question {} is not a decision the machinery waits on; answer it with `magi answer`",
        q.short()
    );
    let (q, ()) = questions.update(&q.id, |r| {
        if crate::task_chat::saved_reply_requires_confirmation(home, r) {
            ensure!(
                crate::task_inquiry::Outbox::at(home)
                    .get(&r.id)?
                    .decision
                    .is_some_and(
                        |decision| decision.status != crate::task_decision::Status::Resolved
                    ),
                "saved decision is settled; no effect remains necessary"
            );
            r.status = crate::ask::QuestionStatus::Open;
            r.answer = None;
            r.answered_at = None;
            r.answer_authority = None;
        }
        ensure!(
            r.status.open(),
            "question {} is already {}",
            r.short(),
            r.status.as_str()
        );
        let reply = r.resolve_reply(choice)?.to_owned();
        ensure!(
            crate::land::approval(Some(&reply)) != crate::land::Approval::Merge || r.merge_target.is_some()
                || !(r.node == crate::land::APPROVAL_NODE || (r.node == crate::bump::NOTICE_NODE && r.seat == "release-watch")),
            "approval has no recorded PR/head; recover its target or discuss a non-merge operation in chat"
        );
        let replacement =
            r.node == crate::github_text::ASK_NODE && reply == crate::github_text::USE_MY_TEXT;
        ensure!(
            replacement || title.is_none(),
            "--title is only valid for a publication decision's `use my text` choice"
        );
        let title = if replacement {
            let title =
                title.context("use my text needs --title with the literal replacement title")?;
            ensure!(
                !title.trim().is_empty(),
                "replacement title must not be empty"
            );
            Some(title.to_owned())
        } else {
            None
        };
        let answer = if r.free_text() {
            Answer::Text(reply)
        } else {
            Answer::Choice(reply)
        };
        r.publication_title = title;
        r.answer(answer)?;
        // Persist acceptance before exposing the answer to a concurrent
        // consumer. A crash before the question save leaves a queued intent,
        // never a fabricated application receipt.
        let outbox = crate::task_inquiry::Outbox::at(home);
        if outbox.get(&r.id).is_ok() {
            outbox.update_decision(&r.id, |d| {
                let accepted =
                    d.accept(&format!("decision: {}", r.resolution().unwrap_or_default()))?;
                let changed = d.source_answer_at != r.answered_at;
                d.source_answer_at = r.answered_at;
                Ok(accepted || changed)
            })?;
        }
        Ok(())
    })?;
    Ok(q)
}

/// The conversation this process speaks for, or why it may not act.
pub fn authorize(talks: &Talks, run: &str, node: &str) -> Result<Talk> {
    if node != crate::queue::CHAT_NODE || run.trim().is_empty() {
        bail!("task control requires MAGI_NODE=chat and MAGI_RUN from a standing chat");
    }
    let id = crate::chat_watch::authorize(talks, run, node)?;
    talks.get(&id)
}

/// The task that recorded `run` (a full run id).
fn run_task(queue: &Queue, run: &str) -> Result<Task> {
    ensure!(
        crate::run::is_run_id(run),
        "an execution command needs the full run id"
    );
    queue
        .list()
        .into_iter()
        .find(|t| t.runs.iter().any(|r| r == run))
        .with_context(|| format!("run {run} belongs to no task"))
}

/// Claim a task from a full or unambiguous-prefix id and read it under the
/// claim, never from a lookup made before it.
fn claimed(queue: &Queue, reference: &str) -> Result<(crate::queue::Claim, Task)> {
    let id = queue.resolve_id(reference)?;
    let claim = queue
        .claim(&id)
        .with_context(|| format!("task {id} is claimed by a running daemon"))?;
    let task = queue.get(&id)?;
    Ok((claim, task))
}

fn check_hold_release(task: &Task, operation: &str) -> Result<()> {
    match task.status {
        TaskStatus::Queued
        | TaskStatus::Held
        | TaskStatus::Failed
        | TaskStatus::Blocked
        | TaskStatus::Parked => Ok(()),
        TaskStatus::Done => bail!(
            "cannot {operation} task {} (done): completed work must not be restarted; create a new task instead",
            task.id
        ),
        TaskStatus::Running => bail!(
            "cannot {operation} task {} (running): its run may still be active; use resume or abandon for the existing run instead",
            task.id
        ),
    }
}

/// Mark a task complete under its queue claim, as `magi task done` does.
pub fn done(queue: &Queue, home: &Path, reference: &str) -> Result<Task> {
    let (_claim, mut task) = claimed(queue, reference)?;
    task.succeed();
    // Manual completion also retires earlier attempts when the latest one landed.
    crate::daemon::supersede_prior_runs(&task, home);
    queue.put(&mut task)?;
    Ok(task)
}

/// Change priority under the same claim and state check as the terminal route.
pub fn set_priority(queue: &Queue, reference: &str, priority: i32) -> Result<Task> {
    let (_claim, mut task) = claimed(queue, reference)?;
    task.set_priority(priority)?;
    queue.put(&mut task)?;
    Ok(task)
}

/// Hold a task for the owner.
pub fn hold(queue: &Queue, reference: &str, reason: &str) -> Result<Task> {
    let (_claim, mut task) = claimed(queue, reference)?;
    check_hold_release(&task, "hold")?;
    let reason = reason.trim();
    task.hold_manual((!reason.is_empty()).then(|| reason.to_owned()));
    queue.put(&mut task)?;
    Ok(task)
}

/// Release a task (the same transition as `magi task release`: the attempt
/// count restarts, the history stays). An operator hold is released too: the
/// owner asked for it.
pub fn release(queue: &Queue, reference: &str) -> Result<Task> {
    let (_claim, mut task) = claimed(queue, reference)?;
    check_hold_release(&task, "release")?;
    crate::base_sync_recovery::ensure_generic_release(queue, &task)?;
    let run = task.execution_runs().last().cloned();
    let preserved = match run {
        Some(run) => crate::repair_budget::queue_resumption(queue, &mut task, &run)?,
        None => false,
    };
    if !preserved {
        task.release_operator("chat", None);
    }
    queue.put(&mut task)?;
    Ok(task)
}

/// Change a queued or held task's instruction and queue it again, in one
/// save under one claim. Only what [`Task::edit`] allows can be retried; the
/// attempt count follows [`Task::release`] (restarts), while runs, passes and
/// the resumption history are kept.
pub fn retry(
    queue: &Queue,
    reference: &str,
    title: Option<String>,
    instruction: &str,
) -> Result<Task> {
    ensure!(
        !instruction.trim().is_empty(),
        "the new instruction is empty"
    );
    let (_claim, mut task) = claimed(queue, reference)?;
    if let Some(state) = crate::base_sync_recovery::held_run(queue, &task)
        && state
            .base_sync_recovery
            .stop
            .as_ref()
            .is_some_and(|stop| stop.kind == crate::base_sync_recovery::StopKind::Conflict)
    {
        anyhow::bail!(
            "retry cannot authorize editing a retained base-sync stop. {} Keep the retained review criteria and use a dedicated repair grant for the narrow resolution.",
            state.base_sync_next_action()
        );
    }
    crate::base_sync_recovery::ensure_generic_release(queue, &task)?;
    let title = title.unwrap_or_else(|| crate::queue::title_from(instruction, 72));
    task.edit(title, instruction.to_owned())?;
    task.release_operator("chat", None);
    queue.put(&mut task)?;
    Ok(task)
}

/// Resume a stopped, blocked or parked run (its task's latest). Only queues
/// the pinned resume; returns the owning task.
pub fn resume(queue: &Queue, home: &Path, run: &str) -> Result<Task> {
    let task = run_task(queue, run)?;
    ensure!(
        task.execution_runs().last().map(String::as_str) == Some(run),
        "only the task's latest run can be resumed"
    );
    crate::resume::validate(home, run)?;
    ensure!(
        crate::resume::pin_queued(queue, run, None, None)?,
        "no queued task owns run {run}"
    );
    queue.get(&task.id)
}

// These reads and input checks cannot start an effect. Keep them shared by
// decision acceptance and execution so invalid input never pins a new request.
fn rerun_preflight(
    queue: &Queue,
    home: &Path,
    run: &str,
    request: &str,
) -> Result<crate::run::RunState> {
    run_task(queue, run)?;
    let state = crate::run::RunState::load_under(run, home)?;
    ensure!(state.pr.is_some(), "the run recorded no pull request");
    ensure!(
        crate::ask::valid_asset_name(request),
        "request must be a safe filename of at most 64 bytes"
    );
    Ok(state)
}

/// チャットのリクエスト単位で受付記録を残し、共通遷移でチェックを再実行する。
pub async fn rerun_checks(
    queue: &Queue,
    home: &Path,
    talk: &str,
    run: &str,
    request: &str,
    head: &str,
    workflows: &[String],
) -> Result<Vec<String>> {
    let state = rerun_preflight(queue, home, run, request).map_err(BeforeEffect)?;
    let dir = home.join("chat-controls").join(talk);
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{request}.lock")))?;
    crate::pr_head::try_lock_file(&lock)?;
    let path = dir.join(format!("{request}.json"));
    let pr = &state
        .pr
        .as_ref()
        .context("the run recorded no pull request")?
        .url;
    let action = serde_json::json!({
        "run": run, "repo": state.repo, "pr": pr, "head": head, "runs": workflows
    });
    if path.exists() {
        let prior: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure!(
            prior["action"] == action,
            "request ID already binds a different operation"
        );
        ensure!(
            prior["status"] == "applied",
            "previous request has an uncertain result; inspect its receipt before a new request"
        );
        return Ok(workflows.to_vec());
    }
    let record = |status: &str| -> Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(&serde_json::json!({
                "action": action, "status": status
            }))?,
        )?;
        std::fs::rename(tmp, &path)?;
        Ok(())
    };
    let operation = format!("chat:{talk}:{request}");
    let started = std::sync::atomic::AtomicBool::new(false);
    crate::daemon::rerun_checks_once(home, &operation, &state.repo, pr, head, workflows, || {
        // Once a receipt exists, failure must remain uncertain even if the
        // external command has not returned yet.
        started.store(true, std::sync::atomic::Ordering::SeqCst);
        record("started")
    })
    .await
    .map_err(|error| {
        if started.load(std::sync::atomic::Ordering::SeqCst) {
            error
        } else {
            anyhow::Error::new(BeforeEffect(error))
        }
    })?;
    record("applied")?;
    Ok(workflows.to_vec())
}

/// Abandon a recorded run whose driver is proven gone. No confirmation phrase
/// is needed; an unknown or live driver is refused by
/// [`crate::driver::abandon_proven_dead`], so a working driver is never broken.
pub fn abandon(queue: &Queue, home: &Path, run: &str) -> Result<()> {
    run_task(queue, run)?;
    crate::driver::abandon_proven_dead(home, run)
}

/// One line a person or agent can read after a successful command.
pub fn describe(task: &Task, what: &str) -> String {
    let status = match task.status {
        TaskStatus::Queued => "queued",
        _ => task.status.as_str(),
    };
    format!("{what} {} {} ({status})", task.short(), task.title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Source;
    use jiff::Timestamp;

    fn talk(talks: &Talks, id: &str) -> Talk {
        let mut t: Talk = serde_json::from_value(serde_json::json!({
            "schema":1,"id":id,"repo":".","agent":"worker","status":"open","turns":[],
            "created_at":Timestamp::now(),"updated_at":Timestamp::now(),
            "seat":crate::agent::SeatState::new("talk","worker",1)
        }))
        .unwrap();
        talks.put(&mut t).unwrap();
        t
    }

    fn filed(queue: &Queue, talk: &str, text: &str) -> Task {
        let mut t = Task::new(
            text.into(),
            text.into(),
            ".".into(),
            Source::Agent {
                run: talk.into(),
                node: crate::queue::CHAT_NODE.into(),
            },
        );
        queue.put(&mut t).unwrap();
        t
    }

    #[test]
    fn resume_recovery_waits_for_the_inquiry_publisher() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut task = filed(&queue, "talk-a", "work");
        let mut state = crate::run::RunState::new(
            task.repo.clone(),
            "main".into(),
            "base".into(),
            "work".into(),
            crate::config::Config::default(),
        );
        state.status = crate::run::RunStatus::Blocked;
        state.driver_exited = true;
        state.save_under(tmp.path()).unwrap();
        task.runs.push(state.id.clone());
        queue.put(&mut task).unwrap();
        let outbox = crate::task_inquiry::Outbox::at(tmp.path());
        let inquiry = outbox
            .file(
                &task,
                Some(state.id.clone()),
                "resume".into(),
                "Resume?",
                "",
            )
            .unwrap();
        let request = serde_json::to_string(&Operation::Resume {
            run: state.id.clone(),
        })
        .unwrap();
        outbox
            .update_decision(&inquiry.id, |d| {
                d.accept(&request)?;
                d.start()
            })
            .unwrap();
        let before = outbox.get(&inquiry.id).unwrap().decision.unwrap();
        let publisher = outbox.execution_claim(&inquiry.id).unwrap();
        reconcile_decisions(&queue, tmp.path()).unwrap();
        assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap(), before);
        drop(publisher);
        reconcile_decisions(&queue, tmp.path()).unwrap();
        let recovered = outbox.get(&inquiry.id).unwrap().decision.unwrap();
        assert_eq!(recovered.status, crate::task_decision::Status::RetryNeeded);
        assert!(recovered.retry_safe);
    }

    #[test]
    fn only_the_chat_seat_of_an_open_talk_is_authorized() {
        let tmp = tempfile::tempdir().unwrap();
        let talks = Talks::at(tmp.path().join("talks"));
        let t = talk(&talks, "talk-a");
        assert!(authorize(&talks, &t.id, "chat").is_ok());
        assert!(authorize(&talks, &t.id, "deputy").is_err());
        assert!(authorize(&talks, &t.id, "").is_err());
        assert!(authorize(&talks, "", "chat").is_err());
        assert!(
            authorize(&talks, "talk-", "chat").is_err(),
            "prefix is not an id"
        );
    }

    #[test]
    fn tasks_of_any_origin_are_controlled_on_request() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let theirs = filed(&queue, "talk-b", "theirs");
        let mut hand = Task::new("h".into(), "h".into(), ".".into(), Source::Human);
        hand.hold_manual(Some("operator".into()));
        queue.put(&mut hand).unwrap();
        // Another chat's task and an operator-held human task are the owner's
        // work too: the request goes through without any ownership scope.
        assert_eq!(
            hold(&queue, &theirs.id, "wait").unwrap().status,
            TaskStatus::Held
        );
        assert_eq!(
            release(&queue, &hand.id).unwrap().status,
            TaskStatus::Queued
        );
        let again = retry(&queue, &theirs.id, None, "new").unwrap();
        assert_eq!(again.instruction, "new");
        assert_eq!(again.status, TaskStatus::Queued);
    }

    #[test]
    fn hold_then_release_keeps_history_and_restarts_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut t = filed(&queue, "talk-a", "work");
        t.attempts = 3;
        t.runs.push("20260101-000000-aaaa".into());
        queue.put(&mut t).unwrap();
        let held = hold(&queue, &t.id, "owner asked").unwrap();
        assert_eq!(held.status, TaskStatus::Held);
        assert_eq!(held.hold_reason.as_deref(), Some("owner asked"));
        let back = release(&queue, &t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Queued);
        assert_eq!(back.attempts, 0);
        assert_eq!(back.runs, t.runs);
        assert!(back.resumption.is_some());
    }

    #[test]
    fn hold_and_release_refuse_done_and_running_without_saving() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        for status in [TaskStatus::Done, TaskStatus::Running] {
            let mut t = filed(&queue, "talk-a", "work");
            t.start("20260101-000000-aaaa".into());
            if status == TaskStatus::Done {
                t.succeed();
            }
            queue.put(&mut t).unwrap();
            let before = std::fs::read(queue.path_of(&t.id)).unwrap();
            for operation in ["hold", "release"] {
                let error = if operation == "hold" {
                    hold(&queue, &t.id, "owner asked")
                } else {
                    release(&queue, &t.id)
                }
                .unwrap_err()
                .to_string();
                assert!(error.contains(operation), "{error}");
                assert!(error.contains(&t.id), "{error}");
                assert!(error.contains(status.as_str()), "{error}");
                let guidance = if status == TaskStatus::Done {
                    "create a new task"
                } else {
                    "use resume or abandon"
                };
                assert!(error.contains(guidance), "{error}");
                assert_eq!(std::fs::read(queue.path_of(&t.id)).unwrap(), before);
            }
        }
    }

    #[test]
    fn hold_and_release_allow_unfinished_inactive_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        for status in [
            TaskStatus::Queued,
            TaskStatus::Held,
            TaskStatus::Failed,
            TaskStatus::Blocked,
            TaskStatus::Parked,
        ] {
            for operation in ["hold", "release"] {
                let mut t = filed(&queue, "talk-a", "work");
                t.status = status;
                t.hold_reason = Some("old reason".into());
                t.attempts = 3;
                t.runs.push("20260101-000000-aaaa".into());
                queue.put(&mut t).unwrap();
                let changed = if operation == "hold" {
                    let held = hold(&queue, &t.id, " new reason ").unwrap();
                    assert_eq!(held.status, TaskStatus::Held);
                    assert_eq!(held.hold_reason.as_deref(), Some("new reason"));
                    assert_eq!(held.attempts, t.attempts);
                    held
                } else {
                    let released = release(&queue, &t.id).unwrap();
                    assert_eq!(released.status, TaskStatus::Queued);
                    assert_eq!(released.attempts, 0);
                    assert_eq!(released.resumption.as_ref().unwrap().source, "chat");
                    released
                };
                assert_eq!(changed.runs, t.runs);
                assert_eq!(queue.get(&t.id).unwrap().status, changed.status);
            }
        }
    }

    #[test]
    fn retry_edits_and_requeues_in_one_save() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut t = filed(&queue, "talk-a", "old text");
        t.runs.push("20260101-000000-aaaa".into());
        t.attempts = 2;
        t.hold_manual(Some("stopped".into()));
        queue.put(&mut t).unwrap();
        let again = retry(&queue, &t.id, None, "new text").unwrap();
        assert_eq!(again.instruction, "new text");
        assert_eq!(again.status, TaskStatus::Queued);
        assert_eq!(again.attempts, 0);
        assert_eq!(again.runs, t.runs, "run history is kept");
        assert_eq!(again.resumption.as_ref().unwrap().source, "chat");
        assert_eq!(again.resumption.as_ref().unwrap().instruction, "new text");
        // A running task cannot be edited: nothing is changed.
        let mut running = filed(&queue, "talk-a", "busy");
        running.start("20260101-000000-bbbb".into());
        queue.put(&mut running).unwrap();
        assert!(retry(&queue, &running.id, None, "x").is_err());
        assert_eq!(queue.get(&running.id).unwrap().instruction, "busy");
        assert!(retry(&queue, &t.id, None, "  ").is_err());
    }

    #[test]
    fn execution_commands_need_the_full_id_of_a_recorded_run() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut t = filed(&queue, "talk-b", "work");
        t.runs.push("20260101-000000-aaaa".into());
        queue.put(&mut t).unwrap();
        assert_eq!(
            run_task(&queue, "20260101-000000-aaaa").unwrap().id,
            t.id,
            "any chat may address another chat's run"
        );
        assert!(run_task(&queue, "aaaa").is_err());
        assert!(run_task(&queue, "20260101-000000-bbbb").is_err());
        assert!(resume(&queue, tmp.path(), "aaaa").is_err());
    }

    #[test]
    fn abandon_needs_no_phrase_but_refuses_unrecorded_and_unknown_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut t = filed(&queue, "talk-b", "work");
        t.runs.push("20260101-000000-aaaa".into());
        queue.put(&mut t).unwrap();
        // A run no task recorded is refused before liveness is judged.
        assert!(abandon(&queue, tmp.path(), "20260101-000000-bbbb").is_err());
        // A recorded run whose driver is not proven dead (here: no state to
        // judge at all) is never abandoned.
        assert!(abandon(&queue, tmp.path(), "20260101-000000-aaaa").is_err());
    }
}
