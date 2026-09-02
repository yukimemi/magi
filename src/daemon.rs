//! The unattended loop: take the next task, run the graph, record what
//! happened, take the next one.
//!
//! This is what turns magi from a command a human types into something an
//! agent can hand work to. [`crate::queue`] is the mailbox; this module is the
//! thing that empties it. Nothing here decides *how* a task is implemented —
//! that is [`crate::graph`] — it only decides which task runs next, and what a
//! finished run means for the task that produced it.
//!
//! # One run at a time, on purpose
//!
//! There is no `--jobs` flag and there will not be one. A single run is
//! already internally parallel: candidates implement concurrently and judges
//! rank concurrently, so the machine is not idle while one task is in flight.
//! The real constraint is not CPU but the agent CLIs' quota, and two graphs at
//! once doubles the burn rate on exactly the resource whose exhaustion produces
//! [`RunStatus::Stalled`]. Serialising the loop is what keeps a full backlog
//! from converting the whole day's quota into a pile of untrustworthy verdicts.
//!
//! # A crash is legible
//!
//! The task is written as [`crate::queue::TaskStatus::Running`], with its run
//! id, *before* the graph starts, and is only rewritten once the run reaches a
//! terminal status. A daemon killed mid-run therefore leaves the task
//! `Running` and pointing at the run that was in flight, which is the state a
//! human needs to see: the run's own report explains how far it got, and the
//! task can be released deliberately. The alternative — reverting the task to
//! `Queued` on the way out — would hide the abandoned run and re-spend its
//! quota on the next poll.
//!
//! # Retries are bounded
//!
//! Every attempt at a task consumes one of [`Opts::max_attempts`], after which
//! the task is [`crate::queue::TaskStatus::Held`] for a human. The one
//! exception is a run that ended `Stalled`: the panel collapsed because the
//! agent CLIs hit their quota, which is a fact about the machine and not about
//! the task, so it must not spend an attempt. Without that exception a quota
//! outage would quietly hold the entire backlog, and the operator would come
//! back to a reset quota and nothing left that the loop is willing to run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::config::{Config, MergeMode};
use crate::graph::Runner;
use crate::queue::{Queue, Task};
use crate::run::{RunState, RunStatus};

/// On-disk format for [`Status`]. Bumped when a field's meaning changes.
pub const SCHEMA: u32 = 1;

/// How often the status file is refreshed. A reader treats a status file older
/// than [`STALE_SECS`] as "no daemon", so the heartbeat has to be brisk enough
/// that a busy daemon is never mistaken for a dead one.
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// How old a heartbeat may be before a reader calls the daemon dead. Six
/// missed beats: long enough to survive a slow filesystem, short enough that
/// a crashed daemon is not still reported as running a task.
///
/// The single threshold every reader shares — the web UI's `/api/health` and
/// `magi doctor` both call [`Reading::running`] rather than each comparing
/// against their own copy of this number, so a crashed daemon cannot look
/// alive on one screen and dead on another.
pub const STALE_SECS: i64 = 30;

/// Default queue poll interval.
pub const POLL: Duration = Duration::from_secs(5);

/// How old a claim has to be before startup sweeps it. Longer than any run
/// this graph plausibly takes, so a sweep cannot pull a task out from under a
/// daemon that is merely slow.
pub const STALE_CLAIM: Duration = Duration::from_secs(6 * 60 * 60);

/// What the loop is working on, for the status file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Current {
    /// Task id being run.
    pub task: String,
    /// Run id the task produced.
    pub run: String,
}

/// The daemon's liveness, published to `<home>/daemon.json`.
///
/// This is the only interface between the loop and the web UI, which is why it
/// carries `updated_at` as well as `started_at`: a reader cannot tell a
/// running daemon from a `SIGKILL`ed one by the file's existence alone, but it
/// can compare the heartbeat against the clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// On-disk format version.
    pub schema: u32,
    /// Process id, so a human can find or kill the daemon.
    pub pid: u32,
    /// When this process started.
    pub started_at: Timestamp,
    /// Last heartbeat.
    pub updated_at: Timestamp,
    /// True when the queue has nothing runnable.
    pub idle: bool,
    /// The task and run in flight, if any.
    pub current: Option<Current>,
    /// Tasks that reached a terminal status in this process.
    pub completed: usize,
    /// Queue polls since start, so a wedged loop shows up as a frozen count.
    pub polls: u64,
}

