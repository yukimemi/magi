//! Telling a chat how a task it filed ended.
//!
//! A chat is single-turn: it speaks only when the operator writes, so "I will
//! tell you when it finishes" cannot be kept by the agent. This module keeps
//! it for the agent. When a task filed by a chat ([`Task::filed_by_chat`])
//! reaches a final state, a fixed English notice is queued as a draft of the
//! originating talk, and the talk's own turn machinery (its session, its
//! lease) runs it. No new seat and no new waiter: the same shape as
//! [`crate::consult`].
//!
//! - **A reconciler, not a hook.** [`sweep`] looks at the queue on every
//!   daemon tick, so whoever put the task in its final state (the loop, `magi
//!   task done`, the web UI, a restart) is covered the same way.
//! - **Once.** The task records what was reported ([`Task::chat_notice`]); the
//!   talk records the accepted key in the same write as the draft
//!   ([`talk::queue_once_matching`]). A crash between the two writes is repaired by the
//!   next sweep, which finds the key in the talk and only fixes the task.
//!   Lock order is task, then talk.
//! - **Closed or gone is skipped,** never reopened (unlike a consult).
//! - **A parking upgrade starts nothing.** The draft stays durable.

use std::path::Path;

use anyhow::{Result, bail};
use jiff::Timestamp;

use crate::config::Config;
use crate::prompt::{CHAT_TASK_NOTICE_HEADING, ChatTaskNotice, chat_task_notice};
use crate::queue::{ChatNotice, ChatNoticeOutcome, Queue, Task, TaskStatus};
use crate::talk::{self, Talks};

/// Facts about a task's latest run, read tolerantly: an unreadable record
/// only leaves them out of the notice.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunFacts {
    /// The pull request url, if the run opened one.
    pub pr: Option<String>,
    /// The winning candidate's summary, if there is one.
    pub summary: Option<String>,
}

/// Read [`RunFacts`] from `<home>/runs/<id>/run.json`.
pub fn run_facts(home: &Path, run: &str) -> RunFacts {
    let Ok(body) = std::fs::read_to_string(home.join("runs").join(run).join("run.json")) else {
        return RunFacts::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return RunFacts::default();
    };
    let text = |x: Option<&serde_json::Value>| {
        x.and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let winner = text(v.pointer("/tally/winner"));
    let summary = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|cs| {
            cs.iter()
                .find(|c| winner.is_some() && text(c.get("label")).as_deref() == winner.as_deref())
        })
        .and_then(|c| text(c.get("summary")));
    RunFacts {
        pr: text(v.pointer("/pr/url")),
        summary,
    }
}

/// The notice text for `task`, built from what magi recorded.
pub fn notice_text(task: &Task, facts: &RunFacts) -> String {
    let (status, reason) = match task.status {
        TaskStatus::Done => ("done", None),
        TaskStatus::Held => (
            "held (needs a decision before it runs again)",
            task.hold_reason.as_deref().or(task.last_error.as_deref()),
        ),
        TaskStatus::Blocked => ("blocked", task.block_reason.as_deref()),
        _ => ("not final", None),
    };
    chat_task_notice(&ChatTaskNotice {
        id: &task.id,
        title: &task.title,
        status,
        reason,
        run: task.runs.last().map(String::as_str),
        pr: facts.pr.as_deref(),
        summary: facts.summary.as_deref(),
    })
}

