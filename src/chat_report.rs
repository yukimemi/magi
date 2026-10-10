//! Telling a chat how the tasks it filed ended.
//!
//! A chat is single-turn: it cannot wait, so an agent that says "I will tell
//! you when it is done" has no way to. When a task a chat filed reaches an
//! ending ([`Task::chat_report_due`]: done, held, blocked) magi queues a notice
//! as the next draft of that talk, and the talk's own turn machinery (its
//! session, its turn gate) runs it - the shape [`crate::consult`] uses. No new
//! seat, no new waiter.
//!
//! - **A sweep, not hooks.** Endings are reached from many places (`settle`,
//!   `magi task done`, the conductor, the web's buttons), so [`sweep`] looks at
//!   the queue instead; the loop calls it on a timer. A notice can be one lap
//!   late, and none is queued while no `magi serve` / `magi web` runs.
//! - **Recorded first.** [`Queue::record_chat_report`] writes
//!   [`Task::chat_report`] under the task's lock *before* the draft is queued,
//!   and withdraws it if queueing fails: a crash loses one notice, never posts
//!   two.
//! - **A gone chat is silence.** A missing or closed talk is recorded as
//!   `skipped` and nothing is said; unlike a consult, a report never reopens it.

use anyhow::{Context, Result, bail};

use crate::queue::{CHAT_NODE, Queue, Task};
use crate::talk::{self, Talks};

/// Marks the notice so the chat can tell it from the owner's own words.
pub const HEADING: &str = "Task report from magi";

/// How often the loop sweeps.
pub const LAP: std::time::Duration = std::time::Duration::from_secs(5);