impl Status {
    /// A fresh, idle status for this process.
    #[must_use]
    pub fn new() -> Self {
        let now = Timestamp::now();
        Self {
            schema: SCHEMA,
            pid: std::process::id(),
            started_at: now,
            updated_at: now,
            idle: true,
            current: None,
            completed: 0,
            polls: 0,
        }
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

/// How the loop should behave.
#[derive(Debug, Clone)]
pub struct Opts {
    /// Repository used by tasks that name none.
    pub repo: PathBuf,
    /// Explicit `magi.toml`, instead of the discovered layer stack.
    pub config: Option<PathBuf>,
    /// Queue poll interval.
    pub poll: Duration,
    /// Attempts a task gets before it is held for a human.
    pub max_attempts: usize,
    /// Drain what is runnable now, then return, instead of waiting for more.
    pub once: bool,
    /// Merge mode override (`none`, `local`, `pr`); `None` keeps the config's.
    pub merge: Option<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            repo: PathBuf::from("."),
            config: None,
            poll: POLL,
            max_attempts: 2,
            once: false,
            merge: None,
        }
    }
}

/// Where the status file lives.
#[must_use]
pub fn status_path() -> PathBuf {
    crate::run::home().join("daemon.json")
}

/// Publish the status file for this process.
pub fn write_status(status: &Status) -> Result<()> {
    write_status_to(&status_path(), status)
}

/// Publish a status to an explicit path.
///
/// Written to a sibling `.tmp` and renamed, because the web UI reads this file
/// on every health poll and must never see a half-written one.
pub fn write_status_to(path: &Path, status: &Status) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(status).context("serialize daemon status")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Delete the status file. Called on the way out so a clean exit reads as
/// "no daemon" rather than as a daemon whose heartbeat merely stopped.
pub fn clear_status() {
    let _ = std::fs::remove_file(status_path());
}

/// The daemon's published state, read permissively.
///
/// This mirrors [`Status`], but is a separate declaration on purpose: every
/// field defaults, so a status file from an older or newer magi still yields
/// a usable reading — one this build has never heard of — instead of a parse
/// error that hides the daemon entirely.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Reading {
    /// Format version the daemon claims.
    pub schema: u32,
    /// Daemon process id, for an operator who wants to stop it.
    pub pid: Option<u32>,
    /// When that process started.
    pub started_at: Option<Timestamp>,
    /// Last heartbeat. Absent means the file is unusable, hence not running.
    pub updated_at: Option<Timestamp>,
    /// True when the queue had nothing runnable at the last poll.
    pub idle: bool,
    /// What the daemon is working on.
    pub current: Option<Current>,
    /// Tasks this daemon process has finished.
    pub completed: u64,
    /// Queue polls this daemon process has made.
    pub polls: u64,
}

impl Reading {
    /// Seconds since the last heartbeat, or `None` when there has never been
    /// one.
    #[must_use]
    pub fn age_secs(&self, now: Timestamp) -> Option<i64> {
        self.updated_at
            .map(|at| (now.as_second() - at.as_second()).max(0))
    }

    /// Whether the loop counts as running: a heartbeat no older than
    /// [`STALE_SECS`]. The alternative is a reader that claims a task is in
    /// progress hours after the daemon that owned it was killed.
    #[must_use]
    pub fn running(&self, now: Timestamp) -> bool {
        self.age_secs(now).is_some_and(|secs| secs <= STALE_SECS)
    }
}

/// Read `<home>/daemon.json` permissively, or `None` when there is nothing
/// usable there.
///
/// Missing, half-written and unparseable all collapse to `None`, because the
/// only question a reader asks is whether a daemon is alive, and a file it
/// cannot read is not evidence that one is.
#[must_use]
pub fn read_status(home: &Path) -> Option<Reading> {
    let body = std::fs::read_to_string(home.join("daemon.json")).ok()?;
    serde_json::from_str(&body).ok()
}

