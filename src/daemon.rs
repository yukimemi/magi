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
    clear_status_at(&status_path());
}

/// Delete a status file at an explicit path, so the loop's teardown and
/// [`clear_status`] cannot drift apart: the loop is handed the path it
/// published to, and a test can watch a temp file disappear.
fn clear_status_at(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// A cooperative stop, shared with whoever asked the loop to run.
///
/// Cloning is how the request travels: [`serve_until`] keeps one handle, the
/// Ctrl-C listener and the web UI keep others, and every clone points at the
/// same flag. There is no channel because there is nothing to send — the only
/// message is "stop", it is idempotent, and a flag cannot be missed by a
/// receiver that was not listening yet.
///
/// The handle also answers the question the operator's screen asks next: a
/// stop does not take effect until the run in flight has finished, so
/// [`Stop::finishing`] reports "asked to stop, still working" rather than
/// leaving a caller to infer it from a heartbeat and hope.
#[derive(Debug, Clone, Default)]
pub struct Stop {
    /// Set once, never cleared: a stop is not something an operator takes back
    /// half way through, and a clearable flag would let a start racing a stop
    /// resurrect a loop that is already unwinding.
    stopped: Arc<AtomicBool>,
    /// Whether a run is in flight, so `finishing` can distinguish a stop that
    /// has landed from one that is waiting on `execute`.
    busy: Arc<AtomicBool>,
    /// Wakes the idle wait. Without this a stop would not be seen until the
    /// poll interval elapsed, and an operator tapping stop on a phone would
    /// watch a button do nothing for five seconds.
    wake: Arc<Notify>,
}

impl Stop {
    /// A stop nobody has asked for yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the loop to stop. Idempotent, and safe to call before the loop
    /// starts: the flag is checked before the first poll.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        // `notify_one` rather than `notify_waiters` because the loop may not be
        // parked yet: this stores a permit, so a wait that registers a moment
        // later returns at once instead of sleeping out the whole interval.
        self.wake.notify_one();
    }

    /// Has a stop been asked for?
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Has a stop been asked for that has not taken effect yet, because a run
    /// is still in flight?
    ///
    /// This is the state a screen has to be able to show. A stop never abandons
    /// a run — see [`serve_until`] — so between the tap and the loop's return
    /// there is a window of tens of minutes in which "running" and "stopped"
    /// are both misleading answers.
    #[must_use]
    pub fn finishing(&self) -> bool {
        self.stopped() && self.busy.load(Ordering::SeqCst)
    }

    /// Mark a run as in flight, or finished, for [`Stop::finishing`].
    fn busy(&self, running: bool) {
        self.busy.store(running, Ordering::SeqCst);
    }

    /// Wait out one poll interval, returning early once a stop is asked for.
    async fn idle(&self, poll: Duration) {
        tokio::select! {
            () = tokio::time::sleep(poll) => {}
            () = self.wake.notified() => {}
        }
    }
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

/// What a live daemon is working on right now, or `None`.
///
/// One definition of liveness, because deleting a task and deleting a run are
/// both gated on it from both the CLI and the web UI - four callers that must
/// never disagree about whether the same thing is in flight. A stale heartbeat
/// reads as "no daemon": that is [`Reading::running`]'s judgement, and a task
/// left at `running` or a run left at `implementing` by a killed daemon is a
/// leftover record rather than work in progress.
#[must_use]
pub fn current_work(home: &Path, now: Timestamp) -> Option<Current> {
    read_status(home)
        .filter(|reading| reading.running(now))
        .and_then(|reading| reading.current)
}

/// Whether a live daemon is working on this run at this moment.
#[must_use]
pub fn is_working_on(home: &Path, run: &str, now: Timestamp) -> bool {
    current_work(home, now).is_some_and(|c| c.run == run)
}

/// Whether a live daemon is working on this task at this moment.
#[must_use]
pub fn is_working_on_task(home: &Path, task: &str, now: Timestamp) -> bool {
    current_work(home, now).is_some_and(|c| c.task == task)
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

/// What a finished run tells the queue about the task it came from.
///
/// A struct rather than a fourth and fifth boolean argument: the two flags
/// answer different questions about the same run, and a call site passing
/// `(…, true, false)` is one transposition away from refunding attempts
/// forever.
#[derive(Debug, Clone, Copy)]
pub struct Verdict {
    /// Where the graph stopped.
    pub status: RunStatus,
    /// The run opened a pull request.
    pub left_pr: bool,
    /// At least one seat was lost to a rate limit.
    pub quota_hit: bool,
}

/// Record a finished run against the task it came from.
///
/// Kept pure and separate from the loop because this mapping *is* the retry
/// policy, and a policy that can only be exercised by spawning a graph is a
/// policy nobody checks. The table:
///
/// | run status              | task becomes        | attempt spent |
/// |-------------------------|---------------------|---------------|
/// | `Merged`, `Ready`       | `Done`              | yes           |
/// | `Stalled`, quota hit    | `Failed` (requeued) | **no**        |
/// | `Stalled`, no quota     | `Failed`, or `Held` | yes           |
/// | `Blocked` with a PR     | `Held`              | yes           |
/// | `Blocked`, `Failed`     | `Failed`, or `Held` | yes           |
/// | anything non-terminal   | `Failed`, or `Held` | yes           |
///
/// The two `Stalled` rows are the ones worth reading twice. A quorum lost to
/// rate limits is a property of the machine and not of the task, so the
/// attempt is refunded and a reset quota picks the work up where it stopped.
/// A quorum lost to judges that answered with the wrong shape is ordinary
/// flakiness, and refunding *that* takes the bound off the retry loop
/// entirely: run e633 stalled with `quota: []` after two judges wrote
/// unusable JSON, was refunded, and the next attempt paid for a fresh
/// hour-long implement wave before it could fail the same way. `max_attempts`
/// exists precisely so that cannot repeat forever.
///
/// A non-terminal status means `execute` returned while the graph was still
/// mid-flight, which is a bug rather than a verdict; it is treated as a
/// failure so that a task cannot loop on it either.
///
/// `left_pr` splits the `Blocked` row, and it is the difference between a run
/// that failed and a run that finished into a gate. See [`Task::handed_off`].
pub fn settle(task: &mut Task, verdict: Verdict, detail: &str, max_attempts: usize) {
    match verdict.status {
        RunStatus::Merged | RunStatus::Ready => task.succeed(),
        RunStatus::Stalled if verdict.quota_hit => task.stall(detail),
        RunStatus::Stalled | RunStatus::Failed => task.fail(detail, max_attempts),
        RunStatus::Blocked if verdict.left_pr => task.handed_off(detail),
        RunStatus::Blocked => task.fail(detail, max_attempts),
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
/// A thin wrapper over [`serve_until`] with a stop nothing but Ctrl-C ever
/// sets, so there is one loop body rather than two that drift apart the first
/// time the retry policy changes on only one of them.
pub async fn serve(opts: Opts) -> Result<()> {
    serve_until(opts, Stop::new()).await
}

/// [`serve`], but stopping when `stop` is set as well as on Ctrl-C.
///
/// Neither a signal nor a `stop` abandons a run in flight. Killing the graph
/// mid-node leaves worktrees, branches and agent sessions behind, and every
/// agent call already paid for is lost; finishing the run costs the operator a
/// wait and saves them a cleanup. A stop therefore only sets a flag: the
/// current `execute` runs to its terminal status, the task's outcome is
/// recorded, and only then does the loop return. That window is what
/// [`Stop::finishing`] is for. An operator who genuinely wants the run dead
/// still has a second Ctrl-C, which the runtime turns into a process kill —
/// and the task left `Running` then tells the next daemon, and the next human,
/// where to look.
///
/// While the queue is empty the stop is honoured within one wakeup rather than
/// one poll interval: the wait is a `select!` against [`Stop`]'s notify, so a
/// caller that taps stop does not sit through the remainder of a sleep.
pub async fn serve_until(opts: Opts, stop: Stop) -> Result<()> {
    let signal = {
        let stop = stop.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                stop.stop();
                tracing::info!("shutdown requested; a run in flight will be finished first");
            }
        })
    };

    let outcome = drive(&opts, &Queue::open(), &status_path(), &stop).await;

    signal.abort();
    outcome
}

/// The loop proper: setup, poll, teardown, with the queue and the status file
/// supplied rather than discovered.
///
/// Both are parameters because [`crate::run::home`] is process-global and its
/// override is a `OnceLock`, so a unit test that pinned it would fight every
/// other test in the binary — and a loop that resolved the home itself could
/// only be exercised against the operator's real one, publishing over a live
/// daemon's status file and claiming tasks out of a live backlog.
async fn drive(opts: &Opts, queue: &Queue, status_file: &Path, stop: &Stop) -> Result<()> {
    let swept = sweep_stale_claims(queue, STALE_CLAIM);
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
    write_status_to(status_file, &lock(&status)).context("publish the daemon status file")?;
    let beat = tokio::spawn(heartbeat(Arc::clone(&status), status_file.to_path_buf()));

    tracing::info!(
        "magi serve: queue {} (poll {}s, {} attempts per task, one run at a time)",
        queue.root().display(),
        opts.poll.as_secs(),
        opts.max_attempts
    );

    let outcome = poll(opts, queue, &status, stop).await;

    beat.abort();
    clear_status_at(status_file);
    outcome
}

/// Refresh the status file on a fixed tick.
///
/// Separate from the loop because a run takes tens of minutes: a status file
/// written only between tasks would look stale for the whole of every run, and
/// a reader would report the daemon dead exactly while it was busiest.
async fn heartbeat(status: Arc<Mutex<Status>>, path: PathBuf) {
    loop {
        tokio::time::sleep(HEARTBEAT).await;
        let snapshot = {
            let mut guard = lock(&status);
            guard.updated_at = Timestamp::now();
            guard.clone()
        };
        if let Err(e) = write_status_to(&path, &snapshot) {
            // A failed heartbeat must not take the daemon down: the loop is the
            // product, the status file is only the window onto it.
            tracing::warn!("could not refresh the daemon status file: {e:#}");
        }
    }
}

/// Poll the queue until stopped, factored out so [`drive`] owns only setup and
/// teardown and cannot skip the teardown on an early return.
async fn poll(opts: &Opts, queue: &Queue, status: &Arc<Mutex<Status>>, stop: &Stop) -> Result<()> {
    // Only consulted by `once`, where a task that just failed is still
    // `runnable` and would otherwise be picked up again inside the same drain.
    // In the long-running mode a later poll retrying a failed task is the point,
    // and the attempt counter is what bounds it.
    let mut attempted: Vec<String> = Vec::new();

    while !stop.stopped() {
        lock(status).polls += 1;

        let candidates: Vec<Task> = runnable(queue)
            .into_iter()
            .filter(|t| !opts.once || !attempted.contains(&t.id))
            .collect();

        let mut ran = false;
        for candidate in candidates {
            if stop.stopped() {
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
            // A stop asked for from here on is "finishing", not "stopped": the
            // run gets to reach a terminal status before the loop returns.
            stop.busy(true);
            attempt(opts, queue, status, &mut task).await;
            stop.busy(false);
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
        stop.idle(opts.poll).await;
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
    let verdict = Verdict {
        status: runner.state.status,
        // A run that opened a pull request handed its work over, whatever the
        // gate then decided about merging it.
        left_pr: runner.state.pr.is_some(),
        // Only a rate limit earns the task its attempt back.
        quota_hit: !runner.state.quota.is_empty(),
    };
    settle(task, verdict, &detail, opts.max_attempts);
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
            settle(
                &mut t,
                Verdict {
                    status: run,
                    left_pr: false,
                    quota_hit: matches!(run, RunStatus::Stalled),
                },
                "why",
                2,
            );
            assert_eq!(t.status, want, "task status after {}", label(run));
            assert_eq!(t.attempts, attempts, "attempts after {}", label(run));
        }
    }

    #[test]
    fn a_quota_stall_costs_the_task_no_attempt_but_a_block_does() {
        let mut stalled = task();
        stalled.start("20260902-000000-aaaa".to_owned());
        settle(
            &mut stalled,
            Verdict {
                status: RunStatus::Stalled,
                left_pr: false,
                quota_hit: true,
            },
            "quota",
            1,
        );
        assert_eq!(stalled.attempts, 0);
        assert!(
            stalled.status.runnable(),
            "a machine problem must leave the task in line"
        );

        let mut blocked = task();
        blocked.start("20260902-000000-aaaa".to_owned());
        settle(
            &mut blocked,
            Verdict {
                status: RunStatus::Blocked,
                left_pr: false,
                quota_hit: false,
            },
            "findings open",
            1,
        );
        assert_eq!(blocked.attempts, 1);
        assert_eq!(
            blocked.status,
            TaskStatus::Held,
            "the last attempt hands the task to a human"
        );
    }

    #[test]
    fn a_run_that_opened_a_pull_request_is_never_re_competed() {
        // Attempts to spare: without the pull request this task would go
        // straight back in line and run the whole competition again.
        let mut delivered = task();
        delivered.start("20260903-080619-01c2".to_owned());
        settle(
            &mut delivered,
            Verdict {
                status: RunStatus::Blocked,
                left_pr: true,
                quota_hit: false,
            },
            "no check status",
            4,
        );
        assert_eq!(
            delivered.status,
            TaskStatus::Held,
            "a pull request waiting on CI or a person is not a retryable failure"
        );
        assert!(
            !delivered.status.runnable(),
            "the loop must not pick this task up again"
        );
        assert_eq!(
            delivered.last_error.as_deref(),
            Some("no check status"),
            "the operator needs to be told what the gate was waiting for"
        );

        // The same status without a pull request is a plain failure, and with
        // attempts left it is retried.
        let mut empty_handed = task();
        empty_handed.start("20260903-080619-01c2".to_owned());
        settle(
            &mut empty_handed,
            Verdict {
                status: RunStatus::Blocked,
                left_pr: false,
                quota_hit: false,
            },
            "findings open",
            4,
        );
        assert_eq!(empty_handed.status, TaskStatus::Failed);
        assert!(empty_handed.status.runnable());
    }

    #[test]
    fn only_a_rate_limit_buys_the_task_its_attempt_back() {
        // Run e633: quorum lost because two judges answered with the wrong
        // JSON shape, `quota: []`. Refunding that takes the bound off the
        // retry loop, and each retry pays for a fresh hour-long implement
        // wave before it can fail the same way.
        let mut flaky = task();
        flaky.start("20260903-123023-e633".to_owned());
        settle(
            &mut flaky,
            Verdict {
                status: RunStatus::Stalled,
                left_pr: false,
                quota_hit: false,
            },
            "verdict rests on 1 of 3 judges",
            2,
        );
        assert_eq!(
            flaky.attempts, 1,
            "flakiness spends an attempt, so `max_attempts` still bounds it"
        );
        assert!(flaky.status.runnable(), "and it is still worth retrying");

        // The same status, lost to a rate limit, is the machine's fault.
        let mut limited = task();
        limited.start("20260903-123023-e633".to_owned());
        settle(
            &mut limited,
            Verdict {
                status: RunStatus::Stalled,
                left_pr: false,
                quota_hit: true,
            },
            "judge-2, judge-3 out of quota",
            2,
        );
        assert_eq!(limited.attempts, 0, "a quota window is refunded");
        assert!(limited.status.runnable());

        // And the bound really binds: a task that keeps stalling on flakiness
        // reaches a human instead of running the roster forever.
        let mut worn = task();
        for _ in 0..2 {
            worn.release();
        }
        worn.start("20260903-123023-e633".to_owned());
        worn.attempts = 2;
        settle(
            &mut worn,
            Verdict {
                status: RunStatus::Stalled,
                left_pr: false,
                quota_hit: false,
            },
            "no quorum again",
            2,
        );
        assert_eq!(worn.status, TaskStatus::Held);
        assert!(!worn.status.runnable());
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
    fn only_a_live_daemon_on_this_very_run_counts_as_working_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let now = Timestamp::now();
        let mine = "20260903-080619-01c2";

        assert!(
            !is_working_on(dir.path(), mine, now),
            "no status file means nobody is working on anything"
        );

        let mut status = Status::new();
        status.current = Some(Current {
            task: "20260903-080340-0167".to_owned(),
            run: mine.to_owned(),
        });
        status.updated_at = now;
        write_status_to(&dir.path().join("daemon.json"), &status).unwrap();
        assert!(is_working_on(dir.path(), mine, now));
        assert!(
            !is_working_on(dir.path(), "20260903-105039-3cbf", now),
            "a daemon busy with one run is not working on another"
        );

        // A killed daemon stops writing heartbeats but leaves the file behind
        // naming the run it died in. That run must not be undeletable forever.
        status.updated_at = now - jiff::SignedDuration::from_secs(600);
        write_status_to(&dir.path().join("daemon.json"), &status).unwrap();
        assert!(
            !is_working_on(dir.path(), mine, now),
            "a stale heartbeat is a dead daemon, so its run is a leftover"
        );
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

    /// A loop whose queue lives in a temp tree and whose poll interval is far
    /// longer than the test's patience, so anything that waits out a poll
    /// instead of noticing the stop fails rather than merely being slow.
    fn idle_loop(dir: &Path) -> (Opts, Queue, PathBuf) {
        let opts = Opts {
            poll: Duration::from_secs(30),
            ..Opts::default()
        };
        // The status file goes in a directory that does not exist yet, so its
        // creation is itself evidence the loop published one.
        (
            opts,
            Queue::at(dir.join("queue")),
            dir.join("home").join("daemon.json"),
        )
    }

    #[test]
    fn a_stop_is_idempotent_and_once_set_stays_set() {
        let stop = Stop::new();
        assert!(!stop.stopped());

        stop.stop();
        assert!(stop.stopped());
        stop.stop();
        assert!(stop.stopped(), "a second stop is not a toggle");

        let shared = stop.clone();
        assert!(
            shared.stopped(),
            "a clone is the same stop; that is how the loop and its caller share one"
        );
    }

    #[test]
    fn only_a_stop_with_a_run_in_flight_reads_as_finishing() {
        let stop = Stop::new();
        stop.busy(true);
        assert!(
            !stop.finishing(),
            "a busy loop nobody has asked to stop is just running"
        );

        stop.stop();
        assert!(
            stop.finishing(),
            "a stop asked for mid-run has not landed until the run is settled"
        );

        stop.busy(false);
        assert!(
            !stop.finishing(),
            "once the run is settled the stop has landed and there is nothing to finish"
        );
    }

    #[tokio::test]
    async fn a_loop_already_asked_to_stop_returns_without_waiting_out_a_poll() {
        let dir = tempfile::tempdir().unwrap();
        let (opts, queue, status_file) = idle_loop(dir.path());
        let stop = Stop::new();
        stop.stop();

        let began = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(2),
            drive(&opts, &queue, &status_file, &stop),
        )
        .await
        .expect("a stopped loop must return, not sit out its poll interval")
        .expect("the loop's own setup and teardown must not fail");
        assert!(
            began.elapsed() < opts.poll,
            "returned only after {:?}, which is a poll interval, not a stop",
            began.elapsed()
        );
    }

    #[tokio::test]
    async fn a_stop_while_idle_wakes_the_wait_instead_of_sleeping_it_out() {
        let dir = tempfile::tempdir().unwrap();
        let (opts, queue, status_file) = idle_loop(dir.path());
        let stop = Stop::new();

        // Asked for after the loop is already parked on its empty queue, which
        // is the case an operator tapping stop on a phone actually hits.
        let asker = {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                stop.stop();
            })
        };

        let began = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(2),
            drive(&opts, &queue, &status_file, &stop),
        )
        .await
        .expect("a stop asked for while idle must wake the wait")
        .expect("the loop's own setup and teardown must not fail");
        asker.await.unwrap();
        assert!(
            began.elapsed() < opts.poll,
            "returned only after {:?}, so the stop waited on the sleep",
            began.elapsed()
        );
    }

    #[tokio::test]
    async fn a_stopped_loop_leaves_no_status_file_claiming_it_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let (opts, queue, status_file) = idle_loop(dir.path());
        let home = status_file.parent().unwrap().to_path_buf();
        let stop = Stop::new();
        stop.stop();

        tokio::time::timeout(
            Duration::from_secs(2),
            drive(&opts, &queue, &status_file, &stop),
        )
        .await
        .expect("a stopped loop must return")
        .expect("the loop's own setup and teardown must not fail");

        assert!(
            home.is_dir(),
            "the loop did publish a status file, so its removal is the teardown and not an absence"
        );
        assert!(
            !status_file.exists(),
            "a stopped loop clears its status file"
        );
        assert!(
            read_status(&home).is_none(),
            "a reader must see no daemon at all, not a heartbeat that merely stopped"
        );
    }
}