/// The notice for `task`'s present ending. Pure: `run` and `pr` are whatever
/// the caller could read, and what is missing is left out, never guessed (a
/// task done by hand says nothing about a merge).
pub fn message(task: &Task, run: Option<&crate::run::RunState>) -> String {
    use crate::queue::TaskStatus as S;
    let mut out = format!(
        "# {HEADING}\n\nTask {} (\"{}\") has {}.\n",
        task.id,
        task.title.trim(),
        match task.status {
            S::Done => "finished (status: done)",
            S::Held => "stopped and is held (status: held)",
            S::Blocked => "stopped and is blocked (status: blocked)",
            _ => "changed state",
        }
    );
    let reason = match task.status {
        S::Held => task.hold_reason.as_deref().or(task.last_error.as_deref()),
        S::Blocked => task.block_reason.as_deref().or(task.last_error.as_deref()),
        _ => None,
    };
    if let Some(r) = reason.map(str::trim).filter(|r| !r.is_empty()) {
        out.push_str(&format!("\nReason: {}\n", first_chars(r, 400)));
    }
    if let Some(id) = task.runs.last() {
        out.push_str(&format!("\nLinked run: {id}\n"));
    }
    if let Some(run) = run {
        let status = serde_json::to_value(run.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        if !status.is_empty() {
            out.push_str(&format!("Run status: {status}\n"));
        }
        if let Some(pr) = &run.pr {
            out.push_str(&format!("Pull request: {} ({})\n", pr.url, pr.state));
        }
        let winner = run
            .tally
            .as_ref()
            .and_then(|t| run.candidates.iter().find(|c| c.label == t.winner))
            .or_else(|| run.candidates.first());
        if let Some(c) = winner
            && let Some(line) = c.summary.lines().map(str::trim).find(|l| !l.is_empty())
        {
            out.push_str(&format!("Result: {}\n", first_chars(line, 300)));
        }
    }
    out.push_str(
        "\nTell the operator this outcome in your own words, briefly. Do not edit the repository.",
    );
    out
}

fn first_chars(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    line.chars().take(n).collect()
}

/// One pass: queue a notice for every chat task with an unreported ending.
///
/// `kick(talk_id)` is called once per talk that received a draft, to start the
/// turn; it must return quickly (spawn, do not wait). Returns the ids of the
/// tasks reported. A task that cannot be handled is logged and left for the
/// next lap.
pub fn sweep(
    queue: &Queue,
    talks: &Talks,
    home: &std::path::Path,
    kick: &dyn Fn(&str),
) -> Vec<String> {
    let mut reported = Vec::new();
    for task in queue.list() {
        if task.chat_report_due().is_none() {
            continue;
        }
        match report_one(queue, talks, home, &task.id) {
            Ok(Some(talk_id)) => {
                reported.push(task.id.clone());
                if let Some(id) = talk_id {
                    kick(&id);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("chat report for task {}: {e:#}", task.short()),
        }
    }
    reported
}

/// `Ok(None)`: nothing was due. `Ok(Some(None))`: recorded as skipped.
/// `Ok(Some(Some(talk)))`: a draft was queued for `talk`.
fn report_one(
    queue: &Queue,
    talks: &Talks,
    home: &std::path::Path,
    id: &str,
) -> Result<Option<Option<String>>> {
    let gone = |t: &Task| {
        let Some(talk) = t.filed_by_chat() else {
            return true;
        };
        !talks.path_of(talk).exists() || talks.get(talk).map_or(true, |t| !t.status.open())
    };
    // An unreadable (not missing) talk is a transient failure, not a reason
    // to record "skipped" for good.
    if let Some(talk) = queue.get(id)?.filed_by_chat()
        && talks.path_of(talk).exists()
        && talks.get(talk).is_err()
    {
        bail!("talk {talk} is unreadable");
    }
    let Some((task, key)) = queue.record_chat_report(id, gone)? else {
        return Ok(None);
    };
    if task.chat_report.as_ref().is_some_and(|r| r.skipped) {
        return Ok(Some(None));
    }
    let talk_id = task.filed_by_chat().context("task has no chat")?.to_owned();
    let run = task
        .runs
        .last()
        .and_then(|r| crate::run::RunState::load_under(r, home).ok());
    let text = message(&task, run.as_ref());
    let queued = talks
        .get(&talk_id)
        .and_then(|mut talk| talk::queue(&mut talk, talks, &text, Vec::new()));
    if let Err(e) = queued {
        let _ = queue.withdraw_chat_report(&task.id, &key);
        return Err(e).context("queue the report into the chat");
    }
    Ok(Some(Some(talk_id)))
}

/// Run [`sweep`] every [`LAP`] until `stop`. With no `kick` of its own the
/// loop starts the turn itself, under the talk's cross-process lease.
pub async fn run(
    queue: Queue,
    talks: Talks,
    home: std::path::PathBuf,
    kick: Option<crate::daemon::TalkKick>,
    stop: crate::daemon::Stop,
) {
    while !stop.stopped() {
        let (queue, talks, home, kick) = (queue.clone(), talks.clone(), home.clone(), kick.clone());
        let parking = stop.clone();
        let handle = tokio::runtime::Handle::current();
        let swept = tokio::task::spawn_blocking(move || {
            sweep(&queue, &talks, &home, &|id| match &kick {
                Some(k) => k.call(id),
                // A parking upgrade starts no turn; the draft stays durable.
                None if parking.parking() => {}
                None => {
                    let (talks, id) = (talks.clone(), id.to_owned());
                    handle.spawn(async move {
                        if let Err(e) = drain_unattended(&talks, &id).await {
                            tracing::warn!("chat report turn for talk {id}: {e:#}");
                        }
                    });
                }
            })
        })
        .await;
        if let Err(e) = swept {
            tracing::warn!("chat report sweep: {e}");
        }
        tokio::time::sleep(LAP).await;
    }
}

/// Run the draft queued on `talk_id` here, if the talk's lease is free. A held
/// lease means its holder drains the draft before letting go.
pub async fn drain_unattended(talks: &Talks, talk_id: &str) -> Result<()> {
    let Some(lease) = talks.claim_turn(talk_id)? else {
        return Ok(());
    };
    let talk = talks.get(talk_id)?;
    let (cfg, _) = crate::config::Config::discover(&talk.repo, None)?;
    crate::consult::drain_owned(talks, &cfg, talk_id, lease).await
}

/// `magi talk post`: add an agent-authored message to a transcript without
/// starting a turn.
///
/// `run` and `node` are the caller's `MAGI_RUN` / `MAGI_NODE`. The caller must
/// be a seat working on a task that this very talk filed; anything else is
/// refused. The message is not in the chat agent's CLI session memory.
pub fn post(
    queue: &Queue,
    talks: &Talks,
    talk_id: &str,
    run: Option<&str>,
    node: Option<&str>,
    message: &str,
) -> Result<()> {
    let run = run.map(str::trim).filter(|r| !r.is_empty());
    let Some(run) = run else {
        bail!("`magi talk post` is for agent seats: MAGI_RUN is not set");
    };
    if node == Some(CHAT_NODE) {
        bail!("the chat itself answers in its turn; `magi talk post` is for implementer seats");
    }
    let talk_id = talks.resolve_id(talk_id)?;
    let tasks = queue.list();
    let task = tasks
        .iter()
        .find(|t| t.runs.iter().any(|r| r == run) || t.id == run)
        .with_context(|| format!("MAGI_RUN {run} does not belong to any task"))?;
    if task.filed_by_chat() != Some(talk_id.as_str()) {
        bail!(
            "task {} was not filed by chat {}; a seat may only post to the chat that filed its task",
            task.short(),
            crate::queue::short(&talk_id)
        );
    }
    talk::post_agent(talks, &talk_id, message)
}