/// Remove claim files older than `older_than` and return the task ids swept.
///
/// A daemon killed with `SIGKILL` never runs [`crate::queue::Claim`]'s
/// destructor, and the orphaned `.lock` file would make its task permanently
/// unclaimable — the backlog would stop for good at exactly the task that was
/// in flight when the machine went down.
///
/// The test is age alone. There is no portable way to ask whether the pid
/// recorded in the lock is still alive and still magi (pids are reused, and
/// `/proc` does not exist on two of the three platforms magi targets), so this
/// trades a check it cannot make for a bound it can. The risk is real and
/// one-sided: a run that outlives `older_than` can have its claim swept while
/// it is still working, letting a second daemon start a second run on the same
/// task. [`STALE_CLAIM`] is therefore set an order of magnitude above any
/// plausible run, and the sweep is only ever called at startup, when this
/// process knows it holds no claims of its own.
pub fn sweep_stale_claims(queue: &Queue, older_than: Duration) -> Vec<String> {
    let mut swept: Vec<String> = std::fs::read_dir(queue.root())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lock"))
        .filter(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .and_then(|t| t.elapsed().map_err(std::io::Error::other))
                .is_ok_and(|age| age >= older_than)
        })
        .filter(|p| std::fs::remove_file(p).is_ok())
        .filter_map(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(std::borrow::ToOwned::to_owned)
        })
        .collect();
    swept.sort_unstable();
    swept
}

/// Record a finished run against the task it came from.
///
/// Kept pure and separate from the loop because this mapping *is* the retry
/// policy, and a policy that can only be exercised by spawning a graph is a
/// policy nobody checks. The table:
///
/// | run status            | task becomes        | attempt spent |
/// |-----------------------|---------------------|---------------|
/// | `Merged`, `Ready`     | `Done`              | yes           |
/// | `Stalled`             | `Failed` (requeued) | **no**        |
/// | `Blocked`, `Failed`   | `Failed`, or `Held` | yes           |
/// | anything non-terminal | `Failed`, or `Held` | yes           |
///
/// `Stalled` is the interesting row: the judging panel lost its quorum because
/// agent CLIs hit their quota, so the task goes back in line without spending
/// an attempt. A non-terminal status means `execute` returned while the graph
/// was still mid-flight, which is a bug rather than a verdict; it is treated as
/// a failure so that a task cannot loop on it forever.
pub fn settle(task: &mut Task, status: RunStatus, detail: &str, max_attempts: usize) {
    match status {
        RunStatus::Merged | RunStatus::Ready => task.succeed(),
        RunStatus::Stalled => task.stall(detail),
        RunStatus::Blocked | RunStatus::Failed => task.fail(detail, max_attempts),
        other => task.fail(
            format!(
                "the graph stopped at `{}` without reaching a terminal status: {detail}",
                label(other)
            ),
            max_attempts,
        ),
    }
}

/// Run the loop until Ctrl-C, or until the queue drains with [`Opts::once`].
///
/// Ctrl-C does not abandon a run in flight. Killing the graph mid-node leaves
/// worktrees, branches and agent sessions behind, and every agent call already
/// paid for is lost; finishing the run costs the operator a wait and saves them
/// a cleanup. The signal therefore sets a flag: the current `execute` runs to
/// its terminal status, the task's outcome is recorded, and only then does the
/// loop return. An operator who genuinely wants the run dead still has a second
/// Ctrl-C, which the runtime turns into a process kill — and the task left
/// `Running` then tells the next daemon, and the next human, where to look.
pub async fn serve(opts: Opts) -> Result<()> {
    let queue = Queue::open();

    let swept = sweep_stale_claims(&queue, STALE_CLAIM);
    if !swept.is_empty() {
        tracing::warn!(
            "swept {} stale claim(s) left behind by an earlier daemon: {}",
            swept.len(),
            swept.join(", ")
        );
    }

    // The status file is a *snapshot*, not a stream of events: a reader only
    // ever wants the latest values, and every tick rewrites the whole file
    // anyway. A shared `Mutex<Status>` therefore says exactly what is meant,
    // while an mpsc channel would force the loop to re-send unchanged fields on
    // every heartbeat — or the heartbeat to keep its own shadow copy of them —
    // for no gain. The lock is only ever held across a field assignment, never
    // across an await.
    let status = Arc::new(Mutex::new(Status::new()));
    write_status(&lock(&status)).context("publish the daemon status file")?;
    let beat = tokio::spawn(heartbeat(Arc::clone(&status)));

    let stopping = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Notify::new());
    let signal = {
        let stopping = Arc::clone(&stopping);
        let wake = Arc::clone(&wake);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                stopping.store(true, Ordering::SeqCst);
                wake.notify_waiters();
                tracing::info!("shutdown requested; a run in flight will be finished first");
            }
        })
    };

    tracing::info!(
        "magi serve: queue {} (poll {}s, {} attempts per task, one run at a time)",
        queue.root().display(),
        opts.poll.as_secs(),
        opts.max_attempts
    );

    let outcome = drive(&opts, &queue, &status, &stopping, &wake).await;

    beat.abort();
    signal.abort();
    clear_status();
    outcome
}

