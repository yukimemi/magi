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
//! - **Scope.** Only tasks that conversation filed itself
//!   ([`Task::filed_by_chat`]) and the runs recorded in their `runs`. A
//!   follow-up's inherited `origin_chat` does not count. Task ids and run ids
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

/// The ledger identity of a chat-requested rerun. It is not a question id.
pub const RERUN_REQUEST: &str = "chat-control";

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
    task.filed_by_chat() == Some(talk)
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

/// Hold a task of this conversation for the owner.
pub fn hold(queue: &Queue, talk: &str, reference: &str, reason: &str) -> Result<Task> {
    let id = owned_task(queue, talk, reference)?.id;
    let (_claim, mut task) = claimed(queue, &id, talk)?;
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
    let token = format!("chat:{talk}");
    ensure!(
        crate::resume::pin_queued(queue, run, &token, None, None)?,
        "no queued task owns run {run}"
    );
    queue.get(&task.id)
}

/// Rerun the failed checks of the pull request a run of this conversation's
/// task opened, once per head and workflow run. Returns the workflow run ids
/// that were requested.
pub async fn rerun_checks(
    queue: &Queue,
    home: &Path,
    talk: &str,
    run: &str,
) -> Result<Vec<String>> {
    use crate::release_watch::{GhForge, ReleaseForge};
    owned_run_task(queue, talk, run)?;
    let state = crate::run::RunState::load_under(run, home)?;
    let pr = state
        .pr
        .as_ref()
        .map(|p| p.url.clone())
        .context("the run recorded no pull request")?;
    let snap = GhForge.snapshot(&state.repo, &pr).await?;
    ensure!(
        snap.state == crate::land::PrLifecycle::Open,
        "the pull request is no longer open"
    );
    let mut failed: Vec<String> = Vec::new();
    for check in &snap.checks {
        if let Some(id) = &check.run
            && crate::release_watch::is_failed(check.verdict)
            && !failed.contains(id)
        {
            failed.push(id.clone());
        }
    }
    ensure!(!failed.is_empty(), "no failed workflow run to rerun");
    ensure!(failed.len() <= 20, "too many failed workflow runs (max 20)");
    crate::daemon::rerun_checks_once(
        home,
        RERUN_REQUEST,
        &state.repo,
        &pr,
        &snap.head,
        &failed,
        || Ok(()),
    )
    .await?;
    Ok(failed)
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