/// Queue the notice for `task` into its talk, once. `Skipped` when the talk is
/// gone or closed; an error only for a transient failure worth retrying.
/// The boolean records whether a new draft was actually added.
fn deliver(
    talks: &Talks,
    home: &Path,
    task: &Task,
    talk_id: &str,
    key: &str,
) -> Result<(ChatNoticeOutcome, bool)> {
    let gone = || !talks.list().iter().any(|t| t.id == talk_id);
    let mut talk = match talks.get(talk_id) {
        Ok(t) => t,
        Err(_) if gone() => return Ok((ChatNoticeOutcome::Skipped, false)),
        Err(e) => return Err(e),
    };
    if !talk.status.open() {
        return Ok((ChatNoticeOutcome::Skipped, false));
    }
    let facts = task
        .runs
        .last()
        .map(|r| run_facts(home, r))
        .unwrap_or_default();
    let text = notice_text(task, &facts);
    let prefix = format!("{}:", task.id);
    let previous = |stored: &str| {
        key == "done"
            && stored
                .strip_prefix(&prefix)
                .is_some_and(crate::queue::is_done_notice_key)
    };
    match talk::queue_once_matching(
        &mut talk,
        talks,
        &text,
        &format!("{prefix}{key}"),
        &previous,
    ) {
        Ok(added) => Ok((ChatNoticeOutcome::Posted, added)),
        Err(e) => match talks.get(talk_id) {
            Ok(t) if t.status.open() => Err(e),
            Ok(_) => Ok((ChatNoticeOutcome::Skipped, false)),
            Err(_) if gone() => Ok((ChatNoticeOutcome::Skipped, false)),
            Err(_) => Err(e),
        },
    }
}

/// Report every chat-filed task whose final state has not been reported.
/// Returns the talk ids that got a new draft.
pub fn sweep(queue: &Queue, talks: &Talks, home: &Path) -> Vec<String> {
    let mut posted = Vec::new();
    for task in queue.list() {
        if task.chat_notice_due().is_none() {
            continue;
        }
        let result = queue.modify(&task.id, |t| {
            // Re-judged under the task's lock, on the stored record.
            let (Some(key), Some(talk_id)) =
                (t.chat_notice_due(), t.filed_by_chat().map(str::to_owned))
            else {
                return false;
            };
            match deliver(talks, home, t, &talk_id, &key) {
                Ok((outcome, added)) => {
                    if added {
                        posted.push(talk_id);
                    }
                    t.chat_notice = Some(ChatNotice {
                        key,
                        at: Timestamp::now(),
                        outcome,
                    });
                    true
                }
                Err(e) => {
                    tracing::warn!("chat notice for task {} not queued: {e:#}", t.short());
                    false
                }
            }
        });
        if let Err(e) = result {
            tracing::warn!("chat notice for task {}: {e:#}", task.short());
        }
    }
    posted.sort();
    posted.dedup();
    posted
}

/// Open talks holding a completion notice in their draft. Only these are
/// started on the daemon's own initiative: an operator's unsent draft is not.
pub fn talks_to_start(talks: &Talks) -> Vec<String> {
    talks
        .list()
        .into_iter()
        .filter(|t| t.status.open() && pending_is_only_notices(t))
        .map(|t| t.id)
        .collect()
}

/// Is the draft made of completion notices alone? A notice appended to an
/// operator's unsent draft must not start that draft on the daemon's initiative.
fn pending_is_only_notices(t: &talk::Talk) -> bool {
    let head = format!("# {CHAT_TASK_NOTICE_HEADING}\n");
    let body = &t.pending;
    if body.is_empty() || !t.pending_attachments.is_empty() {
        return false;
    }
    let Some(cuts) = t.pending_breaks.as_deref() else {
        return false;
    };
    let mut starts = vec![0];
    starts.extend_from_slice(cuts);
    starts
        .iter()
        .all(|&s| body.is_char_boundary(s) && body.get(s..).is_some_and(|m| m.starts_with(&head)))
}