/// Refresh the status file on a fixed tick.
///
/// Separate from the loop because a run takes tens of minutes: a status file
/// written only between tasks would look stale for the whole of every run, and
/// a reader would report the daemon dead exactly while it was busiest.
async fn heartbeat(status: Arc<Mutex<Status>>) {
    loop {
        tokio::time::sleep(HEARTBEAT).await;
        let snapshot = {
            let mut guard = lock(&status);
            guard.updated_at = Timestamp::now();
            guard.clone()
        };
        if let Err(e) = write_status(&snapshot) {
            // A failed heartbeat must not take the daemon down: the loop is the
            // product, the status file is only the window onto it.
            tracing::warn!("could not refresh the daemon status file: {e:#}");
        }
    }
}

/// The loop proper, factored out so [`serve`] owns only setup and teardown.
async fn drive(
    opts: &Opts,
    queue: &Queue,
    status: &Arc<Mutex<Status>>,
    stopping: &AtomicBool,
    wake: &Notify,
) -> Result<()> {
    // Only consulted by `once`, where a task that just failed is still
    // `runnable` and would otherwise be picked up again inside the same drain.
    // In the long-running mode a later poll retrying a failed task is the point,
    // and the attempt counter is what bounds it.
    let mut attempted: Vec<String> = Vec::new();

    while !stopping.load(Ordering::SeqCst) {
        lock(status).polls += 1;

        let candidates: Vec<Task> = runnable(queue)
            .into_iter()
            .filter(|t| !opts.once || !attempted.contains(&t.id))
            .collect();

        let mut ran = false;
        for candidate in candidates {
            if stopping.load(Ordering::SeqCst) {
                break;
            }
            // A claim we cannot take means another daemon, or a human running
            // `magi run`, got there first. That is not the task's fault and
            // must not spend one of its attempts: move to the next candidate
            // rather than recording a failure.
            let Ok(_claim) = queue.claim(&candidate.id) else {
                tracing::debug!("task {} is claimed elsewhere; skipping", candidate.short());
                continue;
            };
            // Re-read under the claim: the task on disk may have been held or
            // edited between the listing and the lock.
            let mut task = match queue.get(&candidate.id) {
                Ok(t) if t.status.runnable() => t,
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("could not re-read task {}: {e:#}", candidate.short());
                    continue;
                }
            };
            attempted.push(task.id.clone());
            lock(status).idle = false;
            attempt(opts, queue, status, &mut task).await;
            {
                let mut guard = lock(status);
                guard.current = None;
                guard.completed += 1;
            }
            ran = true;
            break;
        }

        if ran {
            continue;
        }

        lock(status).idle = true;
        if opts.once {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(opts.poll) => {}
            () = wake.notified() => {}
        }
    }
    Ok(())
}

/// Run one claimed task to a terminal status and record the outcome.
///
/// Every transition is flushed to the queue as it happens, so the state on disk
/// is what actually occurred rather than what this process still intends to
/// write.
async fn attempt(opts: &Opts, queue: &Queue, status: &Arc<Mutex<Status>>, task: &mut Task) {
    let repo = repo_for(task, &opts.repo);
    tracing::info!(
        "task {} — {} (repo {})",
        task.short(),
        task.title,
        repo.display()
    );

    let config = match prepare(&repo, opts) {
        Ok(c) => c,
        Err(e) => {
            // A setup failure spends an attempt even though no run was minted.
            // Without that, a task naming a repository that does not exist
            // would be retried at every poll for as long as the daemon lives.
            task.attempts += 1;
            task.fail(format!("config: {e:#}"), opts.max_attempts);
            record(queue, task);
            return;
        }
    };

    let mut runner = match Runner::start(&repo, task.instruction.clone(), config).await {
        Ok(r) => r,
        Err(e) => {
            task.attempts += 1;
            task.fail(format!("could not start the run: {e:#}"), opts.max_attempts);
            record(queue, task);
            return;
        }
    };

    // `start` has minted the run, so the task can now point at it. Persisting
    // `Running` before `execute` is what makes a crash mid-run legible.
    let run = runner.state.id.clone();
    task.start(run.clone());
    record(queue, task);
    lock(status).current = Some(Current {
        task: task.id.clone(),
        run,
    });

    let detail = match runner.execute().await {
        Ok(()) => describe(&runner.state),
        Err(e) => format!("{e:#}"),
    };
    settle(task, runner.state.status, &detail, opts.max_attempts);
    record(queue, task);
    tracing::info!(
        "task {} is {} after run {} ({})",
        task.short(),
        task.status.as_str(),
        runner.state.short(),
        label(runner.state.status)
    );
}

