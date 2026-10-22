//! Direct task and run control for the standing chat: `magi task control ...`.
//!
//! A chat agent can resume a run, hold or release a task, retry a task with a
//! changed instruction, rerun the failed checks of a pull request and abandon
//! a dead run by command, with no question or offered choice in between. Every
//! command goes through the transitions the rest of magi already uses
//! ([`crate::resume`], [`Task::hold_manual`], [`Task::release_operator`],
//! [`Task::edit`], [`crate::daemon::rerun_checks_once`],
//! [`crate::driver::abandon_proven_dead`]); this module only decides *who may
//! ask* and *about what*.
//!
//! - **Authorization.** `MAGI_NODE=chat` and `MAGI_RUN` equal to the exact id
//!   of an open conversation ([`crate::chat_watch::authorize`]).
//! - **Scope.** Tasks that conversation filed itself ([`Task::filed_by_chat`])
//!   or whose decision destination is its persisted [`Task::chat`], and their
//!   recorded runs. An inherited `origin_chat` alone does not count. Task ids and run ids
//!   are checked against the queue under the task's claim, not trusted.
//! - **Resume starts nothing here.** It pins the owning task to the run and
//!   queues it; the loop's ordinary start logic resumes it under the graph's
//!   execution lease, so a second driver cannot appear.
//! - **Abandon is individually confirmed.** The owner's latest persisted send
//!   must be exactly `abandon-run: <full run id>` and `--quote` must equal it;
//!   liveness is then judged by the shared judge under the owner lock.
//! - **Merging is not here.** It stays behind the approval gate.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::queue::{Queue, Task, TaskStatus};
use crate::talk::{Talk, Talks};

#[cfg(any(unix, windows))]
pub(crate) mod bridge;