/// Start the chat's turn for every talk owed a notice, under the talk's own
/// lease, each spawned into `turns` (the daemon's in-flight set, which its exit
/// waits for). `parking` (an upgrade is parking) starts
/// nothing. Returns how many turns were started; a talk whose lease is held
/// elsewhere is left to its holder, who drains the draft before letting go.
pub fn start_turns(talks: &Talks, parking: bool, turns: &mut tokio::task::JoinSet<()>) -> usize {
    if parking {
        return 0;
    }
    let mut started = 0;
    for id in talks_to_start(talks) {
        let lease = match talks.claim_turn(&id) {
            Ok(Some(lease)) => lease,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("chat {id}: turn lease: {e:#}");
                continue;
            }
        };
        started += 1;
        let talks = talks.clone();
        turns.spawn(async move {
            let run = async {
                let talk = talks.get(&id)?;
                let (cfg, _) = Config::discover(&talk.repo, None)?;
                crate::consult::drain_turns(&talks, &id, lease, &cfg).await
            };
            if let Err(e) = run.await {
                tracing::warn!("chat {id}: notice turn failed: {e:#}");
            }
        });
    }
    started
}

/// The task a `magi talk post` comes from: `run` (the posting seat's run id)
/// must belong to a task filed by exactly this talk.
pub fn authorize_post(tasks: &[Task], run: &str, node: &str, talk_id: &str) -> Result<()> {
    if run.trim().is_empty() {
        bail!("MAGI_RUN is not set: `magi talk post` is for a task seat of the chat's own task");
    }
    if node.trim().is_empty() {
        bail!("MAGI_NODE is not set: `magi talk post` is for a task seat of the chat's own task");
    }
    if node == crate::queue::CHAT_NODE {
        bail!("a chat seat cannot post to its own transcript");
    }
    let task = tasks
        .iter()
        .find(|t| t.runs.iter().any(|r| r == run))
        .ok_or_else(|| anyhow::anyhow!("run {run} belongs to no task"))?;
    match task.filed_by_chat() {
        Some(id) if id == talk_id => Ok(()),
        _ => bail!("run {run} does not belong to a task filed from chat {talk_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Source;

    #[test]
    fn run_facts_reads_the_winners_summary_and_the_pr() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("runs").join("r1");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("run.json"),
            r#"{"candidates":[{"label":"A","summary":"first"},{"label":"B","summary":"second"}],
                "tally":{"winner":"B"},"pr":{"url":"https://example.test/pull/7"}}"#,
        )
        .expect("write");
        let f = run_facts(tmp.path(), "r1");
        assert_eq!(f.summary.as_deref(), Some("second"));
        assert_eq!(f.pr.as_deref(), Some("https://example.test/pull/7"));
        assert_eq!(run_facts(tmp.path(), "missing"), RunFacts::default());
    }

    #[test]
    fn the_notice_names_the_task_state_reason_and_links() {
        let mut t = Task::new(
            "fix it".to_owned(),
            "fix it".to_owned(),
            ".".into(),
            Source::Agent {
                run: "talk".to_owned(),
                node: crate::queue::CHAT_NODE.to_owned(),
            },
        );
        t.start("run-1".to_owned());
        t.fail("the gate failed", 1);
        let text = notice_text(
            &t,
            &RunFacts {
                pr: Some("https://example.test/pull/7".to_owned()),
                summary: None,
            },
        );
        assert!(text.starts_with("# Update on a task you filed"));
        for needle in [t.id.as_str(), "held", "the gate failed", "run-1", "pull/7"] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
    }

    #[test]
    fn a_post_needs_a_run_of_a_task_this_chat_filed() {
        let mut t = Task::new(
            "x".to_owned(),
            "x".to_owned(),
            ".".into(),
            Source::Agent {
                run: "talk-a".to_owned(),
                node: crate::queue::CHAT_NODE.to_owned(),
            },
        );
        t.start("run-1".to_owned());
        let tasks = [t];
        assert!(authorize_post(&tasks, "run-1", "implement", "talk-a").is_ok());
        assert!(authorize_post(&tasks, "run-1", "implement", "talk-b").is_err());
        assert!(authorize_post(&tasks, "", "implement", "talk-a").is_err());
        assert!(authorize_post(&tasks, "run-1", "", "talk-a").is_err());
        assert!(authorize_post(&tasks, "run-1", "chat", "talk-a").is_err());
        assert!(authorize_post(&tasks, "run-2", "implement", "talk-a").is_err());
    }
}