/// Load the config for a task's repository, with the merge override applied.
fn prepare(repo: &Path, opts: &Opts) -> Result<Config> {
    let (mut config, _layers) = Config::discover(repo, opts.config.as_deref())?;
    if let Some(mode) = &opts.merge {
        config.merge.mode = merge_mode(mode)?;
    }
    Ok(config)
}

/// Which repository a task runs in. A task that names none — the normal case
/// for one filed from a phone — runs in the daemon's own default.
fn repo_for(task: &Task, fallback: &Path) -> PathBuf {
    if task.repo.as_os_str().is_empty() || task.repo == Path::new(".") {
        return fallback.to_path_buf();
    }
    task.repo.clone()
}

/// Persist a transition. A queue write failure is logged rather than fatal: the
/// run already happened, and taking the daemon down would only add a lost
/// backlog to a full disk.
fn record(queue: &Queue, task: &mut Task) {
    if let Err(e) = queue.put(task) {
        tracing::error!("could not record task {}: {e:#}", task.short());
    }
}

/// Every runnable task, in the order the loop should try them.
///
/// The head of this list is exactly what [`Queue::next_runnable`] offers; the
/// tail exists so that a claim somebody else holds costs the loop the next
/// candidate rather than a whole poll interval of idleness.
fn runnable(queue: &Queue) -> Vec<Task> {
    let mut tasks: Vec<Task> = queue
        .list()
        .into_iter()
        .filter(|t| t.status.runnable())
        .collect();
    tasks.sort_unstable_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
    tasks
}

/// Why a run ended where it did, in one line, for [`Task::last_error`].
///
/// A stalled run names the seats the quota took out: "out of quota" is not
/// actionable, while "judge-2, judge-3 hit a limit" tells the operator which
/// agent to replace or which plan to top up.
fn describe(state: &RunState) -> String {
    let mut detail = if state.status == RunStatus::Stalled {
        let mut seats: Vec<&str> = state.quota.iter().map(|q| q.seat.as_str()).collect();
        seats.sort_unstable();
        seats.dedup();
        if seats.is_empty() {
            "the judging panel lost its quorum".to_owned()
        } else {
            format!(
                "the judging panel lost its quorum; quota took out {}",
                seats.join(", ")
            )
        }
    } else {
        format!("run ended {}", label(state.status))
    };
    if let Some(last) = state.events.last() {
        detail.push_str(&format!(" ({}: {})", last.node, last.message));
    }
    detail.push_str(&format!(" [run {}]", state.id));
    detail
}

/// Stable lower-case name for a run status, for logs and task errors.
fn label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Prep => "prep",
        RunStatus::Implementing => "implementing",
        RunStatus::Judging => "judging",
        RunStatus::Deliberating => "deliberating",
        RunStatus::Voting => "voting",
        RunStatus::Reviewing => "reviewing",
        RunStatus::Gating => "gating",
        RunStatus::Merged => "merged",
        RunStatus::Ready => "ready",
        RunStatus::Stalled => "stalled",
        RunStatus::Blocked => "blocked",
        RunStatus::Failed => "failed",
    }
}

/// Parse a merge mode override.
fn merge_mode(mode: &str) -> Result<MergeMode> {
    match mode {
        "none" => Ok(MergeMode::None),
        "local" => Ok(MergeMode::Local),
        "pr" => Ok(MergeMode::Pr),
        other => bail!("unknown merge mode `{other}`; expected none, local or pr"),
    }
}