/// Only these operations cross the standing turn's inherited capability.
/// Paths, commands, merge decisions and conversation identities are not input.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
pub enum Operation {
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
    /// 個別確認後、生存が否定された run を破棄する。
    Abandon {
        /// 完全な run ID。
        run: String,
        /// 最新の所有者メッセージ全文。
        quote: Option<String>,
    },
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
    ensure!(
        !crate::consult::pending_consults(&crate::ask::Questions::at(home.join("questions")), talk),
        "task control is excluded during consultation"
    );
    Ok(match operation {
        Operation::Resume { run } => {
            let t = resume(queue, home, talk, &run)?;
            format!(
                "{}; the loop (`magi serve`) resumes the run under its execution lease",
                describe(&t, "resume queued for")
            )
        }
        Operation::Hold { task, reason } => describe(&hold(queue, talk, &task, &reason)?, "held"),
        Operation::Release { task } => describe(&release(queue, talk, &task)?, "released"),
        Operation::Retry {
            task,
            title,
            instruction,
        } => describe(
            &retry(queue, talk, &task, title, &instruction)?,
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
        Operation::Abandon { run, quote } => {
            abandon(queue, talks, home, talk, &run, quote.as_deref())?;
            format!("abandoned run {run}")
        }
    })
}

/// The exact owner instruction that confirms abandoning `run`.
pub fn abandon_instruction(run: &str) -> String {
    format!("abandon-run: {run}")
}

/// The conversation this process speaks for, or why it may not act.
pub fn authorize(talks: &Talks, run: &str, node: &str) -> Result<Talk> {
    if node != crate::queue::CHAT_NODE || run.trim().is_empty() {
        bail!("task control requires MAGI_NODE=chat and MAGI_RUN from a standing chat");
    }
    let id = crate::chat_watch::authorize(talks, run, node)?;
    talks.get(&id)
}

/// Whether `task` was filed by exactly this conversation.
fn owned(task: &Task, talk: &str) -> bool {
    task.filed_by_chat() == Some(talk) || task.chat.as_deref() == Some(talk)
}

/// A task of this conversation, from a full or unambiguous-prefix id.
pub fn owned_task(queue: &Queue, talk: &str, reference: &str) -> Result<Task> {
    let id = queue.resolve_id(reference)?;
    let task = queue.get(&id)?;
    ensure!(
        owned(&task, talk),
        "task {} was not filed by this conversation",
        task.short()
    );
    Ok(task)
}

/// The task of this conversation that recorded `run` (a full run id).
pub fn owned_run_task(queue: &Queue, talk: &str, run: &str) -> Result<Task> {
    ensure!(
        crate::run::is_run_id(run),
        "an execution command needs the full run id"
    );
    queue
        .list()
        .into_iter()
        .find(|t| owned(t, talk) && t.runs.iter().any(|r| r == run))
        .with_context(|| format!("run {run} belongs to no task filed by this conversation"))
}

fn claimed(queue: &Queue, id: &str, talk: &str) -> Result<(crate::queue::Claim, Task)> {
    let claim = queue
        .claim(id)
        .with_context(|| format!("task {id} is claimed by a running daemon"))?;
    let task = queue.get(id)?;
    // The relation is re-checked under the claim, never trusted from a
    // lookup made before it.
    ensure!(owned(&task, talk), "task is not filed by this conversation");
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

/// Hold a task of this conversation for the owner.
pub fn hold(queue: &Queue, talk: &str, reference: &str, reason: &str) -> Result<Task> {
    let id = owned_task(queue, talk, reference)?.id;
    let (_claim, mut task) = claimed(queue, &id, talk)?;
    check_hold_release(&task, "hold")?;
    let reason = reason.trim();
    task.hold_manual((!reason.is_empty()).then(|| reason.to_owned()));
    queue.put(&mut task)?;
    Ok(task)
}

/// Release a task of this conversation (the same transition as `magi task
/// release`: the attempt count restarts, the history stays).
pub fn release(queue: &Queue, talk: &str, reference: &str) -> Result<Task> {
    let id = owned_task(queue, talk, reference)?.id;
    let (_claim, mut task) = claimed(queue, &id, talk)?;
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
    talk: &str,
    reference: &str,
    title: Option<String>,
    instruction: &str,
) -> Result<Task> {
    ensure!(
        !instruction.trim().is_empty(),
        "the new instruction is empty"
    );
    let id = owned_task(queue, talk, reference)?.id;
    let (_claim, mut task) = claimed(queue, &id, talk)?;
    let title = title.unwrap_or_else(|| crate::queue::title_from(instruction, 72));
    task.edit(title, instruction.to_owned())?;
    task.release_operator("chat", None);
    queue.put(&mut task)?;
    Ok(task)
}

/// Resume a stopped, blocked or parked run of this conversation's task. Only
/// queues the pinned resume; returns the owning task.
pub fn resume(queue: &Queue, home: &Path, talk: &str, run: &str) -> Result<Task> {
    let task = owned_run_task(queue, talk, run)?;
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
    owned_run_task(queue, talk, run)?;
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

/// Abandon a run of this conversation's task whose driver is proven gone.
/// `quote` must be the entire latest persisted owner send, and that send must
/// be exactly [`abandon_instruction`]. The evidence guard is held through the
/// abandon, so the owner cannot retract in between.
pub fn abandon(
    queue: &Queue,
    talks: &Talks,
    home: &Path,
    talk_id: &str,
    run: &str,
    quote: Option<&str>,
) -> Result<()> {
    owned_run_task(queue, talk_id, run)?;
    let _evidence = talks.answer_guard()?;
    let talk = talks.get(talk_id)?;
    let expected = abandon_instruction(run);
    let latest = crate::consult::latest_owner_instruction(&talk);
    ensure!(
        latest == Some(expected.as_str()) && quote == Some(expected.as_str()),
        "abandoning a run needs individual owner confirmation: the owner's latest persisted \
         message must be exactly `{expected}` and --quote must repeat it"
    );
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
    fn tasks_of_other_origins_are_out_of_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mine = filed(&queue, "talk-a", "mine");
        let theirs = filed(&queue, "talk-b", "theirs");
        let mut hand = Task::new("h".into(), "h".into(), ".".into(), Source::Human);
        queue.put(&mut hand).unwrap();
        assert!(hold(&queue, "talk-a", &mine.id, "wait").is_ok());
        assert!(hold(&queue, "talk-a", &theirs.id, "x").is_err());
        assert!(release(&queue, "talk-a", &hand.id).is_err());
        assert!(retry(&queue, "talk-a", &theirs.id, None, "new").is_err());
        assert_eq!(queue.get(&theirs.id).unwrap().status, TaskStatus::Queued);
    }

    #[test]
    fn hold_then_release_keeps_history_and_restarts_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut t = filed(&queue, "talk-a", "work");
        t.attempts = 3;
        t.runs.push("20260101-000000-aaaa".into());
        queue.put(&mut t).unwrap();
        let held = hold(&queue, "talk-a", &t.id, "owner asked").unwrap();
        assert_eq!(held.status, TaskStatus::Held);
        assert_eq!(held.hold_reason.as_deref(), Some("owner asked"));
        let back = release(&queue, "talk-a", &t.id).unwrap();
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
                    hold(&queue, "talk-a", &t.id, "owner asked")
                } else {
                    release(&queue, "talk-a", &t.id)
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
                    let held = hold(&queue, "talk-a", &t.id, " new reason ").unwrap();
                    assert_eq!(held.status, TaskStatus::Held);
                    assert_eq!(held.hold_reason.as_deref(), Some("new reason"));
                    assert_eq!(held.attempts, t.attempts);
                    held
                } else {
                    let released = release(&queue, "talk-a", &t.id).unwrap();
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
        let again = retry(&queue, "talk-a", &t.id, None, "new text").unwrap();
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
        assert!(retry(&queue, "talk-a", &running.id, None, "x").is_err());
        assert_eq!(queue.get(&running.id).unwrap().instruction, "busy");
        assert!(retry(&queue, "talk-a", &t.id, None, "  ").is_err());
    }

    #[test]
    fn execution_commands_need_a_full_run_id_of_an_owned_task() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut t = filed(&queue, "talk-a", "work");
        t.runs.push("20260101-000000-aaaa".into());
        queue.put(&mut t).unwrap();
        assert!(owned_run_task(&queue, "talk-a", "20260101-000000-aaaa").is_ok());
        assert!(owned_run_task(&queue, "talk-b", "20260101-000000-aaaa").is_err());
        assert!(owned_run_task(&queue, "talk-a", "aaaa").is_err());
        assert!(resume(&queue, tmp.path(), "talk-b", "20260101-000000-aaaa").is_err());
    }

    #[test]
    fn abandon_needs_the_exact_owner_message_and_a_matching_quote() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let talks = Talks::at(tmp.path().join("talks"));
        talk(&talks, "talk-a");
        let mut t = filed(&queue, "talk-a", "work");
        t.runs.push("20260101-000000-aaaa".into());
        queue.put(&mut t).unwrap();
        let run = "20260101-000000-aaaa";
        // No owner message at all.
        assert!(
            abandon(
                &queue,
                &talks,
                tmp.path(),
                "talk-a",
                run,
                Some("abandon-run: x")
            )
            .is_err()
        );
        assert!(abandon(&queue, &talks, tmp.path(), "talk-a", run, None).is_err());
        // An unrelated run is out of scope before anything else.
        assert!(
            abandon(
                &queue,
                &talks,
                tmp.path(),
                "talk-a",
                "20260101-000000-bbbb",
                None
            )
            .is_err()
        );
    }
}
