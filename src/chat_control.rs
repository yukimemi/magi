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
        } => {
            crate::chat_merge::merge(
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
            .await?
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
    ensure!(
        crate::task_chat::awaits_decision(&q),
        "question {} is not a decision the machinery waits on; answer it with `magi answer`",
        q.short()
    );
    let (q, ()) = questions.update(&q.id, |r| {
        ensure!(
            r.status.open(),
            "question {} is already {}",
            r.short(),
            r.status.as_str()
        );
        let reply = r.resolve_reply(choice)?.to_owned();
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
        r.answer(answer)
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
    task.release_operator("chat", None);
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
        task.runs.last().map(String::as_str) == Some(run),
        "only the task's latest run can be resumed"
    );
    crate::resume::validate(home, run)?;
    ensure!(
        crate::resume::pin_queued(queue, run, None, None)?,
        "no queued task owns run {run}"
    );
    queue.get(&task.id)
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
    run_task(queue, run)?;
    let state = crate::run::RunState::load_under(run, home)?;
    let pr = &state
        .pr
        .as_ref()
        .context("the run recorded no pull request")?
        .url;
    ensure!(
        crate::ask::valid_asset_name(request),
        "request must be a safe filename of at most 64 bytes"
    );
    let dir = home.join("chat-controls").join(talk);
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{request}.lock")))?;
    crate::pr_head::try_lock_file(&lock)?;
    let path = dir.join(format!("{request}.json"));
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
    crate::daemon::rerun_checks_once(home, &operation, &state.repo, pr, head, workflows, || {
        record("started")
    })
    .await?;
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