/// Take the status lock, recovering from a poisoned one.
///
/// A panic elsewhere must not silently stop the heartbeat: the status is plain
/// data, and the worst a poisoned lock can hold is a stale timestamp.
fn lock(status: &Mutex<Status>) -> MutexGuard<'_, Status> {
    status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Source, TaskStatus};
    use pretty_assertions::assert_eq;

    fn task() -> Task {
        Task::new(
            "add retries".to_owned(),
            "add retries".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        )
    }

    #[test]
    fn every_run_status_settles_the_task_it_came_from() {
        // run status, resulting task status, attempts still standing after one
        let table = [
            (RunStatus::Merged, TaskStatus::Done, 1),
            (RunStatus::Ready, TaskStatus::Done, 1),
            (RunStatus::Stalled, TaskStatus::Failed, 0),
            (RunStatus::Blocked, TaskStatus::Failed, 1),
            (RunStatus::Failed, TaskStatus::Failed, 1),
            (RunStatus::Prep, TaskStatus::Failed, 1),
            (RunStatus::Implementing, TaskStatus::Failed, 1),
            (RunStatus::Judging, TaskStatus::Failed, 1),
            (RunStatus::Deliberating, TaskStatus::Failed, 1),
            (RunStatus::Voting, TaskStatus::Failed, 1),
            (RunStatus::Reviewing, TaskStatus::Failed, 1),
            (RunStatus::Gating, TaskStatus::Failed, 1),
        ];
        for (run, want, attempts) in table {
            let mut t = task();
            t.start("20260902-000000-aaaa".to_owned());
            settle(&mut t, run, "why", 2);
            assert_eq!(t.status, want, "task status after {}", label(run));
            assert_eq!(t.attempts, attempts, "attempts after {}", label(run));
        }
    }

    #[test]
    fn a_quota_stall_costs_the_task_no_attempt_but_a_block_does() {
        let mut stalled = task();
        stalled.start("20260902-000000-aaaa".to_owned());
        settle(&mut stalled, RunStatus::Stalled, "quota", 1);
        assert_eq!(stalled.attempts, 0);
        assert!(
            stalled.status.runnable(),
            "a machine problem must leave the task in line"
        );

        let mut blocked = task();
        blocked.start("20260902-000000-aaaa".to_owned());
        settle(&mut blocked, RunStatus::Blocked, "findings open", 1);
        assert_eq!(blocked.attempts, 1);
        assert_eq!(
            blocked.status,
            TaskStatus::Held,
            "the last attempt hands the task to a human"
        );
    }

    #[test]
    fn a_held_task_is_never_offered_to_the_loop() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        for (n, priority) in [(1, 0), (2, 5), (3, 5)] {
            let mut t = task();
            t.id = format!("2026090{n}-000000-000{n}");
            t.priority = priority;
            queue.put(&mut t).unwrap();
        }
        let mut held = task();
        held.id = "20260909-000000-9999".to_owned();
        held.priority = 99;
        held.hold();
        queue.put(&mut held).unwrap();

        let order: Vec<String> = runnable(&queue).into_iter().map(|t| t.id).collect();
        assert_eq!(order.len(), 3);
        assert!(!order.contains(&held.id));
        assert_eq!(
            order.first().cloned(),
            queue.next_runnable().map(|t| t.id),
            "the loop's first candidate is exactly what the queue offers"
        );
        assert_eq!(
            order,
            vec![
                "20260902-000000-0002".to_owned(),
                "20260903-000000-0003".to_owned(),
                "20260901-000000-0001".to_owned(),
            ],
            "priority first, then oldest, so nothing starves"
        );
    }

    #[test]
    fn sweep_removes_an_abandoned_lock_and_keeps_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut old = task();
        old.id = "20260101-000000-old0".to_owned();
        queue.put(&mut old).unwrap();
        let mut fresh = task();
        fresh.id = "20260101-000000-new0".to_owned();
        queue.put(&mut fresh).unwrap();

        let abandoned = queue.claim(&old.id).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let live = queue.claim(&fresh.id).unwrap();

        let swept = sweep_stale_claims(&queue, Duration::from_millis(50));
        assert_eq!(swept, vec![old.id.clone()]);
        assert!(
            queue.claim(&old.id).is_ok(),
            "a swept task is claimable again"
        );
        assert!(
            queue.claim(&fresh.id).is_err(),
            "a lock younger than the threshold still protects its task"
        );
        drop((abandoned, live));
    }

    #[test]
    fn an_already_claimed_task_is_skipped_rather_than_failed() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut only = task();
        queue.put(&mut only).unwrap();

        let _elsewhere = queue.claim(&only.id).unwrap();
        let candidates = runnable(&queue);
        assert_eq!(candidates.len(), 1, "the task is still runnable");
        assert!(
            queue.claim(&candidates[0].id).is_err(),
            "the loop cannot take a claim somebody else holds"
        );

        let after = queue.get(&only.id).unwrap();
        assert_eq!(after.status, TaskStatus::Queued);
        assert_eq!(
            after.attempts, 0,
            "losing the race is not an attempt at the task"
        );
        assert_eq!(after.last_error, None);
    }

    #[test]
    fn the_status_file_round_trips_and_its_heartbeat_advances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");

        let mut status = Status::new();
        status.idle = false;
        status.completed = 7;
        status.current = Some(Current {
            task: "20260902-000000-t111".to_owned(),
            run: "20260902-000001-r111".to_owned(),
        });
        write_status_to(&path, &status).unwrap();
        let first: Status = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(first.schema, SCHEMA);
        assert_eq!(first.pid, std::process::id());
        assert!(!first.idle);
        assert_eq!(first.completed, 7);
        assert_eq!(first.current, status.current);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is renamed, not left behind"
        );

        std::thread::sleep(Duration::from_millis(5));
        status.updated_at = Timestamp::now();
        status.polls = 3;
        write_status_to(&path, &status).unwrap();
        let second: Status =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            second.updated_at > first.updated_at,
            "a reader can only detect staleness if the heartbeat moves"
        );
        assert_eq!(
            second.started_at, first.started_at,
            "the start time is not a heartbeat"
        );
        assert_eq!(second.polls, 3);
    }

    #[test]
    fn reading_counts_as_running_only_while_its_heartbeat_is_fresh() {
        let dir = tempfile::tempdir().unwrap();

        assert!(read_status(dir.path()).is_none(), "no file, no daemon");

        let mut status = Status::new();
        status.updated_at = Timestamp::now() - jiff::SignedDuration::from_secs(60);
        write_status_to(&dir.path().join("daemon.json"), &status).unwrap();
        let stale = read_status(dir.path()).unwrap();
        assert!(
            !stale.running(Timestamp::now()),
            "a minute without a heartbeat is a dead daemon, not a busy one"
        );
        assert!(stale.age_secs(Timestamp::now()).is_some_and(|s| s >= 55));

        status.updated_at = Timestamp::now();
        write_status_to(&dir.path().join("daemon.json"), &status).unwrap();
        let fresh = read_status(dir.path()).unwrap();
        assert!(fresh.running(Timestamp::now()));
    }

    #[test]
    fn a_newer_status_file_still_yields_a_reading() {
        let dir = tempfile::tempdir().unwrap();
        // A field this build has never heard of must not turn the reading into
        // nothing at all; that is the whole reason the reader is permissive.
        std::fs::write(
            dir.path().join("daemon.json"),
            serde_json::json!({
                "schema": 2,
                "updated_at": Timestamp::now().to_string(),
                "idle": true,
                "surprise": { "nested": [1, 2, 3] },
            })
            .to_string(),
        )
        .unwrap();

        let reading = read_status(dir.path()).expect("a forward-compatible read");
        assert!(reading.running(Timestamp::now()));
        assert!(reading.idle);
        assert_eq!(reading.current, None);
    }

    #[test]
    fn a_task_without_a_repository_runs_in_the_daemons_default() {
        let fallback = Path::new("/default");
        let mut blank = task();
        blank.repo = PathBuf::new();
        assert_eq!(repo_for(&blank, fallback), PathBuf::from("/default"));
        let mut dot = task();
        dot.repo = PathBuf::from(".");
        assert_eq!(repo_for(&dot, fallback), PathBuf::from("/default"));
        assert_eq!(
            repo_for(&task(), fallback),
            PathBuf::from("/repo"),
            "a task that names a repository keeps it"
        );
    }

    #[test]
    fn merge_overrides_are_parsed_or_refused() {
        assert_eq!(merge_mode("none").unwrap(), MergeMode::None);
        assert_eq!(merge_mode("local").unwrap(), MergeMode::Local);
        assert_eq!(merge_mode("pr").unwrap(), MergeMode::Pr);
        assert!(merge_mode("squash").is_err());
    }
}
