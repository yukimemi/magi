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
//! # A crash is legible, and the loop notices on its own
//!
//! The task is written as [`crate::queue::TaskStatus::Running`], with its run
//! id, *before* the graph starts, and is only rewritten once the run reaches a
//! terminal status. A daemon killed mid-run therefore leaves the task
//! `Running` and pointing at the run that was in flight. The alternative —
//! reverting the task to `Queued` on the way out — would hide the abandoned
//! run and re-spend its quota on the next poll.
//!
//! A task left `Running` forever is not the point, though:
//! [`crate::queue::TaskStatus::runnable`] never offers it again, so a daemon
//! that died mid-run would otherwise strand its task for good.
//! [`reclaim_orphaned_running`] runs on every poll and settles exactly the
//! tasks no live process is actually driving — proven by [`Queue::claim`]
//! succeeding rather than by a staleness guess — against whatever their last
//! run actually became, through the same [`settle`] a live finish uses. A run
//! that genuinely cannot be read still holds its task for a human; the run's
//! own report explains how far it got.
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

use crate::ask::{self, Questions};
use crate::clean;
use crate::conduct::Conductor;
use crate::config::{Config, MergeMode};
use crate::graph::Runner;
use crate::land;
use crate::queue::{Queue, Task, TaskStatus};
use crate::run::{QuotaLoss, RunState, RunStatus};
use crate::triage;

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

/// How long a task may sit [`TaskStatus::Running`] with no live daemon's
/// heartbeat naming it before [`crate::conduct`] is shown it as stalled.
///
/// [`reclaim_orphaned_running`] settles most crashes immediately, on every
/// poll, by attempting the task's own claim: a dead pid is proof enough for
/// [`sweep_stale_claims`] to drop the lock the same tick, and the very next
/// claim attempt succeeds. But a lock whose pid cannot be parsed at all — an
/// empty or corrupt `.lock` file — falls back to [`STALE_CLAIM`]'s six-hour
/// age instead, since there is nothing else to check (see
/// [`sweep_stale_claims`]'s own doc). For as long as that lock survives, the
/// claim keeps failing and `reclaim_orphaned_running` correctly leaves the
/// task `running` — see
/// `stalled_tasks_still_reaches_a_task_reclaim_could_not_claim_yet` for
/// exactly this ordering. `stalled_tasks` is what surfaces that task to the
/// conductor well before the mechanical six-hour sweep would, and thirty
/// minutes is comfortably below `STALE_CLAIM` while still being generous
/// enough that a task merely late to publish its first [`HEARTBEAT`] is
/// never mistaken for abandoned.
pub const STALLED_RUNNING: Duration = Duration::from_secs(30 * 60);

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
    /// Every task and run currently in flight. More than one entry means the
    /// loop is driving more than one run at once — see
    /// [`crate::config::Daemon::max_concurrent_runs`]. Empty, not absent, when
    /// nothing is running, so a reader never has to treat "no field" and "an
    /// empty list" as two different kinds of idle.
    pub current: Vec<Current>,
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
            current: Vec::new(),
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
    /// Where the janitor's [`crate::clean::fold_orphaned_worktrees`] and
    /// [`crate::git::worktree_prune`] look for and reclaim worktrees.
    /// `None` resolves to [`crate::run::default_worktree_root`] - the
    /// operator's real `~/wt/<repo>` - the same way a run with no
    /// [`crate::config::Graph::worktree_root`] resolves its own. A caller
    /// that does not own that directory (a test, an embedding that manages
    /// worktrees itself) must set this, or every idle tick reclaims worktrees
    /// out from under whoever actually does.
    pub worktrees_root: Option<PathBuf>,
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
            worktrees_root: None,
        }
    }
}

/// How many runs a plain `usize` from config may drive concurrently, floored
/// at one. A `0` in a config file would otherwise stall the loop entirely -
/// no runnable task could ever start - which is never what an operator who
/// wrote `0` meant.
fn max_concurrent(n: usize) -> usize {
    n.max(1)
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
    /// How many runs are in flight, so `finishing` can distinguish a stop
    /// that has landed from one that is waiting on `execute`. A count, not a
    /// flag, because more than one run can be in flight at once - see
    /// [`crate::config::Daemon::max_concurrent_runs`] - and the last one to
    /// finish is the one that should turn "finishing" off.
    busy: Arc<std::sync::atomic::AtomicUsize>,
    /// Wakes the idle wait. Without this a stop would not be seen until the
    /// poll interval elapsed, and an operator tapping stop on a phone would
    /// watch a button do nothing for five seconds.
    wake: Arc<Notify>,
    /// Handed to the run in flight, so a stop can also mean "park at the next
    /// node boundary" instead of "finish the whole competition first".
    pause: crate::graph::Pause,
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
        self.stopped() && self.busy_now()
    }

    /// Ask the loop to stop *and* the run in flight to park at its next node
    /// boundary.
    ///
    /// The plain [`Stop::stop`] never abandons a run, which is right when the
    /// operator only wants the queue to drain: a competition is tens of
    /// minutes and its worktrees are paid for. But an operator who wants to
    /// replace the binary cannot wait out a run that has an hour left, and
    /// killing the process loses whatever the seats in flight had not written.
    /// Parking costs at most the node in progress and leaves the run
    /// resumable.
    pub fn park(&self) {
        self.pause.park();
        self.stop();
    }

    /// Has a park been asked for?
    #[must_use]
    pub fn parking(&self) -> bool {
        self.pause.parked()
    }

    /// The pause handle to give a runner.
    #[must_use]
    pub fn pause(&self) -> crate::graph::Pause {
        self.pause.clone()
    }

    /// Is any run in flight right now?
    ///
    /// `finishing` answers "a stop is waiting on a run", which is false until
    /// someone asks to stop. An upgrade needs the plain question, because it
    /// is about to be the one asking.
    #[must_use]
    pub fn busy_now(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }

    /// Mark one more run as in flight, for [`Stop::finishing`].
    fn enter(&self) {
        self.busy.fetch_add(1, Ordering::SeqCst);
    }

    /// Mark one run as finished. The last one out is what makes
    /// [`Stop::busy_now`] false again.
    fn exit(&self) {
        self.busy.fetch_sub(1, Ordering::SeqCst);
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
    /// What the daemon is working on. Empty means idle; more than one entry
    /// means more than one run is in flight at once.
    ///
    /// `deserialize_with` rather than the plain derive: a daemon started
    /// before this field became a list is still out there writing the old
    /// shape — a single `{"task":...,"run":...}` object, or its absence —
    /// on every heartbeat until it is restarted, and a live process reading
    /// that file during the rollout must still see it as running rather than
    /// as absent. A bare type change here would fail the whole struct's
    /// deserialization on a type mismatch, defeating the permissiveness this
    /// type exists for.
    #[serde(deserialize_with = "de_current")]
    pub current: Vec<Current>,
    /// Tasks this daemon process has finished.
    pub completed: u64,
    /// Queue polls this daemon process has made.
    pub polls: u64,
}

/// Accept the old single-`Current`-or-absent shape as well as the current
/// list, so a reader never has to know which build wrote the file.
fn de_current<'de, D>(deserializer: D) -> std::result::Result<Vec<Current>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Shape {
        Many(Vec<Current>),
        One(Current),
    }
    Ok(
        Option::<Shape>::deserialize(deserializer)?.map_or_else(Vec::new, |shape| match shape {
            Shape::Many(v) => v,
            Shape::One(c) => vec![c],
        }),
    )
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

/// Every run a live daemon is working on right now.
///
/// One definition of liveness, because deleting a task and deleting a run are
/// both gated on it from both the CLI and the web UI - four callers that must
/// never disagree about whether the same thing is in flight. A stale heartbeat
/// reads as "no daemon": that is [`Reading::running`]'s judgement, and a task
/// left at `running` or a run left at `implementing` by a killed daemon is a
/// leftover record rather than work in progress. More than one entry once
/// [`crate::config::Daemon::max_concurrent_runs`] is more than one - a caller
/// after "the one thing in flight" wants [`is_working_on`] or
/// [`is_working_on_task`], not this directly.
#[must_use]
pub fn current_work(home: &Path, now: Timestamp) -> Vec<Current> {
    read_status(home)
        .filter(|reading| reading.running(now))
        .map(|reading| reading.current)
        .unwrap_or_default()
}

/// Whether a live daemon is working on this run at this moment.
#[must_use]
pub fn is_working_on(home: &Path, run: &str, now: Timestamp) -> bool {
    current_work(home, now).iter().any(|c| c.run == run)
}

/// Whether a live daemon is working on a run whose short id is this one.
///
/// For a worktree that has no run record to compare against at all -
/// [`crate::clean::fold_orphaned_worktrees`]'s whole reason to exist - a full
/// id is not available to hand to [`is_working_on`]. The short id is: a run's
/// worktree bay is named after it (see [`crate::run::RunState::worktree_root`]),
/// and it is exactly the gap between the daemon claiming a task and
/// `RunState::new` saving the first `run.json` that this exists to protect -
/// a run genuinely in flight but invisible to a scan of `runs/`.
#[must_use]
pub fn is_working_on_short(home: &Path, short: &str, now: Timestamp) -> bool {
    current_work(home, now)
        .iter()
        .any(|c| crate::run::short_of(&c.run) == short)
}

/// Whether a live daemon is working on this task at this moment.
#[must_use]
pub fn is_working_on_task(home: &Path, task: &str, now: Timestamp) -> bool {
    current_work(home, now).iter().any(|c| c.task == task)
}

/// Remove claim files whose owner is provably dead, or that have simply
/// outlived `older_than`, and return the task ids swept.
///
/// A daemon killed with `SIGKILL` never runs [`crate::queue::Claim`]'s
/// destructor, and the orphaned `.lock` file would make its task permanently
/// unclaimable — the backlog would stop for good at exactly the task that was
/// in flight when the machine went down.
///
/// The pid recorded in the lock is the authority whenever it can be read at
/// all; age is only a fallback for when it cannot be.
///
/// - **A parseable pid wins outright.** [`crate::proc::pid_alive`] decides,
///   full stop — dead sweeps the lock immediately, regardless of age; alive
///   protects it, regardless of age. This is what lets a lock be reclaimed in
///   seconds instead of waiting out [`STALE_CLAIM`]: a lock made 33 minutes
///   before this daemon even started, next to a `queued` task, no longer has
///   to sit for six hours before anything notices its owner is gone.
/// - **A pid that cannot be parsed at all** — an empty or corrupt lock file —
///   falls back to `older_than`, since there is nothing else to check.
///
/// Age must never override a *positive* liveness confirmation. `sweep`
/// [`poll`]s concurrently with every attempt this daemon itself has spawned —
/// see [`InFlightGuard`] — not only between them the way a single sequential
/// loop once did, so a run that legitimately runs longer than `older_than`
/// (a multi-round review, a long land wait carried across several resumed
/// attempts) still has this very process's own live pid sitting in its own
/// lock file on every later sweep. Deciding by age alone in that case would
/// delete this daemon's own still-valid claim on its own in-flight task,
/// which [`reclaim_orphaned_running`] would then read as abandoned and hand
/// to a second attempt — two `Runner`s writing the same `run.json` and the
/// same worktree at once. `pid_alive` answering "alive" for anything it
/// cannot determine (a live process, a pid this build cannot check, one
/// under another account) is exactly what keeps that path from ever
/// firing on a guess.
///
/// [`STALE_CLAIM`] itself stays large: a helper program missing or its
/// output unreadable must not be license to guess, and the risk of an
/// unparseable lock outliving a genuinely dead owner is bounded by an order
/// of magnitude above any plausible run rather than by a positive check.
///
/// Runs on every poll, not only at startup — a daemon up for days must keep
/// noticing a lock some other, now-dead, daemon left behind just as readily
/// as one it trips over on the way up.
pub fn sweep_stale_claims(queue: &Queue, older_than: Duration) -> Vec<String> {
    sweep_stale_claims_with(queue, older_than, crate::proc::pid_alive)
}

/// [`sweep_stale_claims`] with its process-query boundary supplied by the
/// caller. This keeps the lock policy testable where process listing is
/// unavailable, while production still uses the platform query above.
fn sweep_stale_claims_with<F>(queue: &Queue, older_than: Duration, pid_alive: F) -> Vec<String>
where
    F: Fn(u32) -> bool,
{
    let this_process = std::process::id();
    let mut swept: Vec<String> = std::fs::read_dir(queue.root())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lock"))
        .filter(|p| {
            match std::fs::read_to_string(p)
                .ok()
                .and_then(|body| body.trim().parse::<u32>().ok())
            {
                // This process wrote it and is asking the question right
                // now, so it is definitionally still alive - settled without
                // spawning a helper process at all.
                Some(pid) if pid == this_process => false,
                Some(pid) => !pid_alive(pid),
                None => p
                    .metadata()
                    .and_then(|m| m.modified())
                    .and_then(|t| t.elapsed().map_err(std::io::Error::other))
                    .is_ok_and(|age| age >= older_than),
            }
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

/// Is `task` stalled: [`TaskStatus::Running`], past [`STALLED_RUNNING`], with
/// no live daemon's heartbeat naming it? Deterministic — no model call, and
/// the exact test [`stalled_tasks`] uses to decide what `crate::conduct` is
/// shown.
fn is_stalled(task: &Task, home: &Path, now: Timestamp) -> bool {
    task.status == TaskStatus::Running
        && (now.as_second() - task.updated_at.as_second()) >= STALLED_RUNNING.as_secs() as i64
        && !is_working_on_task(home, &task.id, now)
}

/// Every task [`is_stalled`] right now — "止まったタスク" in
/// `crate::conduct`'s vocabulary.
fn stalled_tasks(queue: &Queue, home: &Path, now: Timestamp) -> Vec<Task> {
    queue
        .list()
        .into_iter()
        .filter(|t| is_stalled(t, home, now))
        .collect()
}

/// Runnable tasks a dependency can still be set on — "runnable なタスク" in
/// `crate::conduct`'s vocabulary. Deliberately `Queued` only, not
/// `Failed`-and-so-also-runnable: a task that already attempted and lost
/// belongs in [`finished_tasks`], where the question is a recovery, not a
/// dependency.
fn queued_tasks(queue: &Queue) -> Vec<Task> {
    queue
        .list()
        .into_iter()
        .filter(|t| t.status == TaskStatus::Queued)
        .collect()
}

/// `Failed`/`Held` tasks nobody has decided a recovery for yet — "終わった
/// タスク" in `crate::conduct`'s vocabulary.
fn finished_tasks(queue: &Queue) -> Vec<Task> {
    queue
        .list()
        .into_iter()
        .filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Held))
        .collect()
}

/// Deterministically resolve `Task::blocked_by`: a dependency task that
/// reached `Done`, or a question that was answered, is removed — no model
/// involved, on every poll. An answered question's content is copied onto
/// the task ([`Task::record_answer`]) before its id is dropped, so it
/// reaches the next `crate::conduct` prompt and the next run's instruction
/// (see [`instruction_for`]) rather than only clearing the block.
///
/// A `blocked_by` id that names neither an existing task nor an existing
/// question - `magi task rm` (or an operator by hand) deleted it while this
/// task was still waiting - is caught before any of that: it can never
/// become `Done` or `Answered`, so the ordinary loop below would otherwise
/// leave the task `blocked` forever with nothing to notice. Such a task is
/// quarantined to a machine hold instead ([`crate::queue::missing_blocker_hold_reason`]),
/// which puts it in front of `crate::triage::run_once`'s own walk the next
/// time it runs - see that module's doc for why the choice is "ask a human",
/// never "assume the missing dependency was satisfied and unblock anyway".
fn resolve_blockers(queue: &Queue, questions: &Questions) {
    for listed in queue.list() {
        if listed.status != TaskStatus::Blocked || listed.blocked_by.is_empty() {
            continue;
        }
        let Ok(_claim) = queue.claim(&listed.id) else {
            continue;
        };
        let Ok(mut task) = queue.get(&listed.id) else {
            continue;
        };
        if task.status != TaskStatus::Blocked {
            continue;
        }
        let missing = crate::queue::missing_blockers(queue, questions, &task.blocked_by);
        if !missing.is_empty() {
            task.hold_machine(Some(crate::queue::missing_blocker_hold_reason(
                &task.blocked_by,
                &missing,
            )));
            record(queue, &mut task);
            continue;
        }
        let mut changed = false;
        for id in task.blocked_by.clone() {
            if let Ok(dep) = queue.get(&id) {
                if dep.status == TaskStatus::Done {
                    task.unblock(&id);
                    changed = true;
                }
                continue;
            }
            if let Ok(q) = questions.get(&id)
                && q.status == ask::QuestionStatus::Answered
            {
                let answer = match &q.answer {
                    Some(ask::Answer::Choice(c) | ask::Answer::Text(c)) => c.clone(),
                    None => String::new(),
                };
                task.record_answer(q.summary.clone(), answer);
                task.unblock(&id);
                changed = true;
            }
        }
        if changed {
            record(queue, &mut task);
        }
    }
}

/// Retire an unanswered conductor question after its task no longer refers to
/// it. Conductor questions use the task id in `Question::run`, so run-based
/// cleanup cannot observe a manual release or completion.
///
/// Restricted to `Question::node == crate::conduct::NODE`: an ordinary run's
/// own question also carries a `run`, and a run id that happens to collide
/// with some task's id is not this loop's business — only a conductor
/// question actually uses the task id that way. One `Questions::list()` scan
/// is taken up front and matched against the in-memory task set, rather than
/// calling `Questions::open_for` (a full disk scan on its own) once per task.
fn reconcile_task_questions(queue: &Queue, questions: &Questions) {
    let tasks = queue.list();
    let by_id: std::collections::BTreeMap<&str, &Task> =
        tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let referenced: std::collections::BTreeSet<&str> = tasks
        .iter()
        .flat_map(|task| task.blocked_by.iter().map(String::as_str))
        .collect();

    for mut question in questions.list() {
        if !question.status.open() || question.node != crate::conduct::NODE {
            continue;
        }
        // Keep questions a task still names, including when the reference
        // moved to a dependent task.
        if referenced.contains(question.id.as_str()) {
            continue;
        }
        let Some(task) = by_id.get(question.run.as_str()) else {
            continue;
        };
        question.abandon(format!(
            "task {} no longer waits for this answer",
            task.short()
        ));
        if let Err(e) = questions.put(&mut question) {
            tracing::warn!(
                "could not retire question {} for task {}: {e:#}",
                question.short(),
                task.short()
            );
        }
    }
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
    /// The run parked at a node boundary because it was asked to.
    pub parked: bool,
    /// The run never produced a single candidate a judge could look at.
    ///
    /// Distinct from `quota_hit`: a run can lose a seat to a rate limit and
    /// still have another candidate worth judging, in which case the loss was
    /// not the reason nothing came of the run. This is `true` only when the
    /// implement wave ended with nothing viable at all.
    pub no_viable_candidates: bool,
}

/// Record a finished run against the task it came from.
///
/// Kept pure and separate from the loop because this mapping *is* the retry
/// policy, and a policy that can only be exercised by spawning a graph is a
/// policy nobody checks. The table:
///
/// | run status                           | task becomes        | attempt spent |
/// |---------------------------------------|---------------------|---------------|
/// | parked at a boundary                  | `Failed` (requeued) | **no**        |
/// | `Merged`, `Ready`                      | `Done`               | yes          |
/// | `Stalled`, quota hit                   | `Failed` (requeued) | **no**        |
/// | `Failed`, quota hit, no viable cand.   | `Failed` (requeued) | **no**        |
/// | `Stalled`, no quota                    | `Failed`, or `Held`  | yes          |
/// | `Blocked` with a PR                    | `Held`               | yes          |
/// | `Blocked`, `Failed` otherwise          | `Failed`, or `Held`  | yes          |
/// | `VerifiedNoop`                          | `Held`               | yes          |
/// | anything non-terminal                  | `Failed`, or `Held`  | yes          |
///
/// The `VerifiedNoop` row is independent of the `Failed`-quota row above it,
/// deliberately: every candidate agreeing there is nothing to write is not a
/// machine fact about a rate limit, it is an unverified claim about the
/// *task* that a human still has to check — see [`RunStatus::VerifiedNoop`]'s
/// own doc and [`Task::handed_off`]. `Held` rather than `Done` on purpose: the
/// claim could be wrong (a misread instruction, a stale check), and closing
/// the task automatically on an implementer's say-so would be the exact
/// failure mode task 391f's own audit was raised to avoid. `attempt spent` is
/// `yes` here for the same reason it is on the `Blocked`-with-a-PR row just
/// above, which settles through the same [`Task::handed_off`]: `Held` is not
/// `Failed`-and-requeued, so nothing retries this task on the same unverified
/// claim regardless of whether the one already-spent attempt is refunded, and
/// [`Task::release`] resets the count to zero anyway the moment a human looks
/// at the evidence and lets it run again.
///
/// The `Stalled`-quota and `Failed`-quota rows are the ones worth reading
/// twice, together. A quorum lost to rate limits is a property of the machine
/// and not of the task, so the attempt is refunded and a reset quota picks
/// the work up where it stopped — and that is just as true when every
/// implement seat lost the same race and `after_implement` bails with nothing
/// to judge, which surfaces as `Failed` rather than `Stalled` but is the same
/// machine fact. The `no_viable_candidates` guard is what keeps that row
/// narrow: a `Failed` run that produced a real candidate which then lost for
/// some other reason still spends the attempt, exactly like the quorum lost
/// to judges that answered with the wrong shape is ordinary flakiness, and
/// refunding *that* takes the bound off the retry loop entirely: run e633
/// stalled with `quota: []` after two judges wrote unusable JSON, was
/// refunded, and the next attempt paid for a fresh hour-long implement wave
/// before it could fail the same way. `max_attempts` exists precisely so
/// that cannot repeat forever.
///
/// A non-terminal status means `execute` returned while the graph was still
/// mid-flight, which is a bug rather than a verdict; it is treated as a
/// failure so that a task cannot loop on it either.
///
/// `left_pr` splits the `Blocked` row, and it is the difference between a run
/// that failed and a run that finished into a gate. See [`Task::handed_off`].
pub fn settle(task: &mut Task, verdict: Verdict, detail: &str, max_attempts: usize) {
    // A parked run is the operator's own doing, and its work is intact on
    // disk. The task goes back in line with its attempt refunded so the next
    // loop resumes the same run - which `one_task` prefers over competing
    // again - and so that swapping the binary a few times cannot exhaust a
    // budget meant for agents that actually misbehaved.
    if verdict.parked {
        task.stall(detail);
        return;
    }
    match verdict.status {
        RunStatus::Merged | RunStatus::Ready => task.succeed(),
        RunStatus::Stalled if verdict.quota_hit => task.stall(detail),
        RunStatus::Failed if verdict.quota_hit && verdict.no_viable_candidates => {
            task.stall(detail)
        }
        RunStatus::Stalled | RunStatus::Failed => task.fail(detail, max_attempts),
        RunStatus::Blocked if verdict.left_pr => task.handed_off(detail),
        RunStatus::Blocked => task.fail(detail, max_attempts),
        RunStatus::VerifiedNoop => task.handed_off(detail),
        other => task.fail(
            format!(
                "the graph stopped at `{}` without reaching a terminal status: {detail}",
                label(other)
            ),
            max_attempts,
        ),
    }
}

/// [`settle`], plus attaching the run's own [`diagnostic`] excerpt once the
/// task ends up held.
///
/// The one place [`attempt`] (a live finish) and [`reclaim`] (recovering one a
/// dead daemon never got back to) share this, so the two cannot drift into
/// disagreeing about which held tasks get a diagnostic.
fn settle_and_diagnose(
    task: &mut Task,
    verdict: Verdict,
    detail: &str,
    max_attempts: usize,
    state: &RunState,
) {
    settle(task, verdict, detail, max_attempts);
    if task.status == TaskStatus::Held {
        task.diagnostic = diagnostic(state);
    }
}

/// Reconcile a task left at [`TaskStatus::Running`] by a daemon that never
/// got back to [`settle`] for it — a crash, a `SIGKILL`, or a run carried on
/// by some other means entirely, like a manual `magi run` resume that
/// finishes the graph outside the queue's bookkeeping.
///
/// Pure and separate from [`reclaim_orphaned_running`] for the same reason
/// `settle` is separate from `attempt`: a task recovered this way must land
/// exactly where a live daemon would have put it — the same policy table,
/// not a second one that quietly drifts from it — and that is only checkable
/// without spawning a real run.
fn reclaim(task: &mut Task, last_run: Option<RunState>, max_attempts: usize) {
    match last_run {
        Some(state) => {
            let verdict = Verdict {
                status: state.status,
                left_pr: state.pr.is_some(),
                quota_hit: !state.quota.is_empty(),
                parked: state.parked,
                no_viable_candidates: state.viable().is_empty(),
            };
            let detail = format!(
                "recovered a `running` task whose daemon never recorded the outcome: {}",
                describe(&state)
            );
            settle_and_diagnose(task, verdict, &detail, max_attempts, &state);
        }
        None => {
            let why = "task was `running` with no live daemon and no readable \
                       run to recover; held for a human to check what happened";
            task.last_error = Some(why.to_owned());
            // The phone shows `hold_reason`, so a task held by the machine
            // says why there too and not only in `last_error`.
            task.hold_machine(Some(why.to_owned()));
        }
    }
}

/// Find every task left at `running` that no live process is actually
/// driving, and settle each one against whatever its last run became.
///
/// # Why a claim is proof, not a guess
///
/// [`poll`] takes a task's [`Queue::claim`] *before* [`Task::start`] writes
/// `running`, and the guard is held for the task's whole time in that status:
/// `attempt` does not return, and the loop does not move past the scope
/// holding the claim, until the run has settled. So a `running` task whose
/// lock is gone cannot have a live owner — this process or any other —
/// without needing a staleness threshold or a pid check the way
/// [`sweep_stale_claims`] does for the narrower case of a lock left next to a
/// task that never got as far as `running` at all. Taking the claim here is
/// the whole test: it either fails, because something really does hold it
/// and the task is left alone, or it succeeds, which is the proof — and it is
/// kept for the rest of the decision so nothing else can start a competing
/// run while this one is being written.
///
/// Called on every poll, not only at startup, for the reason
/// [`sweep_stale_claims`] now is too: a daemon that has been up for days must
/// keep noticing this, not only on the one morning it happened to restart.
fn reclaim_orphaned_running(queue: &Queue, max_attempts: usize) -> Vec<String> {
    let mut reclaimed = Vec::new();
    for listed in queue.list() {
        if listed.status != TaskStatus::Running {
            continue;
        }
        let Ok(_claim) = queue.claim(&listed.id) else {
            continue;
        };
        // Re-read under the claim: a release or an edit landed by a human
        // between the listing above and the claim just taken must not be
        // clobbered by a decision based on the stale copy.
        let Ok(mut task) = queue.get(&listed.id) else {
            continue;
        };
        if task.status != TaskStatus::Running {
            continue;
        }
        let last_run = task.runs.last().and_then(|id| RunState::load(id).ok());
        // `execute` normally abandons a run's own open questions the moment
        // `status` lands somewhere non-resumable (see `graph::Runner::settle_questions`),
        // but a daemon that crashed *inside* that path - mid `land`'s CI wait,
        // say - can leave a `run.json` already at `Merged`/`Ready`/`Failed`
        // with the question still `open`, because the process died before
        // reaching that call. `reclaim` itself stays pure on purpose (see its
        // own doc), so the same cleanup runs here instead, against the run
        // this reclaim is already reading. `settle_run` costs nothing when
        // `execute` already got there first.
        if let Some(state) = &last_run
            && let Err(e) = ask::Questions::open().settle_run(&state.id, state.status)
        {
            tracing::warn!("abandon questions for {}: {e:#}", state.id);
        }
        reclaim(&mut task, last_run, max_attempts);
        record(queue, &mut task);
        reclaimed.push(task.id.clone());
    }
    reclaimed
}

/// Find every run whose `run.json` is provably dead — every seat it still
/// lists as [`crate::run::RunState::active`] has overrun its own timeout, and
/// [`crate::run::RunState::liveness`] reads [`crate::run::Liveness::Dead`],
/// not merely "no daemon claims it" — and fail it, clearing the leftover
/// active seats so the run stops reading as `implementing` (or whichever
/// node) forever.
///
/// [`reclaim_orphaned_running`] settles the *task* a dead daemon left
/// `running`, using whatever `run.json` already says — but nothing in that
/// path, nor in [`reclaim`], ever writes back to the run itself (`reclaim`
/// stays pure on purpose, see its own doc), so a `run.json` a killed process
/// never got back to sits exactly where it was left: `active` full of seats
/// nobody will ever answer for, `status` stuck on whatever node was in
/// flight. `magi show` already tells an operator this in prose (`no live
/// daemon claims this run right now`); this is what makes that fact durable
/// on disk, the same way a task's own `TaskStatus::Running` does not get to
/// stay stuck once nothing is driving it.
///
/// Runs on every poll, not only at startup, for the reason
/// [`sweep_stale_claims`] and [`reclaim_orphaned_running`] already are: a
/// daemon up for days must keep noticing a run some other, now-dead, daemon
/// left behind just as readily as one it trips over on the way up.
///
/// Walks `home.join("runs")` directly and reads each `run.json` on its own,
/// rather than the process-global [`RunState::load`] / [`crate::run::list_ids`] —
/// the same reason [`crate::clean`]'s housekeeping passes take an explicit
/// `runs` directory instead: `home` here is a parameter precisely so a test
/// can point it away from the operator's real history (see [`drive`]'s own
/// doc), and a scan that fell through to the global home anyway would walk
/// whichever directory some *other* process or test pinned into that
/// `OnceLock` first — mutating runs this call was never handed.
fn reclaim_abandoned_runs(home: &Path, now: Timestamp) -> Vec<String> {
    reclaim_abandoned_runs_with(
        home,
        now,
        crate::proc::pid_status,
        crate::proc::process_started_at,
    )
}

/// [`reclaim_abandoned_runs`] with its `driver_pid` liveness/identity queries
/// supplied by the caller — mirrors [`sweep_stale_claims_with`], which exists
/// for the identical reason: this sweep's real damage (wiping a run's active
/// seats and failing it) has to be provable against an injected answer in a
/// test, not just the real process table.
fn reclaim_abandoned_runs_with<F, G>(
    home: &Path,
    now: Timestamp,
    query: F,
    identity: G,
) -> Vec<String>
where
    F: Fn(u32) -> Option<bool>,
    G: Fn(u32) -> Option<String>,
{
    let mut abandoned = Vec::new();
    for entry in std::fs::read_dir(home.join("runs"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let id = entry.file_name().to_string_lossy().into_owned();
        if !crate::run::is_run_id(&id) {
            continue;
        }
        // Unreadable is `clean::fold_due`'s problem, not this one's — see
        // that module's docs for why a run this cannot parse is left alone
        // rather than guessed at. A different schema number is not that: this
        // touches only `status` and `active`, never a field whose meaning a
        // schema bump changed, so an old record's values serve this exactly
        // as well as a current one's (see `clean::read_state`'s own doc for
        // the same reasoning applied to folding).
        let Ok(body) = std::fs::read_to_string(entry.path().join("run.json")) else {
            continue;
        };
        let Ok(mut state) = serde_json::from_str::<RunState>(&body) else {
            continue;
        };
        if state.status.done() || !state.active_all_overrun(now) {
            continue;
        }
        // Not `!is_working_on(..)` alone: that is only "no *daemon* claims
        // it", which is also the normal, healthy shape of a manual `magi
        // run` / `magi review` sharing this same home — this scan walks
        // every run on disk, not only ones this daemon itself started. Such
        // a run's active seats can legitimately sit past their own timeout
        // for a little while (the CLI finishing up, its result still being
        // collected) without the process driving it having died. `liveness`
        // is what actually tells the two apart, by corroborating
        // `driver_pid` against the process it names — see its own doc. Only
        // its strongest, provable answer licenses wiping this run's active
        // seats and failing it out from under whatever is still running it.
        let daemon_claims = is_working_on(home, &id, now);
        if state.liveness_with(daemon_claims, &query, &identity) != crate::run::Liveness::Dead {
            continue;
        }
        state.abandon("daemon");
        if let Err(e) = state.save_under(home) {
            tracing::warn!("could not persist abandoned run {id}: {e:#}");
            continue;
        }
        // The seat that asked is gone for good now, exactly like any other
        // door `graph::Runner::settle_questions` closes the moment `status`
        // lands somewhere non-resumable - see that method's own doc. Nothing
        // else reaches this one before the next `janitor()` startup pass
        // (`clean::abandon_settled_questions`), and a daemon that stays up
        // for days must not leave an open question badging the operator
        // until it happens to restart.
        if let Err(e) = Questions::at(home.join("questions")).settle_run(&id, state.status) {
            tracing::warn!("abandon questions for {id}: {e:#}");
        }
        abandoned.push(id);
    }
    abandoned
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

    let worktrees_root = opts
        .worktrees_root
        .clone()
        .unwrap_or_else(crate::run::default_worktree_root);
    let outcome = drive(
        &opts,
        &Queue::open(),
        &status_path(),
        &crate::run::home(),
        &worktrees_root,
        &stop,
    )
    .await;

    signal.abort();
    outcome
}

/// The loop proper: setup, poll, teardown, with the queue and the status file
/// supplied rather than discovered.
///
/// All three of `home`, `worktrees_root` and the queue/status paths are
/// parameters rather than resolved here, for the same reason:
/// [`crate::run::home`] is process-global and its override is a `OnceLock`,
/// so a unit test that pinned it would fight every other test in the binary,
/// and a loop that resolved its own worktree bay could only be exercised
/// against the operator's real `~/wt/<repo>` - publishing over a live
/// daemon's status file, claiming tasks out of a live backlog, and, since
/// [`janitor`] runs on every idle tick, reclaiming worktrees out from under
/// whatever the operator actually has on disk.
async fn drive(
    opts: &Opts,
    queue: &Queue,
    status_file: &Path,
    home: &Path,
    worktrees_root: &Path,
    stop: &Stop,
) -> Result<()> {
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

    // Read once at startup, not per task: how many runs this loop drives at
    // once is a property of the machine running it, not of whichever
    // repository a given task happens to name - see
    // `Config::daemon.max_concurrent_runs`'s doc for why that is a machine
    // fact in the same sense the agent roster is.
    let daemon_cfg = prepare(&opts.repo, opts)
        .map(|c| c.daemon)
        .unwrap_or_default();
    let concurrency = max_concurrent(daemon_cfg.max_concurrent_runs);

    tracing::info!(
        "magi serve: queue {} (poll {}s, {} attempts per task, {} run(s) at once{})",
        queue.root().display(),
        opts.poll.as_secs(),
        opts.max_attempts,
        concurrency,
        if daemon_cfg.pause_for_interrupts {
            ", interrupts enabled"
        } else {
            ""
        }
    );

    // `--once` drains an already-idle queue without reaching the idle wait,
    // but must still perform the startup cleanup.
    janitor(&opts.repo, opts, home, worktrees_root).await;

    let outcome = poll(
        opts,
        queue,
        &status,
        home,
        worktrees_root,
        stop,
        DispatchLimits {
            max_concurrent: concurrency,
            pause_for_interrupts: daemon_cfg.pause_for_interrupts,
        },
    )
    .await;

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

/// Whether a task's last run is sitting in `land`'s merge-approval wait, and
/// if so, whether that wait is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LandResume {
    /// The task's last run is not parked on a land approval; schedule it
    /// like any other candidate.
    NotLanding,
    /// Parked in `land`, waiting on a question nobody has answered yet.
    /// Left alone: attempting it now would only re-observe the same pull
    /// request and park again, spending a `gh` call on a decision that has
    /// not changed since the last time this was checked.
    StillWaiting,
    /// Parked in `land`, and the question is settled - answered or
    /// abandoned. Resuming this is the one kind of candidate that must not
    /// wait on a free [`Config::daemon`] concurrency slot: see [`poll`].
    Ready,
}

/// Classify a runnable candidate by whether it is parked on a land-merge
/// approval. Read-only - no claim taken, nothing written - so it is cheap
/// enough to call on every candidate, every poll.
fn land_resume_state(task: &Task) -> LandResume {
    let Some(run_id) = task.runs.last() else {
        return LandResume::NotLanding;
    };
    let Ok(state) = RunState::load(run_id) else {
        return LandResume::NotLanding;
    };
    if state.status != RunStatus::Landing || !state.parked {
        return LandResume::NotLanding;
    }
    let store = ask::Questions::open();
    let waiting = store
        .list()
        .into_iter()
        .filter(|q| &q.run == run_id && q.node == land::APPROVAL_NODE)
        .max_by(|a, b| a.id.cmp(&b.id));
    let Some(mut q) = waiting else {
        return LandResume::Ready;
    };
    if !q.status.open() {
        return LandResume::Ready;
    }
    // `ask::ask_and_wait`'s own deadline is what used to retire a question
    // nobody ever answered; land's approval bypasses that wait entirely (see
    // `land::approval_gate`), so the same deadline has to be enforced here
    // instead, or `graph.answer_timeout` silently stops meaning anything for
    // a land approval and a run can sit `StillWaiting` forever with nobody
    // told to look at it.
    let timeout = Duration::from_secs(state.config.graph.answer_timeout);
    let elapsed = Timestamp::now().as_second() - q.asked_at.as_second();
    if elapsed >= 0 && elapsed as u64 >= timeout.as_secs() {
        q.abandon(format!(
            "no answer within {}s of asking",
            timeout.as_secs().max(1)
        ));
        // If this can't be persisted, do not treat the wait as settled on a
        // guess: fall through and try again next poll.
        if store.put(&mut q).is_ok() {
            return LandResume::Ready;
        }
    }
    LandResume::StillWaiting
}

/// How often the loop rechecks for new work while something it already
/// started is still running, rather than sleeping out the whole
/// [`Opts::poll`] interval.
///
/// Short on purpose: this is what lets a land-merge approval that comes back
/// while another task is mid-competition be noticed and resumed within a
/// fraction of a second, not within the next multi-second poll.
const RECHECK_WHILE_BUSY: Duration = Duration::from_millis(200);

/// How often [`poll`] rechecks the shared build cache against its cap at a
/// boundary between runs (see [`maybe_prune_cache_between_runs`]), instead of
/// waiting for the queue to run dry.
///
/// A queue that never empties means the `janitor` call at the bottom of this
/// loop's fully-idle branch can go unreached for as long as the backlog
/// lasts. Five minutes is far below a single gate's own 1200s timeout, so a
/// cache that started the day at its 10 GiB cap cannot grow anywhere near the
/// 81.8 GiB an idle-only check let it reach before this existed, and it is
/// well above the cost of a `dir_size` walk over a multi-gigabyte cache, so a
/// backlog of short tasks does not pay for that walk on every poll.
const CACHE_CHECK_INTERVAL_SECS: u64 = 5 * 60;

/// Frees one attempt's concurrency slot - `Stop`'s busy count and its entry
/// in `Status::current` - on drop, so both are released even if the attempt
/// panics rather than returning.
///
/// A `Drop` impl rather than statements written after the `.await` it
/// guards: a panic unwinds straight past code placed "after" a call, and
/// `Runner::execute`'s chain reaches deep enough into agent-output parsing
/// that ruling a panic out there is not a bet this loop can make. Without
/// this, one panicking run would leave [`Stop::busy_now`] stuck `true`
/// forever - the idle branch in [`poll`], and with it the janitor, would
/// never run again - and a ghost entry in `Status::current` naming a task
/// nothing is still working on.
struct InFlightGuard<'a> {
    status: &'a Arc<Mutex<Status>>,
    stop: &'a Stop,
    task_id: &'a str,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        lock(self.status).current.retain(|c| c.task != self.task_id);
        self.stop.exit();
    }
}

/// State of [`poll`]'s own interrupt-scheduling sequence - see
/// [`crate::config::Daemon::pause_for_interrupts`]. Advanced once per tick by
/// [`advance_interrupt`] and consulted by [`interrupt_gate`], both pure and
/// both kept free of `Task`'s non-identity fields on purpose: every decision
/// here turns only on task ids and which ones are in flight, so the "never
/// more than one run at once" and "exactly one resume" invariants can be
/// pinned down with a plain `#[test]`, no `Runner`, no tokio, no fixture
/// queue - which is exactly the coverage this feature's first two attempts
/// were missing.
///
/// Deliberately in-memory only, not written to disk anywhere: a daemon
/// restart mid-sequence loses track of which run it had asked to park and
/// which task was meant to run first, and simply falls back to `Idle` -
/// see [`drive`]'s own setup. The parked run itself is not lost - it is
/// sitting in the queue exactly like any other resumable, interrupted task,
/// `RunStatus::resumable` and [`Task::interrupt`] both intact on disk - it
/// just resumes on the ordinary priority order rather than guaranteed to go
/// first. Giving that guarantee a crash-proof memory would mean a new queue
/// field and a recovery ordering to go with it, which is exactly the
/// complexity this feature's constraints rule out for the one property that
/// actually matters: at most one run, ever, at once.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Interrupt {
    /// No interrupt sequence in progress. Ordinary dispatch applies.
    Idle,
    /// `interrupt_task` is runnable and exactly one other run is in flight;
    /// `parked` names that one task's id. Dispatch is withheld from
    /// everyone, including `interrupt_task` itself, until it has left
    /// flight - only then is `interrupt_task` let through.
    ///
    /// `parked` is a `Vec` rather than a bare id for symmetry with
    /// [`Interrupt::Running`] and [`Interrupt::Resuming`], but
    /// [`advance_interrupt`]'s own `Idle` branch only ever starts a sequence
    /// when exactly one run is in flight, so it is guaranteed to hold
    /// exactly one entry in practice - see that branch's own doc for why
    /// more than one is deliberately never attempted.
    Parking {
        parked: Vec<String>,
        interrupt_task: String,
    },
    /// `interrupt_task` is dispatched and in flight alone. Dispatch is
    /// withheld from everyone until it leaves flight - merged, failed, held,
    /// it makes no difference - at which point the sequence moves to
    /// [`Interrupt::Resuming`], never straight back to [`Interrupt::Idle`]:
    /// going straight to `Idle` would hand `parked` back to ordinary
    /// priority-order dispatch, where a higher-priority task filed in the
    /// meantime could start ahead of it.
    Running {
        parked: Vec<String>,
        interrupt_task: String,
    },
    /// `interrupt_task` left flight; `parked` still names the one run this
    /// sequence owes a resume. Dispatch is withheld from everyone except
    /// that task - see [`interrupt_gate`] - so the resume this feature
    /// promises is never raced by, or run alongside, an unrelated candidate.
    /// Ends the moment it is seen in flight, or - see `advance_interrupt`'s
    /// own doc on abandonment - the moment it is no longer runnable at all.
    Resuming { parked: Vec<String> },
}

/// One tick of the interrupt scheduler's own state machine. Pure: `in_flight`
/// and `runnable` are read-only snapshots of this tick's reality, and the
/// only side effect the caller still owes the world is asking whichever
/// `Pause` handles `parked` names to actually park - see [`poll`]'s own call
/// site.
///
/// `runnable` only has to carry `id` and `interrupt`; the whole [`Task`] is
/// accepted rather than a narrower type because that is what [`poll`] already
/// has on hand from [`runnable`], and building a second, smaller list on
/// every tick just to satisfy this signature would cost more than it proves.
///
/// Abandonment: [`Interrupt::Parking`] and [`Interrupt::Resuming`] both fall
/// back to a task they are waiting on no longer being [`runnable`] - held,
/// blocked, deleted, or finished by some other means entirely, all of which
/// an operator can do to a task sitting in the queue with no claim on it at
/// all, at any moment, interrupt sequence or not. Without this check the
/// sequence would wait forever for a dispatch that can never come, and
/// `interrupt_gate` would withhold every other task in the queue right along
/// with it - a single `magi task hold` on the wrong id turning into a
/// daemon that never dispatches anything again.
fn advance_interrupt(state: Interrupt, in_flight: &[String], runnable: &[Task]) -> Interrupt {
    match state {
        Interrupt::Idle => {
            // Not just "something to interrupt": exactly one thing. More
            // than one run in flight only happens above the default
            // `max_concurrent_runs = 1`, and `parked` guarantees "exactly
            // one resume, never run alongside anything else" only because
            // it is only ever seeded with exactly one id - see
            // `Interrupt::Resuming`'s own doc on why releasing more than one
            // parked id back to ordinary dispatch cannot be made safe
            // against that same setting's own extra concurrency slots.
            // Waiting here for the herd to settle to one is the
            // simplification this feature's own constraints ask for rather
            // than a second concurrency model to reconcile with the first.
            if in_flight.len() != 1 {
                return Interrupt::Idle;
            }
            match runnable.iter().find(|t| t.interrupt) {
                Some(t) => Interrupt::Parking {
                    parked: in_flight.to_vec(),
                    interrupt_task: t.id.clone(),
                },
                None => Interrupt::Idle,
            }
        }
        Interrupt::Parking {
            parked,
            interrupt_task,
        } => {
            if in_flight.iter().any(|id| parked.contains(id)) {
                // Still waiting for what was in flight to actually stop.
                Interrupt::Parking {
                    parked,
                    interrupt_task,
                }
            } else if in_flight.contains(&interrupt_task) {
                Interrupt::Running {
                    parked,
                    interrupt_task,
                }
            } else if runnable.iter().any(|t| t.id == interrupt_task) {
                // The parked run(s) are gone, but the interrupt task has not
                // been dispatched yet on this tick - `interrupt_gate` is
                // what lets it through next.
                Interrupt::Parking {
                    parked,
                    interrupt_task,
                }
            } else {
                // The interrupt task itself is no longer runnable - see this
                // function's own doc on abandonment. The parked run(s) still
                // get their guaranteed resume; there is simply no interrupt
                // to run ahead of them any longer.
                Interrupt::Resuming { parked }
            }
        }
        Interrupt::Running {
            parked,
            interrupt_task,
        } => {
            if in_flight.contains(&interrupt_task) {
                Interrupt::Running {
                    parked,
                    interrupt_task,
                }
            } else {
                // The interrupt task's own run reached a terminal status,
                // whichever one - this is the *only* trigger that moves the
                // sequence on, driven straight off the same in-flight
                // bookkeeping `poll` already reaps every tick, not a second,
                // independent poll of anything.
                Interrupt::Resuming { parked }
            }
        }
        Interrupt::Resuming { parked } => {
            if in_flight.iter().any(|id| parked.contains(id)) {
                // One of the parked runs has been dispatched - the resume
                // this sequence owed is fulfilled. Whatever else is left in
                // `parked` (ordinarily nothing, at the default concurrency
                // of one) rejoins ordinary priority-order dispatch, same as
                // any other runnable task.
                Interrupt::Idle
            } else if runnable.iter().any(|t| parked.contains(&t.id)) {
                Interrupt::Resuming { parked }
            } else {
                // Abandonment (see this function's own doc): nothing left in
                // `parked` is even runnable any longer.
                Interrupt::Idle
            }
        }
    }
}

/// [`Interrupt`], but with [`crate::config::Daemon::pause_for_interrupts`]
/// folded in: disabled, the sequence can never leave [`Interrupt::Idle`], so
/// a task marked [`Task::interrupt`] on a daemon that has not opted in is
/// indistinguishable from any other runnable task - exactly the "off does
/// nothing" this feature promises.
fn advance_interrupt_tick(
    enabled: bool,
    state: Interrupt,
    in_flight: &[String],
    runnable: &[Task],
) -> Interrupt {
    if !enabled {
        return Interrupt::Idle;
    }
    advance_interrupt(state, in_flight, runnable)
}

/// Which of this tick's runnable candidates the interrupt sequence actually
/// allows to be dispatched. Pure, and separate from [`advance_interrupt`] so
/// each half is assertable on its own: this is the half that keeps a
/// competition and an interrupt from ever running at the same moment.
fn interrupt_gate(state: &Interrupt, in_flight: &[String], candidates: Vec<Task>) -> Vec<Task> {
    match state {
        Interrupt::Idle => candidates,
        Interrupt::Parking {
            parked,
            interrupt_task,
        } => {
            if in_flight.iter().any(|id| parked.contains(id)) {
                Vec::new()
            } else {
                candidates
                    .into_iter()
                    .filter(|t| &t.id == interrupt_task)
                    .collect()
            }
        }
        Interrupt::Running { .. } => Vec::new(),
        // At most one: even if `parked` names more than one id (more than
        // one run was in flight when the sequence began, only possible
        // above the default `max_concurrent_runs = 1`), only the first match
        // is offered. Capping this to a single candidate - not merely to
        // `parked`'s own ids - is what makes "exactly one resume, never two
        // dispatched together" true regardless of how many ordinary slots
        // happen to be free this tick.
        Interrupt::Resuming { parked } => candidates
            .into_iter()
            .find(|t| parked.contains(&t.id))
            .into_iter()
            .collect(),
    }
}

/// The daemon-loop knobs [`poll`] needs from [`crate::config::Daemon`],
/// bundled into one parameter so `poll`'s own signature stays readable -
/// see [`drive`]'s call site for where these are actually read.
struct DispatchLimits {
    /// How many *ordinary* candidates run at once. See
    /// [`crate::config::Daemon::max_concurrent_runs`].
    max_concurrent: usize,
    /// See [`crate::config::Daemon::pause_for_interrupts`].
    pause_for_interrupts: bool,
}

/// Which semaphore, if any, dispatching a candidate should draw its permit
/// from.
///
/// Pure and separate from [`poll`]'s loop body for the same reason
/// [`advance_interrupt`] is: the choice between "skip the ordinary pool
/// entirely", "spend the one urgent slot" and "spend an ordinary slot" is
/// exactly the policy this feature adds, and a policy only exercisable by
/// running the whole loop is a policy nobody checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermitKind {
    /// A land-merge resume (see [`LandResume::Ready`]): bypasses every slot.
    /// Checked first - a task can be both a land resume and marked
    /// [`Task::urgent`], and the resume's own "must not queue behind
    /// anything" guarantee takes precedence.
    None,
    /// [`Task::urgent`]: the one extra slot in `urgent_sem`, spent instead of
    /// (never in addition to trying) the ordinary pool. This is what lets
    /// an urgent task dispatch while every ordinary slot is already checked
    /// out, and what stops a second urgent task from opening a third run: it
    /// waits on this same one-slot semaphore rather than falling through to
    /// the ordinary one.
    Urgent,
    /// The ordinary `max_concurrent_runs` pool - unaffected by either of the
    /// above.
    Ordinary,
}

/// [`PermitKind`] for one candidate. `priority` is [`LandResume::Ready`]'s
/// own boolean, already computed by the caller from [`land_resume_state`].
fn permit_kind(priority: bool, urgent: bool) -> PermitKind {
    if priority {
        PermitKind::None
    } else if urgent {
        PermitKind::Urgent
    } else {
        PermitKind::Ordinary
    }
}

/// Poll the queue until stopped, factored out so [`drive`] owns only setup and
/// teardown and cannot skip the teardown on an early return.
///
/// `limits.max_concurrent` bounds how many *ordinary* candidates run at once,
/// see [`crate::config::Daemon::max_concurrent_runs`]. A run parked on a
/// land approval that has since been answered is dispatched outside that
/// bound the moment [`land_resume_state`] reports it [`LandResume::Ready`]:
/// the whole point of parking there is that it must not queue behind
/// whatever else the loop happens to be running, even at the default of one.
/// Both exemptions are still subject to the interrupt gate below: a
/// land-resume candidate is exactly as much "something else running" as an
/// ordinary one from the interrupt sequence's point of view, and letting it
/// slip through while a run is being parked, or while the interrupt task
/// itself has the floor, is precisely the second run this feature must never
/// produce.
async fn poll(
    opts: &Opts,
    queue: &Queue,
    status: &Arc<Mutex<Status>>,
    home: &Path,
    worktrees_root: &Path,
    stop: &Stop,
    limits: DispatchLimits,
) -> Result<()> {
    let DispatchLimits {
        max_concurrent,
        pause_for_interrupts,
    } = limits;
    // Only consulted by `once`, where a task that just failed is still
    // `runnable` and would otherwise be picked up again inside the same drain.
    // In the long-running mode a later poll retrying a failed task is the point,
    // and the attempt counter is what bounds it.
    let mut attempted: Vec<String> = Vec::new();
    let sem = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    // One extra, permanent slot for `--urgent` tasks (see `Task::urgent`),
    // entirely separate from `sem`: an urgent candidate must dispatch
    // alongside whatever `sem` already has checked out, never by waiting for
    // one of those ordinary slots to free up and never by growing
    // `max_concurrent_runs` itself. Sized at one, not unbounded - see
    // `permit_kind`'s own doc - so a second `--urgent` task queues behind the
    // first on this same slot rather than opening a third run.
    let urgent_sem = Arc::new(tokio::sync::Semaphore::new(1));
    // A quota hit is a fact about the machine, not the task that happened to
    // surface it, and every other *ordinary* candidate is no less likely to
    // hit the same wall - see the warning below. A land-merge resume is
    // exempt: it is a human decision finishing, not a fresh competition, and
    // must not sit out a quota cooldown it did not cause.
    let quota_cooldown_until: Arc<Mutex<Option<Timestamp>>> = Arc::new(Mutex::new(None));
    let mut inflight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    let mut conductor = Conductor::new();
    // See `maybe_prune_cache_between_runs`'s own doc: this is the cache check
    // a congested queue would otherwise starve of the fully-idle branch below.
    let mut cache_last_checked: Option<Timestamp> = None;
    // See `Interrupt`'s own doc: in-memory only, advanced once per tick.
    let mut interrupt = Interrupt::Idle;
    // The `Pause` handed to each dispatched candidate's own `Runner` - see
    // `attempt`'s new parameter - kept here so the tick that decides to park
    // a run for an interrupt can reach that specific run's handle and no
    // other's. Pruned to whatever is still in flight at the top of every
    // tick, so a finished attempt's handle does not linger.
    let mut interrupt_pauses: std::collections::HashMap<String, crate::graph::Pause> =
        std::collections::HashMap::new();

    while !stop.stopped() {
        lock(status).polls += 1;

        // Reap whatever finished since the last tick without blocking on
        // anything still running. `InFlightGuard` already released the slot
        // even if the spawned attempt panicked; this only surfaces that it
        // happened, since a panic swallowed here otherwise leaves no trace.
        while let Some(result) = inflight.try_join_next() {
            if let Err(e) = result {
                tracing::error!("a spawned attempt did not finish cleanly: {e}");
            }
        }

        let swept = sweep_stale_claims(queue, STALE_CLAIM);
        if !swept.is_empty() {
            tracing::warn!(
                "swept {} stale claim(s) left behind by an earlier daemon: {}",
                swept.len(),
                swept.join(", ")
            );
        }
        // Capture stalled work before reclaiming it. A dead daemon's ordinary
        // lock is swept and reclaimed in this same poll, but the conductor
        // must still see that it was stranded rather than only its mechanical
        // terminal state.
        let now = Timestamp::now();

        // No run this daemon spawned is mid-compile right now, whether or
        // not another candidate is about to start - see
        // `maybe_prune_cache_between_runs`'s own doc for why this cannot
        // wait for the queue to run dry.
        if !stop.busy_now() {
            maybe_prune_cache_between_runs(
                &opts.repo,
                opts,
                home,
                stop,
                &mut cache_last_checked,
                now,
            )
            .await;
        }

        let stalled = stalled_tasks(queue, home, now);
        let stalled_ids: std::collections::BTreeSet<_> =
            stalled.iter().map(|task| task.id.clone()).collect();
        let reclaimed = reclaim_orphaned_running(queue, opts.max_attempts);
        if !reclaimed.is_empty() {
            tracing::warn!(
                "reclaimed {} task(s) left `running` by a daemon that never \
                 recorded the outcome: {}",
                reclaimed.len(),
                reclaimed.join(", ")
            );
        }
        let abandoned_runs = reclaim_abandoned_runs(home, now);
        if !abandoned_runs.is_empty() {
            tracing::warn!(
                "failed {} run(s) left behind by a killed process, past every \
                 active seat's own timeout: {}",
                abandoned_runs.len(),
                abandoned_runs.join(", ")
            );
        }

        // `home`, not `ask::Questions::open()`'s own process-global default:
        // `poll` is handed its home explicitly precisely so a test can point
        // it elsewhere, the same reason `Queue::at` and the status file path
        // are parameters rather than resolved here - see `drive`'s own doc.
        let questions = Questions::at(home.join("questions"));

        // Deterministic: no model, run before the conductor sees anything so
        // its input reflects the queue's current, already-resolved state.
        resolve_blockers(queue, &questions);
        reconcile_task_questions(queue, &questions);

        // The conductor gets one look per cycle, right before the loop takes
        // its next task, and only when there is something new to look at -
        // see `Conductor::worth_a_look`'s own doc for why "stalled is
        // non-empty" is the wrong test. Checked before `prepare` so an
        // unchanged cycle never pays for a synchronous config load.
        let finished: Vec<Task> = finished_tasks(queue)
            .into_iter()
            .filter(|task| !stalled_ids.contains(&task.id))
            .collect();
        let queued = queued_tasks(queue);
        // An empty queue has nothing to arrange. In particular, do not let
        // the conductor's initial snapshot cause synchronous config I/O
        // between the caller's stop notification and the idle wait below.
        if !(queued.is_empty() && stalled.is_empty() && finished.is_empty())
            && conductor.worth_a_look(queue, &stalled, &finished)
        {
            match prepare(&opts.repo, opts) {
                Ok(cfg) => {
                    conductor
                        .maybe_run(
                            &cfg,
                            &opts.repo,
                            queue,
                            &questions,
                            home,
                            &queued,
                            &stalled,
                            &finished,
                            opts.max_attempts,
                        )
                        .await;
                }
                Err(e) => tracing::warn!("conductor: no config: {e:#}"),
            }
        }

        let candidates: Vec<Task> = runnable(queue)
            .into_iter()
            .filter(|t| !opts.once || !attempted.contains(&t.id))
            .collect();

        // A task id only stays a key here while its attempt is genuinely in
        // flight; `status.current` is the same liveness fact `InFlightGuard`
        // maintains for the phone's own status file, so this piggybacks on
        // it rather than tracking a second copy of the same thing.
        let in_flight: Vec<String> = lock(status)
            .current
            .iter()
            .map(|c| c.task.clone())
            .collect();
        interrupt_pauses.retain(|id, _| in_flight.contains(id));

        interrupt =
            advance_interrupt_tick(pause_for_interrupts, interrupt, &in_flight, &candidates);
        if let Interrupt::Parking {
            parked,
            interrupt_task,
        } = &interrupt
        {
            let reason = format!(
                "task {} asked to run first",
                crate::run::short_of(interrupt_task)
            );
            for id in parked {
                if let Some(pause) = interrupt_pauses.get(id) {
                    pause.park_because(reason.clone());
                }
            }
        }
        let candidates = interrupt_gate(&interrupt, &in_flight, candidates);

        let cooling_down =
            lock(&quota_cooldown_until).is_some_and(|until| Timestamp::now() < until);

        let mut started_any = false;
        for candidate in candidates {
            if stop.stopped() {
                break;
            }

            let resume = land_resume_state(&candidate);
            if resume == LandResume::StillWaiting {
                continue;
            }
            let priority = resume == LandResume::Ready;

            if !priority && cooling_down {
                continue;
            }
            let permit = match permit_kind(priority, candidate.urgent) {
                PermitKind::None => None,
                PermitKind::Urgent => match Arc::clone(&urgent_sem).try_acquire_owned() {
                    Ok(p) => Some(p),
                    // The one urgent slot is already spoken for by another
                    // `--urgent` task's run. Keep looking rather than falling
                    // back to the ordinary pool - see `PermitKind::Urgent`'s
                    // own doc - a later candidate might still be an ordinary
                    // task with a free slot, or another priority resume.
                    Err(_) => continue,
                },
                PermitKind::Ordinary => match Arc::clone(&sem).try_acquire_owned() {
                    Ok(p) => Some(p),
                    // No ordinary slot free right now. A later candidate in
                    // this same list might still be a priority resume or an
                    // urgent task, so keep looking rather than stopping here.
                    Err(_) => continue,
                },
            };

            // A claim we cannot take means another daemon, or a human running
            // `magi run`, got there first. That is not the task's fault and
            // must not spend one of its attempts: move to the next candidate
            // rather than recording a failure.
            let Ok(claim) = queue.claim(&candidate.id) else {
                tracing::info!("task {} is claimed elsewhere; skipping", candidate.short());
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
            let task_id = task.id.clone();
            attempted.push(task_id.clone());
            lock(status).idle = false;
            // A stop asked for from here on is "finishing", not "stopped": the
            // run gets to reach a terminal status before the loop returns.
            stop.enter();
            started_any = true;

            // A fresh, unshared handle - never `stop.pause()` - so parking
            // this run for an interrupt cannot leak into any other run this
            // loop ever drives. See `Pause`'s own doc.
            let run_pause = crate::graph::Pause::new();
            interrupt_pauses.insert(task_id.clone(), run_pause.clone());

            let opts = opts.clone();
            let queue = queue.clone();
            let status = Arc::clone(status);
            let stop = stop.clone();
            let quota_cooldown_until = Arc::clone(&quota_cooldown_until);
            inflight.spawn(async move {
                // Held for the whole attempt: dropping either at the end of
                // this task is what releases the claim and, for an ordinary
                // candidate, frees its concurrency slot back to the loop.
                let _claim = claim;
                let _permit = permit;
                // See `InFlightGuard`: this must survive a panic inside `attempt`.
                let _inflight = InFlightGuard {
                    status: &status,
                    stop: &stop,
                    task_id: &task_id,
                };
                let quota = attempt(&opts, &queue, &status, &stop, run_pause, &mut task).await;
                lock(&status).completed += 1;
                // A quota loss is a fact about the machine, not this task, and
                // the next ordinary candidate the loop offers is no less
                // likely to hit the same wall: without a cooldown here a
                // whole backlog can be run - and failed - in the seconds it
                // takes each attempt to notice the CLI is out of quota.
                if !quota.is_empty() {
                    let with_hint = quota.iter().find(|q| q.reset.is_some());
                    let hint = with_hint.and_then(|q| q.reset.as_deref());
                    let reset_at = with_hint.and_then(|q| {
                        parse_reset_hint(q.reset.as_deref()?, Timestamp::now(), q.at)
                    });
                    let wait = quota_wait(
                        reset_at,
                        Timestamp::now(),
                        QUOTA_WAIT_FALLBACK,
                        QUOTA_WAIT_CAP,
                    );
                    let secs = i64::try_from(wait.as_secs()).unwrap_or(i64::MAX);
                    let until = Timestamp::now()
                        .checked_add(jiff::SignedDuration::from_secs(secs))
                        .unwrap_or(Timestamp::MAX);
                    *lock(&quota_cooldown_until) = Some(until);
                    match hint {
                        Some(h) => tracing::warn!(
                            "quota hit; waiting {}s before taking another ordinary task \
                             (CLI reported reset: {h})",
                            wait.as_secs()
                        ),
                        None => tracing::warn!(
                            "quota hit; waiting {}s before taking another ordinary task \
                             (no reset hint reported)",
                            wait.as_secs()
                        ),
                    }
                }
            });
        }

        if started_any {
            continue;
        }

        if stop.busy_now() {
            // Something started on an earlier tick is still running. Recheck
            // soon rather than sleeping out the whole poll interval - a freed
            // slot, or a land approval answered mid-run, must not sit idle
            // for it.
            stop.idle(RECHECK_WHILE_BUSY.min(opts.poll)).await;
            continue;
        }

        // Truly idle: nothing new to start and nothing still running.
        lock(status).idle = true;
        if opts.once {
            // A one-shot drain must perform the same post-work cleanup as a
            // daemon that reached a normal idle interval. The startup pass
            // cannot see runs or cache files produced by this drain.
            janitor(&opts.repo, opts, home, worktrees_root).await;
            triage_held(queue, home, opts).await;
            break;
        }
        stop.idle(opts.poll).await;
        if stop.stopped() {
            continue;
        }
        // Housekeeping only after a full quiet interval. Running it before
        // the first idle wait can block the executor while an operator's
        // stop request is waiting to be scheduled, defeating Stop's retained
        // wake permit. No run can start while this branch is active, so the
        // janitor still never races an in-flight compile.
        janitor(&opts.repo, opts, home, worktrees_root).await;
        triage_held(queue, home, opts).await;
    }

    // Never return while a run is still in flight, whichever way the loop
    // above exited: a stop only sets a flag - see `serve_until` - and
    // returning here while `inflight` still holds spawned work would abandon
    // it exactly as a mid-node kill would.
    while let Some(result) = inflight.join_next().await {
        if let Err(e) = result {
            tracing::error!("a spawned attempt did not finish cleanly: {e}");
        }
    }
    Ok(())
}

/// Run one claimed task to a terminal status and record the outcome.
///
/// Every transition is flushed to the queue as it happens, so the state on disk
/// is what actually occurred rather than what this process still intends to
/// write.
async fn attempt(
    opts: &Opts,
    queue: &Queue,
    status: &Arc<Mutex<Status>>,
    stop: &Stop,
    interrupt_pause: crate::graph::Pause,
    task: &mut Task,
) -> Vec<QuotaLoss> {
    let repo = repo_for(task, &opts.repo);
    tracing::info!(
        "task {} — {} (repo {})",
        task.short(),
        task.title,
        repo.display()
    );

    let mut config = match prepare(&repo, opts) {
        Ok(c) => c,
        Err(e) => {
            // A setup failure spends an attempt even though no run was minted.
            // Without that, a task naming a repository that does not exist
            // would be retried at every poll for as long as the daemon lives.
            task.attempts += 1;
            task.fail(format!("config: {e:#}"), opts.max_attempts);
            record(queue, task);
            return Vec::new();
        }
    };
    apply_solo(&mut config, task);

    // The free-space gate, checked *before* anything is minted: a task that
    // waits out a full disk costs nothing yet, and must not spend an attempt
    // or start a run the machine cannot finish. Held tasks stay in the list
    // for the human to see, and `magi task release` re-queues them when space
    // comes back - the same recovery as any other hold. A volume whose free
    // space cannot be measured closes the gate too: starting a run blind on a
    // disk that may be full is how the machine ends up with 6.7 GB free.
    if let Some(reason) = disk_gate(&repo, &config) {
        task.last_error = Some(reason.clone());
        task.hold_machine(Some(reason.clone()));
        record(queue, task);
        tracing::warn!("holding {} for want of disk space: {reason}", task.short());
        return Vec::new();
    }

    // A resumable run of this task is carried on, never re-competed. The
    // candidates are built and paid for, and a fresh competition races a
    // second implementation against them.
    //
    // Two runs paid for that lesson. Run 01c2 was blocked and the loop
    // started 3cbf on the same task a moment later, duplicating two and a
    // half hours of agent work. Then b25f stalled on a judge that timed out
    // and one that answered with no JSON - `quota: 0`, so nothing the machine
    // was to blame for - and 4043 started **one second** later, buying three
    // fresh implementations to reach the same panel. `RunStatus::resumable`
    // rather than `!done()` is what catches the second case: a stall is
    // terminal, and its cheap recovery re-asks only the absent seats.
    //
    // A load failure is warned about rather than silently read as "not
    // resumable": the alternative is exactly what let a schema mismatch on
    // run `eba2` fall through to a full re-competition with nobody told why.
    // `crate::conduct` is what actually offers a better answer than
    // `Runner::start` here (see `Recovery::Review`), once this task's next
    // failure shows it up as `held`/`failed` with the run state unreadable.
    let unfinished = (!task.fresh_start)
        .then(|| unfinished_run(&task.runs, task.short()))
        .flatten();
    // `crate::conduct` chose `Review` for this task on an earlier cycle: its
    // branch survived, and this reopens exactly that branch as a
    // review-only pass rather than resuming or competing again. Consumed
    // (cleared) here whichever way this goes, so it never outlives this one
    // attempt - see `queue::Task::review_branch`.
    let review_branch = task.review_branch.take();
    let branch_exists = match &review_branch {
        Some(branch) => crate::git::branch_exists(&repo, branch)
            .await
            .unwrap_or(false),
        None => false,
    };
    let starter = choose_starter(
        review_branch.as_deref(),
        branch_exists,
        unfinished.as_deref(),
    );
    let started = match &starter {
        Starter::Review(branch) => {
            tracing::info!(
                "task {} reopens `{branch}` as a review-only pass",
                task.short()
            );
            Runner::review(&repo, branch, config).await
        }
        Starter::Resume(id) => {
            tracing::info!("resuming run {id} rather than competing again");
            Runner::resume(id).map(|mut r| {
                if let Some(instruction) =
                    prepare_instruction(&starter, Some(&r.state.instruction), task)
                {
                    r.state.instruction = instruction;
                }
                r
            })
        }
        Starter::Start => {
            if let Some(branch) = &review_branch {
                tracing::warn!(
                    "conductor chose review for task {} but branch `{branch}` no longer \
                     exists; requeuing as a fresh competition instead",
                    task.short()
                );
            }
            let instruction = prepare_instruction(&starter, None, task)
                .unwrap_or_else(|| task.instruction.clone());
            Runner::start(&repo, instruction, config).await
        }
    };
    let mut runner = match started {
        Ok(r) => r,
        Err(e) => {
            task.attempts += 1;
            task.fail(format!("could not start the run: {e:#}"), opts.max_attempts);
            record(queue, task);
            return Vec::new();
        }
    };
    // A stop that means "park" reaches the graph through this handle.
    runner.on_pause(stop.pause());
    // `poll`'s interrupt scheduler reaches this one run - and no other -
    // through this handle. See `Pause`'s own doc for why these are never
    // the same one.
    runner.watch_interrupt(interrupt_pause);

    // `start` has minted the run, so the task can now point at it. Persisting
    // `Running` before `execute` is what makes a crash mid-run legible.
    let run = runner.state.id.clone();
    task.start(run.clone());
    record(queue, task);
    lock(status).current.push(Current {
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
        // A run that parked was asked to stop; that is not a failure and must
        // not spend an attempt, or replacing the binary a few times would
        // exhaust a task's budget without an agent ever misbehaving.
        parked: runner.state.parked,
        // A quota loss that left nothing viable is the same machine fact as a
        // `Stalled` quota loss; see `settle`'s doc table.
        no_viable_candidates: runner.state.viable().is_empty(),
    };
    settle_and_diagnose(task, verdict, &detail, opts.max_attempts, &runner.state);
    record(queue, task);
    tracing::info!(
        "task {} is {} after run {} ({})",
        task.short(),
        task.status.as_str(),
        runner.state.short(),
        label(runner.state.status)
    );
    runner.state.quota
}

/// Cut this attempt's candidate count to one when the task asked to run
/// alone.
///
/// Pure and separate from [`attempt`] so the one thing this feature changes -
/// which `candidates` a `solo` task's run is built with - can be asserted
/// without minting a run: `attempt` drives `graph::Runner`, which spawns real
/// agent CLIs, and no test may do that. `config` is mutated in place, taken by
/// value from the caller's own copy, so a repository's `magi.toml` on disk is
/// never touched - only the `Config` this one attempt hands to `Runner::start`.
fn apply_solo(config: &mut Config, task: &Task) {
    if task.solo {
        config.graph.candidates = 1;
    }
}

/// Load the config for a task's repository, with the merge override applied.
fn prepare(repo: &Path, opts: &Opts) -> Result<Config> {
    let (mut config, _layers) = Config::discover(repo, opts.config.as_deref())?;
    if let Some(mode) = &opts.merge {
        config.merge.mode = merge_mode(mode)?;
    }
    Ok(config)
}

/// Prune the shared build cache back under its cap at a safe boundary
/// between runs, so a queue that never empties - and so never reaches
/// [`poll`]'s fully-idle branch, where the ordinary [`janitor`] pass lives -
/// does not leave the cache to grow unchecked for as long as the backlog
/// lasts.
///
/// Called from [`poll`] only when `stop.busy_now()` is already `false`: the
/// same liveness fact the idle branch's own janitor call rests on - no run
/// this daemon spawned is still mid-compile - so pruning here races nothing.
/// The caller must not call this while a run is in flight; there is no
/// second `busy_now()` check inside this function, on purpose, because there
/// is nothing left to check that `busy_now()` has not already answered.
///
/// A stop that has already been asked for *is* checked here, for a different
/// reason. [`clean::prune_cache_if_over_limit`] walks the whole cache
/// synchronously before it decides anything, so the poll loop cannot get back
/// to its own `stopped()` test until that walk is over — and a loop already
/// on its way out must not make the operator wait out housekeeping it is
/// about to stop needing. This is the same call the idle branch makes when it
/// rechecks `stop.stopped()` after its wait before reaching [`janitor`], and
/// it matters more here: `busy_now()` is false throughout, so
/// [`Stop::finishing`] would report a stop as already landed while the walk
/// still held the loop. Nothing is lost by skipping — the cap is a standing
/// policy, and the next daemon's startup pass measures the same cache.
///
/// Rate-limited by [`CACHE_CHECK_INTERVAL_SECS`] rather than run on every
/// poll: a busy loop reaches this the instant one run's `InFlightGuard` drops
/// and the next has not yet claimed a task, which can be every few
/// milliseconds, and re-walking a multi-gigabyte cache that often would cost
/// more than the growth it is guarding against.
async fn maybe_prune_cache_between_runs(
    repo: &Path,
    opts: &Opts,
    home: &Path,
    stop: &Stop,
    last_checked: &mut Option<Timestamp>,
    now: Timestamp,
) {
    if stop.stopped() || !cache_check_due(*last_checked, now, CACHE_CHECK_INTERVAL_SECS) {
        return;
    }
    *last_checked = Some(now);
    let cfg = match prepare(repo, opts) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("cache check: no config: {e:#}");
            return;
        }
    };
    match clean::prune_cache_if_over_limit(&cfg, home) {
        Ok(Some(pruned)) if pruned.files > 0 => tracing::info!(
            "housekeep: pruned {} file(s) ({} bytes) from the shared cache between runs",
            pruned.files,
            pruned.freed
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!("housekeep: prune cache: {e:#}"),
    }
}

/// Whether [`maybe_prune_cache_between_runs`] should re-measure the cache
/// now, given when it last did (if ever). Pure, so the cadence is asserted
/// directly rather than by waiting out real minutes in a test.
fn cache_check_due(last_checked: Option<Timestamp>, now: Timestamp, interval_secs: u64) -> bool {
    last_checked.is_none_or(|last| clean::due(now, last, interval_secs))
}

/// The disk janitor, with its housekeeping logged rather than fatal.
///
/// Called only at the loop's idle points, for the reason the caller documents:
/// a prune racing a live compile would delete files mid-build. The config is
/// re-read on every call because the repository that just ran may not be the
/// daemon's own default, and the cache directory is a repository fact.
///
/// `home` and `worktrees_root` are parameters rather than [`crate::run::home`]
/// and [`crate::run::default_worktree_root`] read here, for the same reason
/// [`drive`] takes its queue and status file rather than resolving them: a
/// test driving the loop must not reach through to the operator's real home
/// or worktree bay just because the janitor runs on every idle tick.
/// `worktrees_root` staying unread by [`clean::fold_due`] once made this easy
/// to get wrong silently - a test's `home` was already isolated, but nothing
/// exercised the parameter next to it, so a real worktree bay stayed wired in
/// underneath. The moment [`clean::fold_orphaned_worktrees`] started reading
/// it for real, every test in this file that drives the loop at all started
/// sweeping the operator's actual `~/wt/<repo>` instead of a fixture's.
async fn janitor(repo: &Path, opts: &Opts, home: &Path, worktrees_root: &Path) {
    let cfg = match prepare(repo, opts) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("housekeep: no config: {e:#}");
            return;
        }
    };
    // A run's own worktree lives under `config.graph.worktree_root` when the
    // repository sets one - the same precedence `RunState::worktree_root`
    // uses - and `worktrees_root` only stands in for the *default* an
    // unconfigured repository resolves to (see this function's own
    // parameter, or the test fixture wiring one to a fake path). Housekeeping
    // that always swept the default regardless of this override would never
    // see, and so never reclaim, a single worktree for a repository that
    // relocated them elsewhere.
    let worktrees_root = cfg.graph.worktree_root.as_deref().unwrap_or(worktrees_root);
    let out = clean::housekeep(&cfg, home, worktrees_root, repo, Timestamp::now()).await;
    // Reported whenever there is anything to say, not only when `folded > 0`:
    // the incident this exists to prevent was 90 of 93 runs skipped and 0
    // folded, on every single pass, for months - a report gated on `folded`
    // would have stayed silent through every one of them.
    if out.folded > 0 || out.unreadable > 0 || out.orphaned_worktrees > 0 {
        let mut extra = Vec::new();
        if out.unreadable > 0 {
            extra.push(format!("{} unreadable", out.unreadable));
        }
        if out.orphaned_worktrees > 0 {
            extra.push(format!("{} orphaned worktree(s)", out.orphaned_worktrees));
        }
        let detail = if extra.is_empty() {
            String::new()
        } else {
            format!(" ({})", extra.join(", "))
        };
        tracing::info!("housekeep: folded {} run(s){detail}", out.folded);
    }
    if out.cache_files > 0 {
        tracing::info!(
            "housekeep: pruned {} file(s) ({} bytes) from the shared cache",
            out.cache_files,
            out.cache_freed
        );
    }
    if out.questions_abandoned > 0 {
        tracing::info!(
            "housekeep: abandoned {} question(s) left open by a finished run",
            out.questions_abandoned
        );
    }
}

/// Run [`triage::run_once`] and log whatever it did, the same "only when
/// there is something to say" rule [`janitor`] follows for its own report.
///
/// Called at the same idle points as [`janitor`] - once per full poll
/// interval, never mid-attempt - for the same reason: it is not liveness
/// critical, and a task's own `hold_reason` string is the one thing this
/// would otherwise re-check (via [`crate::disk::free_bytes`]) on every busy
/// tick for no benefit.
async fn triage_held(queue: &Queue, home: &Path, opts: &Opts) {
    let questions = Questions::at(home.join("questions"));
    let report = triage::run_once(queue, &questions, opts.config.as_deref(), Timestamp::now());
    if report.is_empty() {
        return;
    }
    if !report.quarantined.is_empty() {
        tracing::info!(
            "triage: held {} blocked task(s) whose blocked-on task or \
             question no longer exists: {}",
            report.quarantined.len(),
            report.quarantined.join(", ")
        );
    }
    if !report.resumed.is_empty() {
        tracing::info!(
            "triage: resumed {} held task(s) whose machine hold had resolved: {}",
            report.resumed.len(),
            report.resumed.join(", ")
        );
    }
    if !report.asked.is_empty() {
        tracing::info!(
            "triage: asked about {} held task(s): {}",
            report.asked.len(),
            report.asked.join(", ")
        );
    }
    if !report.answered.is_empty() {
        tracing::info!(
            "triage: applied {} operator answer(s): {}",
            report.answered.len(),
            report.answered.join(", ")
        );
    }
}

/// The free-space gate: what stands between this task and a new run, if
/// anything. `Some(reason)` holds the task; `None` lets it start.
///
/// A zero [`Config::disk::min_free_bytes`] opens the gate unconditionally -
/// the operator opted out. A measurement failure is a gate, not a pass: both
/// sides of "cannot tell" are served by not starting.
fn disk_gate(repo: &Path, config: &Config) -> Option<String> {
    disk_gate_with(repo, config, crate::disk::free_bytes)
}

/// [`disk_gate`] with its free-space measurement supplied by the caller, so a
/// test can assert the exact wiring `attempt` runs - config's threshold in,
/// task-holding reason out - without asking the real machine's disk anything.
fn disk_gate_with<F: Fn(&Path) -> Result<u64>>(
    repo: &Path,
    config: &Config,
    free_bytes: F,
) -> Option<String> {
    let min = config.disk.min_free_bytes;
    if min == 0 {
        return None;
    }
    match free_bytes(repo) {
        Ok(free) => crate::disk::gate(free, min),
        Err(e) => Some(format!(
            "could not measure free space on {} ({e}); the disk gate refuses \
             to let a run start blind",
            repo.display()
        )),
    }
}

/// How long to wait before offering another task when a run lost a seat to a
/// rate limit and its [`QuotaLoss::reset`] carried no hint [`parse_reset_hint`]
/// could read, or carried nothing at all. Long enough that a quota outage
/// cannot burn through a whole backlog in the few seconds each doomed attempt
/// takes to fail; short enough that a quota which clears early is not left
/// idle for the fallback's sake.
const QUOTA_WAIT_FALLBACK: Duration = Duration::from_secs(5 * 60);

/// Longest a parsed reset hint may push the wait out to. The hint comes from
/// the CLI's own words, not a contract, so a parsing slip that lands a day
/// away must not leave the loop asleep for a day.
const QUOTA_WAIT_CAP: Duration = Duration::from_secs(30 * 60);

/// How long [`poll`] should wait before offering the next task, after a run
/// lost at least one seat to a rate limit.
///
/// Pure and separate from the loop so the policy can be exercised without a
/// real quota outage. `reset_at` is the time [`parse_reset_hint`] made of the
/// CLI's free-text hint, if it could; `fallback` is what to wait when there is
/// nothing to parse, or the parsed time has already passed; `cap` bounds how
/// far a parsed hint is trusted to push the wait out.
fn quota_wait(
    reset_at: Option<Timestamp>,
    now: Timestamp,
    fallback: Duration,
    cap: Duration,
) -> Duration {
    match reset_at {
        Some(at) if at > now => {
            let secs = u64::try_from(at.as_second() - now.as_second()).unwrap_or(0);
            Duration::from_secs(secs).min(cap)
        }
        _ => fallback,
    }
}

/// Best-effort reading of a [`QuotaLoss::reset`] hint into a concrete time.
///
/// `reset` is deliberately free text — see [`crate::agent::Quota`], which
/// explains why parsing it exactly "would be a bug factory" — so this only
/// recognises the shapes actually observed in the wild, and returns `None`
/// for anything else rather than guess at a format nobody has seen.
///
/// `recorded` is when the loss was noted ([`QuotaLoss::at`]). It anchors the
/// relative shape (`"in 1h2m49s"`, agy's), which counts from the moment the CLI
/// said it, not from whenever this loop happens to read it: anchoring on `now`
/// would push the reset later on every read and would read an already-elapsed
/// reset as still in the future. (A long hint is still clamped by
/// [`QUOTA_WAIT_CAP`]; the anchor matters for short hints and elapsed ones.)
fn parse_reset_hint(text: &str, now: Timestamp, recorded: Timestamp) -> Option<Timestamp> {
    parse_reset_hint_zoned(text, now)
        .or_else(|| parse_reset_hint_dated(text))
        .or_else(|| parse_reset_hint_relative(text, recorded))
}

/// agy's shape: `"in 1h2m49s"` - `in`, then hours/minutes/seconds, each unit
/// optional but at least one required, in that order. A bare number or any
/// unknown unit is refused.
fn parse_reset_hint_relative(text: &str, recorded: Timestamp) -> Option<Timestamp> {
    let rest = text.trim().trim_end_matches('.').strip_prefix("in ")?;
    let mut rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total: i64 = 0;
    let mut matched = false;
    for (unit, secs) in [('h', 3600), ('m', 60), ('s', 1)] {
        if let Some((digits, tail)) = rest.split_once(unit)
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            total += digits.parse::<i64>().ok()?.checked_mul(secs)?;
            rest = tail;
            matched = true;
        }
    }
    if !rest.is_empty() || !matched {
        return None;
    }
    recorded
        .checked_add(jiff::SignedDuration::from_secs(total))
        .ok()
}

/// Reads a 12-hour `"H:MMam/pm"` clock reading (whitespace trimmed,
/// case-insensitive) into a 24-hour hour and minute. Shared by every
/// reset-hint shape below.
fn parse_12h_clock(clock: &str) -> Option<(i8, i8)> {
    let clock = clock.trim().to_lowercase();
    let (digits, pm) = clock
        .strip_suffix("am")
        .map(|d| (d, false))
        .or_else(|| clock.strip_suffix("pm").map(|d| (d, true)))?;
    let (h, m) = digits.trim().split_once(':')?;
    let mut hour: i8 = h.trim().parse().ok()?;
    let minute: i8 = m.trim().parse().ok()?;
    if !(1..=12).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    if pm && hour != 12 {
        hour += 12;
    } else if !pm && hour == 12 {
        hour = 0;
    }
    Some((hour, minute))
}

/// The Claude CLI's shape: `"H:MMam/pm (Zone)"`, naming only a clock reading
/// and a zone, never a date. A clock reading already past today is read as
/// tomorrow's: a CLI naming a same-day reset that has already gone by means
/// the window rolled over while nothing was watching.
fn parse_reset_hint_zoned(text: &str, now: Timestamp) -> Option<Timestamp> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let zone = text[open + 1..close].trim();
    let (hour, minute) = parse_12h_clock(&text[..open])?;
    let tz = jiff::tz::TimeZone::get(zone).ok()?;
    let candidate = now
        .to_zoned(tz)
        .with()
        .hour(hour)
        .minute(minute)
        .second(0)
        .millisecond(0)
        .microsecond(0)
        .nanosecond(0)
        .build()
        .ok()?;
    let mut at = candidate.timestamp();
    if at <= now {
        at += jiff::SignedDuration::from_hours(24);
    }
    Some(at)
}

/// The Codex CLI's shape: `"Mon DDth, YYYY H:MMam/pm"` (English month
/// abbreviation, an ordinal day, a 4-digit year, a 12-hour clock reading),
/// with no zone at all — unlike [`parse_reset_hint_zoned`], so there is no
/// "already past today" correction to make: the year already disambiguates
/// it. Scanned as a five-word window so it can be pulled out of the middle
/// of a full sentence, e.g. Codex's actual wording: "...or try again at Sep
/// 19th, 2026 5:10 PM." The result is read as UTC, same as this crate reads
/// any other timestamp with no zone attached.
fn parse_reset_hint_dated(text: &str) -> Option<Timestamp> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 5 {
        return None;
    }
    (0..=words.len() - 5)
        .find_map(|start| parse_dated_window(&words[start..start + 5], words.get(start + 5)))
}

/// One five-word window: month, `"DDth,"`, `"YYYY"`, `"H:MM"`, `"am/pm"`. A
/// parenthesis right after the window is refused rather than ignored — it
/// reads as an explicit zone annotation on a shape that otherwise carries
/// none, and guessing UTC anyway would be exactly the silent misread this
/// module's parsing otherwise avoids.
fn parse_dated_window(window: &[&str], trailing: Option<&&str>) -> Option<Timestamp> {
    if trailing.is_some_and(|next| next.starts_with('(')) {
        return None;
    }
    let month = month_number(window[0])?;
    let day_token = window[1].strip_suffix(',')?.to_lowercase();
    let day_digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| day_token.strip_suffix(*suffix))?;
    let day: i8 = day_digits.parse().ok()?;
    let year_token = window[2];
    if year_token.len() != 4 || !year_token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i16 = year_token.parse().ok()?;
    // The am/pm word carries the sentence's own trailing punctuation, e.g.
    // the period ending "...at Sep 19th, 2026 5:10 PM." — strip it before
    // reusing the same 12-hour clock reader the bracketed shape uses.
    let ampm = window[4].trim_matches(|c: char| !c.is_ascii_alphabetic());
    let (hour, minute) = parse_12h_clock(&format!("{}{}", window[3], ampm))?;
    let date = jiff::civil::Date::new(year, month, day).ok()?;
    let candidate = date
        .at(hour, minute, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()?;
    Some(candidate.timestamp())
}

/// The 3-letter English month abbreviation [`parse_reset_hint_dated`] reads,
/// case-insensitively, into a 1-based month number.
fn month_number(name: &str) -> Option<i8> {
    const NAMES: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = name.to_lowercase();
    NAMES
        .iter()
        .position(|n| *n == lower.as_str())
        .map(|i| i as i8 + 1)
}

/// Resuming a `Blocked` run that already spent every review round its own
/// config allowed cannot make progress: `graph::Runner`'s review loop walks
/// `(reviews.len()+1)..=max_rounds`, which is empty once `reviews.len()` has
/// reached `max_rounds`, so `execute` would settle straight back to
/// `Blocked` without asking anyone anything. Read-only against a state this
/// build never mutates — `src/graph.rs` stays untouched — but without this
/// check, [`unfinished_run`] would keep reporting such a run as still
/// "unfinished", and `crate::conduct::Recovery::Requeue` (whose whole
/// promise is a fresh competition when a design needs to change) would
/// silently resume the exhausted run instead, spending an attempt on a
/// cycle that cannot change anything.
fn exhausted_review_budget(state: &RunState) -> bool {
    state.status == RunStatus::Blocked && state.reviews.len() >= state.config.graph.review_rounds
}

/// This task's *most recent* run, if resuming it would actually make
/// progress. `short` is only for the warning's own message.
///
/// Only ever `runs.last()` — never a search back through older history.
/// `runs` accumulates one entry per fresh `Runner::start`/`Runner::review`
/// mint, oldest first, and every entry before the last one was already
/// superseded at the moment it was minted: the daemon only ever starts a new
/// run when the previous one was not worth resuming (unresumable, exhausted,
/// or unreadable), or when `crate::conduct::Recovery::Review` deliberately
/// opens a fresh review-only run alongside an older, already-failed
/// competition. Searching further back would let an old run that merely
/// *looks* resumable — a `Stalled` competition an earlier `Review` pass left
/// behind, say — get resumed instead of the fresh competition
/// `crate::conduct::Recovery::Requeue` actually promised, reviving history
/// nothing asked to revisit.
///
/// Two runs paid for the "prefer resuming over restarting" half of this
/// lesson, which is why this still checks `runs.last()` rather than always
/// restarting. Run 01c2 was blocked and the loop started 3cbf on the same
/// task a moment later, duplicating two and a half hours of agent work. Then
/// b25f stalled on a judge that timed out and one that answered with no JSON
/// — `quota: 0`, so nothing the machine was to blame for — and 4043 started
/// **one second** later, buying three fresh implementations to reach the
/// same panel. `RunStatus::resumable` rather than `!done()` is what catches
/// the second case: a stall is terminal, and its cheap recovery re-asks only
/// the absent seats. [`exhausted_review_budget`] is the other half: a run
/// that is technically `resumable()` but provably cannot progress must not
/// count as "unfinished" either, or `Recovery::Requeue` becomes a silent
/// no-op instead of the fresh competition it promises.
///
/// A load failure is warned about rather than silently read as "not
/// resumable": the alternative is exactly what let a schema mismatch on run
/// `eba2` fall through to a full re-competition with nobody told why.
/// `crate::conduct` is what actually offers a better answer than
/// `Runner::start` here (see `Recovery::Review`), once this task's next
/// failure shows it up as `held`/`failed` with the run state unreadable.
fn unfinished_run(runs: &[String], short: &str) -> Option<String> {
    unfinished_run_with(runs, short, RunState::load)
}

/// [`unfinished_run`] with an injected state reader. Tests provide their
/// fixtures directly rather than touching the process-global run home.
fn unfinished_run_with<F>(runs: &[String], short: &str, load: F) -> Option<String>
where
    F: FnOnce(&str) -> Result<RunState>,
{
    let id = runs.last()?;
    match load(id) {
        Ok(s) if s.status.resumable() && !exhausted_review_budget(&s) => Some(id.clone()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("could not read run {id} for task {short}: {e:#}");
            None
        }
    }
}

/// Which of the three ways [`attempt`] can mint or continue a run this task
/// should use.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Starter {
    /// `crate::graph::Runner::review` against a branch `crate::conduct` chose
    /// and that still exists.
    Review(String),
    /// `crate::graph::Runner::resume` on an unfinished run of this task.
    Resume(String),
    /// `crate::graph::Runner::start`: a fresh competition.
    Start,
}

/// Decide which of [`Runner::review`], [`Runner::resume`] or [`Runner::start`]
/// this attempt should use. Pure, and separate from [`attempt`], so the
/// routing itself is assertable without spawning a real graph or a git
/// process: `attempt`'s own `crate::git::branch_exists` call has already
/// happened by the time this is called.
///
/// `review_branch` wins whenever `branch_exists` confirms it; a `review_branch`
/// whose branch is gone falls all the way through to [`Starter::Start`], not
/// to [`Starter::Resume`] — `crate::conduct` chose review over resuming the
/// old (likely `Blocked`) run in the first place, and a branch that vanished
/// out from under that choice is not evidence resuming it would fare better.
fn choose_starter(
    review_branch: Option<&str>,
    branch_exists: bool,
    unfinished: Option<&str>,
) -> Starter {
    match review_branch {
        Some(branch) if branch_exists => Starter::Review(branch.to_owned()),
        Some(_) => Starter::Start,
        None => match unfinished {
            Some(id) => Starter::Resume(id.to_owned()),
            None => Starter::Start,
        },
    }
}

/// Which repository a task runs in. A task that names none — the normal case
/// for one filed from a phone — runs in the daemon's own default.
fn repo_for(task: &Task, fallback: &Path) -> PathBuf {
    if task.repo.as_os_str().is_empty() || task.repo == Path::new(".") {
        return fallback.to_path_buf();
    }
    task.repo.clone()
}

/// The header [`append_answers`] appends operator answers under. Shared with
/// [`strip_answers_block`] so a resumed run's instruction can be refreshed
/// rather than grown a new block on every resume.
const ANSWERS_HEADER: &str = "\n\n# Operator answers\n\n";

/// Render the first `count` answers in the block appended to an instruction.
fn answers_block(task: &Task, count: usize) -> String {
    let mut s = ANSWERS_HEADER.to_owned();
    for a in &task.answers[..count] {
        s.push_str(&format!("- {}: {}\n", a.question, a.answer));
    }
    s
}

/// Append every answer `crate::conduct` has collected for `task` onto `base`,
/// in the shape both [`instruction_for`] and [`resumed_instruction`] use.
fn append_answers(base: &str, task: &Task) -> String {
    if task.answers.is_empty() {
        return base.to_owned();
    }
    let mut s = base.to_owned();
    s.push_str(&answers_block(task, task.answers.len()));
    s
}

/// Drop the prior answer block only when it is exactly the suffix this task
/// could have appended on an earlier resume. An `ANSWERS_HEADER` written by
/// the task author is ordinary instruction text, not a block to remove.
fn strip_answers_block<'a>(instruction: &'a str, task: &Task) -> &'a str {
    for count in (1..=task.answers.len()).rev() {
        let block = answers_block(task, count);
        if let Some(base) = instruction.strip_suffix(&block) {
            return base;
        }
    }
    instruction
}

/// The instruction handed to `Runner::start`: the task's own text, plus any
/// operator answers `crate::conduct` collected for it (see
/// [`Task::answers`]), so a decision the operator actually made reaches the
/// implementers rather than only clearing the block that was waiting on it.
///
/// Appended rather than merged into [`Task::instruction`] itself, so the
/// task's own record stays exactly what its author wrote.
fn instruction_for(task: &Task) -> String {
    append_answers(&task.instruction, task)
}

/// The instruction a resumed run should carry on with: whatever it already
/// had, refreshed with the task's *current* operator answers.
///
/// A resumable run's own `RunState::instruction` predates any answer
/// `crate::conduct` collects after the run parks, so resuming it unchanged —
/// the behaviour before this function existed — silently drops the very
/// decision the operator made to unblock it. Re-stripping any block this
/// function appended on an earlier resume before re-appending the current
/// list (rather than blindly appending again) is what keeps a task resumed
/// three times over three answered questions from carrying the same answer
/// three times.
fn resumed_instruction(old_instruction: &str, task: &Task) -> String {
    append_answers(strip_answers_block(old_instruction, task), task)
}

/// What [`attempt`] should tell a [`Starter`] about `task`'s current operator
/// answers before handing it to `Runner` — the actual boundary between
/// [`choose_starter`]'s routing and the graph, factored out so it is
/// assertable without a real repository, git branch, or agent CLI.
///
/// `Starter::Review` deliberately answers `None`: `Runner::review` builds its
/// instruction from the reviewed branch's own commit log because there is no
/// task statement to speak of for hand-written work, and splicing operator
/// answers into that text would contradict the very message it sends
/// reviewers ("there is no task statement").
fn prepare_instruction(
    starter: &Starter,
    old_instruction: Option<&str>,
    task: &Task,
) -> Option<String> {
    match starter {
        Starter::Start => Some(instruction_for(task)),
        Starter::Resume(_) => Some(resumed_instruction(
            old_instruction.expect("a resumed run always has a prior instruction"),
            task,
        )),
        Starter::Review(_) => None,
    }
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
///
/// Uses [`RunStatus::display_label`] rather than [`label`]/`as_str` on
/// purpose: unlike `label`'s other callers (an internal log line, an
/// already-a-bug fallback message), this string becomes `Task::last_error`
/// verbatim, which the phone renders in the same alarm-styled box an
/// ordinary failure gets — see `web::tests` and `assets/ui/app.js`'s
/// `.err` styling. A bare `verified_noop` there would read exactly like the
/// failure this whole feature exists to tell apart from one.
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
        format!("run ended {}", state.status.display_label())
    };
    if let Some(last) = state.events.last() {
        detail.push_str(&format!(" ({}: {})", last.node, last.message));
    }
    detail.push_str(&format!(" [run {}]", state.id));
    detail
}

/// Upper bound on [`Task::diagnostic`]'s length, in bytes.
///
/// The task file lives in the backlog indefinitely; a diagnostic is an
/// excerpt of the run's own `artifacts/`, not a copy of them, so this has to
/// stay small regardless of how much a gate command or a candidate printed.
const DIAGNOSTIC_MAX: usize = 4_000;

/// Tail kept from a single failing command's output inside a diagnostic.
/// Smaller than [`crate::graph`]'s own `OUTPUT_TAIL` on purpose: this is a
/// pointer for a human deciding whether to go read the full artifact by hand,
/// not a replacement for reading it.
const DIAGNOSTIC_OUTPUT_TAIL: usize = 800;

/// Assemble a bounded diagnostic excerpt from a held task's own run, so
/// `magi task show` says more than the one-line reason in [`describe`].
///
/// The one-liner answers "where did the run stop"; this answers "what would a
/// human have found opening `artifacts/` by hand" — the point of the whole
/// feature is the case that one-liner actively misleads on: a run held as "no
/// candidate produced a change" can mean the implementer actually finished
/// the task (opened a PR, merged it, tagged a release) and only left a clean
/// local worktree behind, which reads as "nothing happened" unless someone
/// goes and reads what the agent actually said. `None` when the run carries
/// none of the three shapes this recognises — an ordinary run held for
/// something not diagnosable from `RunState` alone still explains itself
/// through `Task::last_error`.
fn diagnostic(state: &RunState) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    // Gate failure: which check(s), and the tail of what each printed.
    for o in state.gate.iter().filter(|o| !o.ok()) {
        parts.push(format!(
            "gate `{}` failed ({:?}):\n{}",
            o.command,
            o.code,
            crate::run::tail(&o.output_tail, DIAGNOSTIC_OUTPUT_TAIL)
        ));
    }

    // The land loop gave up because the fixer declined while checks were
    // still red: the message already names them (see `land::run`).
    if let Some(last) = state
        .events
        .iter()
        .rev()
        .find(|e| e.node == "land" && e.message.contains("fixer produced no commit"))
    {
        parts.push(last.message.clone());
    }

    // No viable candidate: every implementer's own final word, sanitized the
    // same way a judge would have read it, so a run that actually finished
    // the job does not read as an unexplained failure. A verified no-op is
    // called out ahead of its own summary and apart from an ordinary
    // failure's `why` — this is the one candidate shape whose diagnostic a
    // human is expected to actually judge, not just skim.
    if state.viable().is_empty() {
        for c in &state.candidates {
            if let Some(evidence) = &c.verified_noop {
                parts.push(format!(
                    "candidate {} (agent-verified no-op, unconfirmed by magi): {evidence}",
                    c.label
                ));
            } else if !c.summary.trim().is_empty() {
                parts.push(format!("candidate {}: {}", c.label, c.summary.trim()));
            } else if let Some(why) = &c.failed {
                parts.push(format!("candidate {}: {why}", c.label));
            }
        }
    }

    if parts.is_empty() {
        return None;
    }
    // `run::tail` prefixes an "N earlier bytes omitted" marker whose own
    // length depends on N, so asking it for exactly `DIAGNOSTIC_MAX` can come
    // back slightly over. Leave it enough room to always land under the
    // limit.
    Some(crate::run::tail(
        &parts.join("\n\n"),
        DIAGNOSTIC_MAX.saturating_sub(100),
    ))
}

/// Stable lower-case name for a run status, for an internal log line and the
/// "graph stopped without reaching a terminal status" bug message in
/// [`settle`] — never for [`Task::last_error`] itself; see [`describe`]'s own
/// doc for why that one reads [`RunStatus::display_label`] instead. One
/// definition of a status's name, on the type that owns it: this table used
/// to live here as a second copy, and a status renamed in one place would
/// have gone on reading correctly in the other.
fn label(status: RunStatus) -> &'static str {
    status.as_str()
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
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Source, TaskStatus};
    use crate::run::{Candidate, CommandOutcome};
    use pretty_assertions::assert_eq;

    fn task() -> Task {
        Task::new(
            "add retries".to_owned(),
            "add retries".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        )
    }

    /// A runnable task marked to interrupt, with an id fixed for assertions
    /// rather than the random one [`Task::new`] mints.
    fn interrupt_task(id: &str) -> Task {
        let mut t = task();
        t.id = id.to_owned();
        t.interrupt = true;
        t
    }

    /// An ordinary runnable task with an id fixed for assertions.
    fn task_with_id(id: &str) -> Task {
        let mut t = task();
        t.id = id.to_owned();
        t
    }

    /// The land-resume exemption wins outright, whether or not the candidate
    /// also happens to be marked [`Task::urgent`]: a resume's own "must not
    /// queue behind anything" guarantee cannot be weaker just because the
    /// same task was also filed with `--urgent`.
    #[test]
    fn permit_kind_prefers_a_land_resume_over_the_urgent_slot() {
        assert_eq!(permit_kind(true, false), PermitKind::None);
        assert_eq!(permit_kind(true, true), PermitKind::None);
    }

    /// The one property this whole feature exists for: `--urgent` draws from
    /// its own slot, never the ordinary `max_concurrent_runs` pool - and an
    /// ordinary candidate draws from the ordinary pool exactly as before,
    /// untouched by the urgent slot's existence.
    #[test]
    fn permit_kind_separates_urgent_from_ordinary() {
        assert_eq!(permit_kind(false, true), PermitKind::Urgent);
        assert_eq!(permit_kind(false, false), PermitKind::Ordinary);
    }

    /// The exact wiring `attempt` runs before minting anything: a config's
    /// `min_free_bytes` in, a task-holding reason naming both numbers out.
    /// Free space is injected rather than asked of the real disk - the point
    /// of [`disk_gate_with`] existing separately from [`disk_gate`] - so this
    /// is deterministic on every machine this test runs on, never dependent
    /// on how full the CI runner's own disk happens to be.
    #[test]
    fn disk_gate_with_holds_a_task_below_the_threshold_and_names_both_numbers() {
        let cfg = Config::default();
        let repo = Path::new("/any/repo/path");

        let reason =
            disk_gate_with(repo, &cfg, |_| Ok(1024)).expect("must hold below the threshold");
        assert!(reason.contains("1024"), "{reason}");
        assert!(
            reason.contains(&cfg.disk.min_free_bytes.to_string()),
            "{reason}"
        );

        assert_eq!(
            disk_gate_with(repo, &cfg, |_| Ok(cfg.disk.min_free_bytes)),
            None,
            "exactly at the floor is open"
        );
        assert_eq!(
            disk_gate_with(repo, &cfg, |_| Ok(cfg.disk.min_free_bytes + 1)),
            None,
            "comfortably above the floor is open"
        );
    }

    #[test]
    fn disk_gate_with_opens_unconditionally_when_the_operator_opted_out() {
        let mut cfg = Config::default();
        cfg.disk.min_free_bytes = 0;
        let repo = Path::new("/any/repo/path");
        assert_eq!(
            disk_gate_with(repo, &cfg, |_| Ok(0)),
            None,
            "a zero floor never measures at all"
        );
    }

    #[test]
    fn disk_gate_with_closes_rather_than_starts_blind_when_it_cannot_measure() {
        let cfg = Config::default();
        let repo = Path::new("/any/repo/path");
        let reason = disk_gate_with(repo, &cfg, |_| Err(anyhow::anyhow!("no df on this box")))
            .expect("a measurement failure must close the gate, not open it");
        assert!(reason.contains("could not measure"), "{reason}");
    }

    #[test]
    fn no_interrupt_task_leaves_the_sequence_idle_even_with_something_in_flight() {
        let ordinary = task();
        let next = advance_interrupt(
            Interrupt::Idle,
            std::slice::from_ref(&ordinary.id),
            std::slice::from_ref(&ordinary),
        );
        assert_eq!(next, Interrupt::Idle);
    }

    #[test]
    fn an_interrupt_task_with_nothing_in_flight_never_starts_a_sequence() {
        // Nothing to interrupt - this is just an ordinary candidate, and the
        // loop's normal dispatch will pick it up like any other.
        let marked = interrupt_task("marked");
        let next = advance_interrupt(Interrupt::Idle, &[], std::slice::from_ref(&marked));
        assert_eq!(next, Interrupt::Idle);
    }

    #[test]
    fn an_interrupt_task_with_something_in_flight_starts_parking_it() {
        let marked = interrupt_task("marked");
        let next = advance_interrupt(
            Interrupt::Idle,
            &["running".to_owned()],
            std::slice::from_ref(&marked),
        );
        assert_eq!(
            next,
            Interrupt::Parking {
                parked: vec!["running".to_owned()],
                interrupt_task: "marked".to_owned(),
            }
        );
    }

    /// R1-1-2 / R2-1-2: above the default `max_concurrent_runs`, more than
    /// one run can be in flight when a task becomes runnable and marked.
    /// Parking all of them would mean `Resuming` later has more than one id
    /// to release back to ordinary dispatch, which cannot be made safe
    /// against that same setting's own extra concurrency slots letting two
    /// of them start together - see `advance_interrupt`'s own `Idle` branch.
    /// The simplification the task's own constraints ask for: do not begin
    /// a sequence at all until the herd settles back to exactly one.
    #[test]
    fn more_than_one_run_in_flight_never_starts_an_interrupt_sequence() {
        let marked = interrupt_task("marked");

        let two = advance_interrupt(
            Interrupt::Idle,
            &["a".to_owned(), "b".to_owned()],
            std::slice::from_ref(&marked),
        );
        assert_eq!(two, Interrupt::Idle);

        let none = advance_interrupt(Interrupt::Idle, &[], std::slice::from_ref(&marked));
        assert_eq!(none, Interrupt::Idle, "nothing to interrupt either");
    }

    #[test]
    fn parking_holds_until_every_parked_id_has_actually_left_flight() {
        let state = Interrupt::Parking {
            parked: vec!["running".to_owned()],
            interrupt_task: "marked".to_owned(),
        };
        // Still in flight: no change.
        let still_going = advance_interrupt(state.clone(), &["running".to_owned()], &[]);
        assert_eq!(still_going, state);

        // Left flight, but the interrupt task has not been dispatched yet on
        // this tick - stays `Parking` so `interrupt_gate` can let it through,
        // as long as it is still runnable.
        let stopped_but_not_yet_dispatched =
            advance_interrupt(state.clone(), &[], &[interrupt_task("marked")]);
        assert_eq!(stopped_but_not_yet_dispatched, state);

        // Left flight, and the interrupt task is now in flight itself.
        let dispatched = advance_interrupt(state, &["marked".to_owned()], &[]);
        assert_eq!(
            dispatched,
            Interrupt::Running {
                parked: vec!["running".to_owned()],
                interrupt_task: "marked".to_owned(),
            }
        );
    }

    #[test]
    fn the_sequence_moves_to_resuming_the_instant_the_interrupt_tasks_own_run_leaves_flight() {
        let state = Interrupt::Running {
            parked: vec!["running".to_owned()],
            interrupt_task: "marked".to_owned(),
        };
        let still_running = advance_interrupt(state.clone(), &["marked".to_owned()], &[]);
        assert_eq!(still_running, state);

        // Whatever it ended as - merged, failed, held - is not this
        // function's concern: leaving flight is the only trigger, driven
        // straight off the same in-flight list `poll` already reaps. It does
        // not go straight to `Idle`: see `Interrupt::Running`'s own doc for
        // why that would let an unrelated task start ahead of, or alongside,
        // the guaranteed resume.
        let ended = advance_interrupt(state, &[], &[task_with_id("running")]);
        assert_eq!(
            ended,
            Interrupt::Resuming {
                parked: vec!["running".to_owned()]
            }
        );
    }

    #[test]
    fn resuming_ends_the_instant_a_parked_task_is_seen_in_flight() {
        let state = Interrupt::Resuming {
            parked: vec!["running".to_owned()],
        };
        let still_waiting = advance_interrupt(state.clone(), &[], &[task_with_id("running")]);
        assert_eq!(still_waiting, state);

        let dispatched = advance_interrupt(state, &["running".to_owned()], &[]);
        assert_eq!(dispatched, Interrupt::Idle);
    }

    /// R1-2-1: an interrupt task that stops being runnable - held, blocked,
    /// or otherwise moved on by an operator with no claim standing in the
    /// way - must not wedge the sequence (and so the whole loop's dispatch,
    /// via `interrupt_gate`) waiting forever for a dispatch that can never
    /// come. The parked run still gets its resume.
    #[test]
    fn an_interrupt_task_that_stops_being_runnable_abandons_the_wait_without_losing_the_parked_run()
    {
        let state = Interrupt::Parking {
            parked: vec!["running".to_owned()],
            interrupt_task: "marked".to_owned(),
        };
        // `marked` has been held/blocked/deleted since the sequence began:
        // it no longer appears in `runnable` at all.
        let next = advance_interrupt(state, &[], &[]);
        assert_eq!(
            next,
            Interrupt::Resuming {
                parked: vec!["running".to_owned()]
            },
            "abandoning the interrupt must not abandon the resume it owes"
        );
    }

    /// The same abandonment, one step later: `Resuming` itself must not wait
    /// forever for a parked task that has since become unrunnable.
    #[test]
    fn resuming_abandons_a_parked_task_that_stops_being_runnable() {
        let state = Interrupt::Resuming {
            parked: vec!["running".to_owned()],
        };
        let next = advance_interrupt(state, &[], &[]);
        assert_eq!(
            next,
            Interrupt::Idle,
            "nothing is left to wait for; the loop must not stay wedged"
        );
    }

    #[test]
    fn disabled_by_config_the_sequence_can_never_leave_idle() {
        let marked = interrupt_task("marked");
        let next = advance_interrupt_tick(
            false,
            Interrupt::Idle,
            &["running".to_owned()],
            std::slice::from_ref(&marked),
        );
        assert_eq!(
            next,
            Interrupt::Idle,
            "an unmarked, unconfigured daemon must behave exactly as before"
        );
    }

    #[test]
    fn the_gate_blocks_everyone_while_something_parked_is_still_in_flight() {
        let state = Interrupt::Parking {
            parked: vec!["running".to_owned()],
            interrupt_task: "marked".to_owned(),
        };
        let candidates = vec![interrupt_task("marked"), task()];
        let allowed = interrupt_gate(&state, &["running".to_owned()], candidates);
        assert!(
            allowed.is_empty(),
            "nothing may dispatch - not even the interrupt task itself - \
             until the parked run has actually stopped"
        );
    }

    #[test]
    fn the_gate_lets_only_the_interrupt_task_through_once_parked_work_has_stopped() {
        let state = Interrupt::Parking {
            parked: vec!["running".to_owned()],
            interrupt_task: "marked".to_owned(),
        };
        let other = task();
        let candidates = vec![interrupt_task("marked"), other.clone()];
        let allowed = interrupt_gate(&state, &[], candidates);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].id, "marked");
    }

    #[test]
    fn the_gate_blocks_everyone_while_the_interrupt_task_itself_is_in_flight() {
        let state = Interrupt::Running {
            parked: vec!["running".to_owned()],
            interrupt_task: "marked".to_owned(),
        };
        let candidates = vec![task(), task()];
        let allowed = interrupt_gate(&state, &["marked".to_owned()], candidates);
        assert!(allowed.is_empty());
    }

    /// R1-1-1 / R1-1-2: even when more than one task was in flight when the
    /// sequence began (only reachable above the default
    /// `max_concurrent_runs = 1`), `Resuming` offers at most one of them -
    /// never both in the same tick, which is what "exactly one resume, no
    /// simultaneous run" actually requires structurally rather than by
    /// coincidence of how many ordinary slots happen to be free.
    #[test]
    fn the_gate_offers_at_most_one_candidate_while_resuming_even_with_two_parked() {
        let state = Interrupt::Resuming {
            parked: vec!["a".to_owned(), "c".to_owned()],
        };
        let candidates = vec![task_with_id("a"), task_with_id("c"), task_with_id("other")];
        let allowed = interrupt_gate(&state, &[], candidates);
        assert_eq!(
            allowed.len(),
            1,
            "at most one candidate may be offered while resuming: {allowed:?}"
        );
        assert_eq!(allowed[0].id, "a");
    }

    #[test]
    fn the_gate_offers_nothing_while_resuming_if_no_parked_task_is_runnable() {
        let state = Interrupt::Resuming {
            parked: vec!["a".to_owned()],
        };
        let allowed = interrupt_gate(&state, &[], vec![task_with_id("other")]);
        assert!(allowed.is_empty());
    }

    /// The invariant the completion criteria ask for by name: across a whole
    /// simulated sequence, there is never a tick where the gate would let
    /// through both the parked run's resume and the interrupt task, and
    /// exactly one candidate resumes the instant the interrupt task's run
    /// ends - never zero, never more than one.
    #[test]
    fn a_full_sequence_never_gates_two_runs_through_at_once_and_resumes_exactly_one() {
        let running = task(); // id: whatever `Task::new` minted
        let marked = interrupt_task("marked");

        let mut state = Interrupt::Idle;
        // Tick 1: `running` is in flight, `marked` becomes runnable.
        let in_flight = vec![running.id.clone()];
        state = advance_interrupt_tick(true, state, &in_flight, std::slice::from_ref(&marked));
        let gated = interrupt_gate(&state, &in_flight, vec![marked.clone(), running.clone()]);
        assert!(gated.is_empty(), "still waiting on `running` to park");

        // Tick 2: `running` parked and left flight; nothing dispatched yet.
        state = advance_interrupt_tick(true, state, &[], &[marked.clone(), running.clone()]);
        let gated = interrupt_gate(&state, &[], vec![marked.clone(), running.clone()]);
        assert_eq!(
            gated.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["marked"],
            "only the interrupt task may be offered to the dispatcher now"
        );

        // Tick 3: `marked` is now in flight (dispatched from tick 2's gate).
        state = advance_interrupt_tick(
            true,
            state,
            &["marked".to_owned()],
            std::slice::from_ref(&running),
        );
        let gated = interrupt_gate(
            &state,
            &["marked".to_owned()],
            vec![marked.clone(), running.clone()],
        );
        assert!(
            gated.is_empty(),
            "the parked run must not be offered back while the interrupt \
             task is still running"
        );

        // Tick 4: `marked`'s run reached a terminal status and left flight.
        // A higher-priority ordinary task `other` is also runnable now - it
        // must not be let through instead of, or alongside, `running`.
        let other = task_with_id("other");
        state = advance_interrupt_tick(true, state, &[], &[running.clone(), other.clone()]);
        assert_eq!(
            state,
            Interrupt::Resuming {
                parked: vec![running.id.clone()]
            }
        );
        let gated = interrupt_gate(&state, &[], vec![other.clone(), running.clone()]);
        assert_eq!(
            gated.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec![running.id.as_str()],
            "exactly the parked run resumes - not the unrelated task, even \
             though it was offered first"
        );

        // Tick 5: `running` is now in flight (dispatched from tick 4's
        // gate). Only now does the sequence end and ordinary dispatch fully
        // resume.
        state = advance_interrupt_tick(
            true,
            state,
            std::slice::from_ref(&running.id),
            std::slice::from_ref(&other),
        );
        assert_eq!(state, Interrupt::Idle);
        let gated = interrupt_gate(
            &state,
            std::slice::from_ref(&running.id),
            vec![other.clone()],
        );
        assert_eq!(
            gated.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec![other.id.as_str()],
            "ordinary dispatch is unrestricted again"
        );
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
            (RunStatus::VerifiedNoop, TaskStatus::Held, 1),
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
                    parked: false,
                    quota_hit: matches!(run, RunStatus::Stalled),
                    no_viable_candidates: false,
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
                parked: false,
                quota_hit: true,
                no_viable_candidates: false,
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
                parked: false,
                quota_hit: false,
                no_viable_candidates: false,
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
                parked: false,
                quota_hit: false,
                no_viable_candidates: false,
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
                parked: false,
                quota_hit: false,
                no_viable_candidates: false,
            },
            "findings open",
            4,
        );
        assert_eq!(empty_handed.status, TaskStatus::Failed);
        assert!(empty_handed.status.runnable());
    }

    #[test]
    fn a_verified_noop_run_hands_off_rather_than_closing_or_auto_retrying() {
        // Every candidate agreed, with evidence, that nothing belonged in the
        // worktree. That is not a confirmed success to close automatically -
        // a human still has to check the claim - and it is not an ordinary
        // failure either, so this settles exactly like a pull request nobody
        // merged yet: `Held`, same as `Blocked` with a PR.
        let mut noop = task();
        noop.start("20260912-131304-391f".to_owned());
        settle(
            &mut noop,
            Verdict {
                status: RunStatus::VerifiedNoop,
                left_pr: false,
                parked: false,
                quota_hit: false,
                no_viable_candidates: true,
            },
            "candidate A: already fixed by b32cfc4, on main",
            4,
        );
        assert_eq!(
            noop.status,
            TaskStatus::Held,
            "an unverified claim is a request for a human, not a failure"
        );
        assert!(
            !noop.status.runnable(),
            "the loop must not requeue this on the same unverified claim"
        );
        // `Task::release` resets attempts to zero the moment a human looks at
        // the evidence and lets it run again, so it does not matter here
        // whether the one attempt already spent stays spent - what matters is
        // that nothing retries this task unattended in the meantime.
        assert_eq!(noop.attempts, 1);
    }

    #[test]
    fn parking_costs_the_task_no_attempt_and_leaves_it_in_line() {
        // Parking is the operator asking for the process back - to replace the
        // binary, most of all. The run's work is intact on disk, so this is
        // not a failed attempt, and charging for it would mean a few upgrades
        // could exhaust a budget meant for agents that misbehaved.
        let mut parked = task();
        parked.start("20260903-183634-2d98".to_owned());
        settle(
            &mut parked,
            Verdict {
                status: RunStatus::Implementing,
                left_pr: false,
                quota_hit: false,
                parked: true,
                no_viable_candidates: false,
            },
            "parked after `implementing`",
            2,
        );
        assert_eq!(parked.attempts, 0, "a park is refunded");
        assert!(
            parked.status.runnable(),
            "and the task stays in line so the next loop resumes its run"
        );
        assert_eq!(
            parked.last_error.as_deref(),
            Some("parked after `implementing`"),
            "the card says where it stopped"
        );

        // Without the park flag the same non-terminal status is what it always
        // was: `execute` returning mid-flight, which is a bug and spends an
        // attempt so a task cannot loop on it forever.
        let mut broken = task();
        broken.start("20260903-183634-2d98".to_owned());
        settle(
            &mut broken,
            Verdict {
                status: RunStatus::Implementing,
                left_pr: false,
                quota_hit: false,
                parked: false,
                no_viable_candidates: false,
            },
            "returned mid-flight",
            2,
        );
        assert_eq!(broken.attempts, 1);
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
                parked: false,
                quota_hit: false,
                no_viable_candidates: false,
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
                parked: false,
                quota_hit: true,
                no_viable_candidates: false,
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
                parked: false,
                quota_hit: false,
                no_viable_candidates: false,
            },
            "no quorum again",
            2,
        );
        assert_eq!(worn.status, TaskStatus::Held);
        assert!(!worn.status.runnable());
    }

    #[test]
    fn a_quota_wipeout_that_leaves_nothing_to_judge_also_costs_no_attempt() {
        // The implement wave loses every seat to the same rate limit and
        // `after_implement` bails with nothing viable, which surfaces as
        // `Failed` rather than `Stalled`. That is the same machine fact the
        // `Stalled`-quota row already refunds, and must be refunded the same
        // way, or a quota outage quietly holds every task it touches instead
        // of leaving them in line for the reset.
        let mut wiped_out = task();
        wiped_out.start("20260907-025000-a1b2".to_owned());
        settle(
            &mut wiped_out,
            Verdict {
                status: RunStatus::Failed,
                left_pr: false,
                parked: false,
                quota_hit: true,
                no_viable_candidates: true,
            },
            "no candidate produced a change; nothing to judge",
            2,
        );
        assert_eq!(wiped_out.attempts, 0, "a total quota wipeout is refunded");
        assert!(
            wiped_out.status.runnable(),
            "a machine problem must leave the task in line"
        );

        // This is the exemption that must stay narrow: a candidate that did
        // produce a change, and then failed for some other reason, still
        // spends the attempt even though a seat elsewhere hit its quota.
        // Otherwise every ordinary failure that happens to share a run with
        // an unrelated rate limit would be refunded for free.
        let mut partial_progress = task();
        partial_progress.start("20260907-025500-c3d4".to_owned());
        settle(
            &mut partial_progress,
            Verdict {
                status: RunStatus::Failed,
                left_pr: false,
                parked: false,
                quota_hit: true,
                no_viable_candidates: false,
            },
            "gate failed on the winning candidate",
            2,
        );
        assert_eq!(
            partial_progress.attempts, 1,
            "a candidate that actually produced a change spends the attempt \
             even though some other seat hit its quota"
        );
        assert!(partial_progress.status.runnable());
    }

    #[test]
    fn reclaim_refunds_a_recovered_quota_wipeout_the_same_way_a_live_settle_does() {
        // `reclaim` builds its own `Verdict` from a `RunState` it loads off
        // disk, and that construction must reach the same conclusion as the
        // one `attempt` builds from a live run, or a crash at exactly the
        // wrong moment gives a recovered task a different policy than one a
        // daemon finished settling itself.
        let mut t = task();
        t.start("20260907-025000-a1b2".to_owned());
        let mut state = run_state(RunStatus::Failed);
        state.quota.push(QuotaLoss {
            seat: "cand-a".to_owned(),
            node: "implement".to_owned(),
            at: Timestamp::now(),
            reset: None,
        });
        assert!(
            state.viable().is_empty(),
            "no candidate was added, so nothing is viable"
        );
        reclaim(&mut t, Some(state), 2);
        assert_eq!(t.attempts, 0, "a recovered quota wipeout is refunded");
        assert!(t.status.runnable());
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
        held.hold_machine(None);
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
    fn sweep_removes_an_old_unparseable_lock_and_keeps_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut old = task();
        old.id = "20260101-000000-old0".to_owned();
        queue.put(&mut old).unwrap();
        let mut fresh = task();
        fresh.id = "20260101-000000-new0".to_owned();
        queue.put(&mut fresh).unwrap();

        // No parseable pid at all, so age is the only signal there is to
        // check - unlike a real `Queue::claim`, which always names a real,
        // and therefore alive, pid this test cannot fake as dead.
        std::fs::write(dir.path().join(format!("{}.lock", old.id)), "not a pid").unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let live = queue.claim(&fresh.id).unwrap();

        let swept = sweep_stale_claims(&queue, Duration::from_millis(50));
        assert_eq!(swept, vec![old.id.clone()]);
        assert!(
            queue.claim(&old.id).is_ok(),
            "an unparseable lock older than the threshold is swept"
        );
        assert!(
            queue.claim(&fresh.id).is_err(),
            "a live pid protects its lock regardless of age"
        );
        drop(live);
    }

    #[test]
    fn an_old_lock_whose_pid_is_still_alive_is_never_swept_by_age_alone() {
        // The regression this guards: `sweep` now runs concurrently with
        // every attempt this daemon itself has spawned (see
        // `InFlightGuard`), not only between them the way a single
        // sequential loop once did. A run that legitimately outlives
        // `older_than` still has this very process's own live pid sitting in
        // its own lock file on every later sweep, and deciding by age alone
        // would delete that still-valid claim out from under the attempt
        // that holds it - which `reclaim_orphaned_running` would then read
        // as abandoned and hand to a second, competing attempt.
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut t = task();
        t.id = "20260101-000000-live".to_owned();
        queue.put(&mut t).unwrap();

        let claim = queue.claim(&t.id).unwrap();
        std::thread::sleep(Duration::from_millis(60));

        let swept = sweep_stale_claims(&queue, Duration::from_millis(50));
        assert!(
            swept.is_empty(),
            "a lock naming a live pid must never be swept by age, no matter how old: {swept:?}"
        );
        assert!(
            queue.claim(&t.id).is_err(),
            "the lock still protects its task"
        );
        drop(claim);
    }

    /// このテストプロセスにはなり得ない決定的なフィクスチャ PID。
    /// OS 上の状態は意図的に無関係で、各利用箇所が方針問い合わせを注入する。
    fn injected_dead_pid() -> u32 {
        std::process::id().checked_add(1).unwrap_or(1)
    }

    #[test]
    fn a_lock_naming_a_dead_pid_is_swept_at_once_regardless_of_age() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut t = task();
        t.id = "20260101-000000-dead".to_owned();
        queue.put(&mut t).unwrap();
        let dead_pid = injected_dead_pid();

        // Written directly rather than through `Queue::claim`, which would
        // stamp this test process's own very much alive pid and defeat the
        // point: this is what a `.lock` left by a `SIGKILL`ed daemon looks
        // like moments after it died, not six hours later.
        std::fs::write(
            dir.path().join(format!("{}.lock", t.id)),
            dead_pid.to_string(),
        )
        .unwrap();

        let swept = sweep_stale_claims_with(&queue, Duration::from_secs(6 * 60 * 60), |pid| {
            pid != dead_pid
        });
        assert_eq!(
            swept,
            vec![t.id.clone()],
            "a dead owner is reclaimed immediately, not after STALE_CLAIM"
        );
        assert!(queue.claim(&t.id).is_ok(), "the task is claimable again");
    }

    #[test]
    fn sweeping_on_every_poll_catches_a_lock_that_appears_after_the_first_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut t = task();
        t.id = "20260101-000000-late".to_owned();
        queue.put(&mut t).unwrap();
        let dead_pid = injected_dead_pid();

        // Tick one, standing in for the sweep `poll` already runs at
        // startup: nothing to find yet.
        assert!(
            sweep_stale_claims(&queue, Duration::from_secs(6 * 60 * 60)).is_empty(),
            "nothing has claimed the task yet"
        );

        // A second daemon claims the task and dies before it ever writes
        // `running`, well after this loop's own startup sweep already ran.
        std::fs::write(
            dir.path().join(format!("{}.lock", t.id)),
            dead_pid.to_string(),
        )
        .unwrap();

        // Tick two, standing in for a poll long into this daemon's uptime:
        // the same function, called again, notices what only just appeared -
        // proving the sweep is not a one-shot startup check.
        let swept = sweep_stale_claims_with(&queue, Duration::from_secs(6 * 60 * 60), |pid| {
            pid != dead_pid
        });
        assert_eq!(swept, vec![t.id.clone()]);
    }

    #[test]
    fn a_running_task_behind_a_dead_daemons_lock_recovers_once_swept_and_keeps_its_history() {
        // `reclaim_orphaned_running` looks up the task's last run, which
        // touches `run::home()`; the first call anywhere in this binary wins,
        // so this is a no-op if another test already pinned one, and either
        // way the run id below is never written under it.
        crate::run::set_home(std::env::temp_dir().join("magi-daemon-test-home"));
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut t = task();
        t.id = "20260101-000000-crsh".to_owned();
        t.status = TaskStatus::Running;
        t.attempts = 1;
        // No `run.json` behind this id: standing in for a run this test does
        // not need to make readable, since the point is the lock, not the
        // recovery table `reclaim` already has its own tests for.
        t.runs.push("20260904-000000-4043".to_owned());
        queue.put(&mut t).unwrap();
        let dead_pid = injected_dead_pid();

        // The crashed daemon's own claim, naming a pid nothing on the
        // machine holds anymore.
        std::fs::write(
            dir.path().join(format!("{}.lock", t.id)),
            dead_pid.to_string(),
        )
        .unwrap();

        // Before the lock is swept the task looks claimed, and
        // `reclaim_orphaned_running` must leave it alone - this is exactly
        // the bug: a `running` task stranded behind a dead daemon's lock,
        // invisible to the claim-as-proof check because the lock outlived
        // the process that wrote it.
        assert!(reclaim_orphaned_running(&queue, 2).is_empty());
        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Running);

        let swept = sweep_stale_claims_with(&queue, Duration::from_secs(6 * 60 * 60), |pid| {
            pid != dead_pid
        });
        assert_eq!(swept, vec![t.id.clone()]);

        let reclaimed = reclaim_orphaned_running(&queue, 2);
        assert_eq!(reclaimed, vec![t.id.clone()]);
        let after = queue.get(&t.id).unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Held,
            "no run.json to recover from, so a human is asked"
        );
        assert_eq!(
            after.runs,
            vec!["20260904-000000-4043".to_owned()],
            "the crashed run's id is kept as evidence, not discarded"
        );
    }

    #[test]
    fn a_lock_is_kept_when_the_process_query_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut t = task();
        t.id = "20260101-000000-unknown".to_owned();
        queue.put(&mut t).unwrap();
        let dead_pid = injected_dead_pid();
        std::fs::write(
            dir.path().join(format!("{}.lock", t.id)),
            dead_pid.to_string(),
        )
        .unwrap();

        let swept = sweep_stale_claims_with(&queue, Duration::ZERO, |_| true);
        assert!(swept.is_empty(), "an unknown pid must keep its lock");
        assert!(queue.claim(&t.id).is_err(), "the lock remains protective");
    }

    fn run_state(status: RunStatus) -> RunState {
        let mut state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        );
        state.status = status;
        state
    }

    fn candidate(label: char, summary: &str, empty: bool, failed: Option<&str>) -> Candidate {
        Candidate {
            index: 0,
            label,
            agent: "claude".to_owned(),
            branch: format!("magi/x/{label}"),
            worktree: PathBuf::from("/repo"),
            summary: summary.to_owned(),
            stat: String::new(),
            files: 0,
            commits: usize::from(!empty),
            empty,
            failed: failed.map(str::to_owned),
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }
    }

    #[test]
    fn diagnostic_names_the_failing_gate_checks_and_their_output() {
        let mut state = run_state(RunStatus::Blocked);
        state.gate = vec![
            CommandOutcome {
                command: "cargo make check".to_owned(),
                code: Some(0),
                output_tail: "ok".to_owned(),
                duration_ms: 0,
                resource_blocked: false,
            },
            CommandOutcome {
                command: "cargo test".to_owned(),
                code: Some(101),
                output_tail: "thread 'x' panicked: assertion failed".to_owned(),
                duration_ms: 0,
                resource_blocked: false,
            },
        ];
        let d = diagnostic(&state).expect("a failing gate must produce a diagnostic");
        assert!(d.contains("cargo test"), "{d}");
        assert!(
            !d.contains("cargo make check"),
            "a passing check is not a diagnostic: {d}"
        );
        assert!(d.contains("assertion failed"), "{d}");
    }

    #[test]
    fn diagnostic_names_the_checks_the_fixer_gave_up_in_front_of() {
        let mut state = run_state(RunStatus::Blocked);
        state.event(
            "land",
            "stopped: the fixer produced no commit while 2 check(s) were failing \
             (build, lint); stopping instead of looping on an unchanged tree",
        );
        let d = diagnostic(&state).expect("a stalled land loop must produce a diagnostic");
        assert!(d.contains("build"), "{d}");
        assert!(d.contains("lint"), "{d}");
        assert!(d.contains("fixer produced no commit"), "{d}");
    }

    #[test]
    fn describe_never_leaves_a_verified_noop_reading_as_a_bare_status_code() {
        // `describe`'s output becomes `Task::last_error` verbatim, and the
        // phone renders that in the same alarm-styled box an ordinary
        // failure gets. A bare `verified_noop` there would read exactly like
        // the failure this status exists to be told apart from.
        let state = run_state(RunStatus::VerifiedNoop);
        let d = describe(&state);
        assert!(
            d.contains("agent-verified no-op"),
            "expected the display label, not the wire spelling: {d}"
        );
        assert!(!d.contains("verified_noop"), "{d}");
    }

    #[test]
    fn diagnostic_carries_a_candidates_own_final_word_when_none_was_viable() {
        // The whole point of the feature: a run held as "no candidate produced
        // a change" can mean the implementer actually finished the task and
        // only left a clean local tree behind - see AGENTS.md on this exact
        // failure mode. The diagnostic has to carry what the agent actually
        // said, not just the fact that nothing was there to judge.
        let mut state = run_state(RunStatus::Failed);
        state.candidates = vec![candidate(
            'A',
            "opened pull request #42, merged it, tagged v1.2.3 and published the release",
            true,
            None,
        )];
        let d = diagnostic(&state).expect("an empty candidate with a summary must be surfaced");
        assert!(d.contains("candidate A"), "{d}");
        assert!(d.contains("tagged v1.2.3"), "{d}");
    }

    #[test]
    fn diagnostic_falls_back_to_a_candidates_failure_reason_when_it_has_no_summary() {
        let mut state = run_state(RunStatus::Failed);
        state.candidates = vec![candidate('A', "", true, Some("agent timed out"))];
        let d = diagnostic(&state).expect("a candidate's own failure reason must be surfaced");
        assert!(d.contains("candidate A"), "{d}");
        assert!(d.contains("agent timed out"), "{d}");
    }

    #[test]
    fn diagnostic_is_none_when_nothing_recognisable_explains_the_hold() {
        // A viable candidate existed, the gate never ran, and nothing land
        // said matches - `Task::last_error` is left to explain this one alone.
        let mut state = run_state(RunStatus::Failed);
        state.candidates = vec![candidate('A', "did the work", false, None)];
        assert!(diagnostic(&state).is_none());
    }

    #[test]
    fn diagnostic_is_bounded_however_much_a_run_printed() {
        let mut state = run_state(RunStatus::Blocked);
        state.gate = vec![
            CommandOutcome {
                command: "cargo test".to_owned(),
                code: Some(101),
                output_tail: "x".repeat(50_000),
                duration_ms: 0,
                resource_blocked: false,
            },
            CommandOutcome {
                command: "cargo clippy".to_owned(),
                code: Some(1),
                output_tail: "y".repeat(50_000),
                duration_ms: 0,
                resource_blocked: false,
            },
        ];
        state.candidates = vec![
            candidate('A', &"z".repeat(50_000), true, None),
            candidate('B', &"w".repeat(50_000), true, None),
        ];
        let d = diagnostic(&state).expect("plenty here to diagnose");
        assert!(
            d.len() <= DIAGNOSTIC_MAX,
            "diagnostic grew to {} bytes, unbounded",
            d.len()
        );
    }

    #[test]
    fn settle_and_diagnose_attaches_a_diagnostic_only_once_the_task_is_held() {
        let mut state = run_state(RunStatus::Blocked);
        state.gate = vec![CommandOutcome {
            command: "cargo test".to_owned(),
            code: Some(101),
            output_tail: "assertion failed".to_owned(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        let verdict = Verdict {
            status: RunStatus::Blocked,
            left_pr: false,
            quota_hit: false,
            parked: false,
            no_viable_candidates: false,
        };

        // Attempt one of two still has a retry coming: no diagnostic yet, the
        // task is going to run again and this run's evidence would go stale.
        let mut t = task();
        t.start("run-1".to_owned());
        settle_and_diagnose(&mut t, verdict, "gate failed", 2, &state);
        assert_eq!(t.status, TaskStatus::Failed);
        assert!(t.diagnostic.is_none());

        // Attempt two exhausts the budget: now it is held, and the
        // diagnostic is what `magi task show` has to say more than one line.
        t.start("run-2".to_owned());
        settle_and_diagnose(&mut t, verdict, "gate failed", 2, &state);
        assert_eq!(t.status, TaskStatus::Held);
        let d = t.diagnostic.expect("a held task must carry its diagnostic");
        assert!(d.contains("cargo test"), "{d}");
    }

    fn approval_question(run: &str) -> ask::Question {
        ask::Question::new(
            run.to_owned(),
            land::APPROVAL_NODE.to_owned(),
            "land".to_owned(),
            "merge?".to_owned(),
            String::new(),
            vec!["merge".to_owned(), "hold".to_owned()],
        )
    }

    #[test]
    fn land_resume_state_leaves_a_fresh_open_question_waiting() {
        crate::run::set_home(std::env::temp_dir().join("magi-daemon-test-home"));
        let mut state = run_state(RunStatus::Landing);
        state.id = "20260101-000000-fre1".to_owned();
        state.parked = true;
        state.save().unwrap();
        ask::Questions::open()
            .put(&mut approval_question(&state.id))
            .unwrap();

        let mut t = task();
        t.runs.push(state.id.clone());
        assert_eq!(
            land_resume_state(&t),
            LandResume::StillWaiting,
            "nobody has answered and the timeout has not passed"
        );
    }

    #[test]
    fn land_resume_state_abandons_a_question_that_outlived_answer_timeout() {
        // `ask::ask_and_wait`'s own deadline used to retire a question
        // nobody answered; land's approval bypasses that wait (see
        // `land::approval_gate`), so this is now the only place
        // `graph.answer_timeout` is enforced for a land approval at all.
        crate::run::set_home(std::env::temp_dir().join("magi-daemon-test-home"));
        let mut state = run_state(RunStatus::Landing);
        state.id = "20260101-000000-exp1".to_owned();
        state.parked = true;
        state.config.graph.answer_timeout = 60;
        state.save().unwrap();

        let store = ask::Questions::open();
        let mut q = approval_question(&state.id);
        q.asked_at = Timestamp::now() - jiff::SignedDuration::from_secs(120);
        store.put(&mut q).unwrap();

        let mut t = task();
        t.runs.push(state.id.clone());
        assert_eq!(
            land_resume_state(&t),
            LandResume::Ready,
            "an expired question must not be waited on forever"
        );

        let after = store.get(&q.id).unwrap();
        assert!(
            !after.status.open(),
            "the question is abandoned, not silently ignored"
        );
        assert!(
            after.resolution().is_none(),
            "an abandoned question is not read as a decision"
        );
    }

    #[test]
    fn reclaim_settles_a_running_task_against_its_last_run() {
        let mut t = task();
        t.start("20260904-000000-4043".to_owned());
        reclaim(&mut t, Some(run_state(RunStatus::Ready)), 2);
        assert_eq!(
            t.status,
            TaskStatus::Done,
            "a run that actually finished must not stay `running` forever"
        );
    }

    #[test]
    fn reclaim_reuses_the_same_retry_policy_as_a_live_settle() {
        // A blocked run with attempts left goes back to `Failed`, exactly as
        // it would from `attempt` itself - `reclaim` must not invent a second
        // policy for a task a daemon merely stopped without reporting.
        let mut t = task();
        t.start("20260904-000000-4043".to_owned());
        reclaim(&mut t, Some(run_state(RunStatus::Blocked)), 2);
        assert_eq!(t.status, TaskStatus::Failed);
        assert!(t.status.runnable());
    }

    #[test]
    fn reclaim_holds_a_running_task_whose_run_cannot_be_found() {
        let mut t = task();
        t.start("20260904-000000-4043".to_owned());
        reclaim(&mut t, None, 2);
        assert_eq!(t.status, TaskStatus::Held);
        assert!(
            t.last_error
                .as_deref()
                .is_some_and(|e| e.contains("running")),
            "the operator needs to know why this task was held"
        );
    }

    #[test]
    fn orphaned_running_tasks_are_reclaimed_but_live_ones_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());

        // No run recorded, so this never has to touch `RunState::load`.
        let mut orphaned = task();
        orphaned.id = "20260904-000000-orph".to_owned();
        orphaned.status = TaskStatus::Running;
        orphaned.attempts = 1;
        queue.put(&mut orphaned).unwrap();

        let mut alive = task();
        alive.id = "20260904-000000-live".to_owned();
        alive.status = TaskStatus::Running;
        alive.attempts = 1;
        queue.put(&mut alive).unwrap();
        let _held_by_a_live_daemon = queue.claim(&alive.id).unwrap();

        let mut queued = task();
        queued.id = "20260904-000000-wait".to_owned();
        queue.put(&mut queued).unwrap();

        let reclaimed = reclaim_orphaned_running(&queue, 2);
        assert_eq!(reclaimed, vec![orphaned.id.clone()]);

        assert_eq!(
            queue.get(&orphaned.id).unwrap().status,
            TaskStatus::Held,
            "nothing was driving it and there was no run to recover"
        );
        assert_eq!(
            queue.get(&alive.id).unwrap().status,
            TaskStatus::Running,
            "a live claim must protect the task it belongs to"
        );
        assert_eq!(queue.get(&queued.id).unwrap().status, TaskStatus::Queued);
    }

    /// Read a run.json back from an explicit `home`, the same way
    /// `reclaim_abandoned_runs` itself does - never through the
    /// process-global `RunState::load`, which this test's own `home` (an
    /// isolated tempdir, never pinned into the shared `OnceLock`) does not
    /// use at all.
    fn read_run_under(home: &Path, id: &str) -> RunState {
        let body = std::fs::read_to_string(home.join("runs").join(id).join("run.json")).unwrap();
        serde_json::from_str(&body).unwrap()
    }

    #[test]
    fn reclaim_abandoned_runs_fails_a_run_whose_active_seats_are_all_provably_dead() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let now = Timestamp::now();
        let overrun_seat = || crate::run::ActiveSeat {
            node: "implement".to_owned(),
            started_at: now - jiff::SignedDuration::new(21_000, 0),
            timeout_secs: 3_600,
            attempt: 0,
            task: None,
            command: None,
            index: None,
            total: None,
        };

        let mut dead = run_state(RunStatus::Implementing);
        dead.id = "20260101-000000-dead".to_owned();
        dead.active.insert("impl-A".to_owned(), overrun_seat());
        // A `driver_pid` the injected query below confirms gone outright —
        // `liveness` reads this as `Dead`, not merely "no daemon claims it".
        dead.driver_pid = Some(4242);
        dead.save_under(&home).unwrap();

        // Same shape, but a live daemon's heartbeat names it: must be left
        // exactly alone, however far past its own timeout the seat sits.
        let mut alive = run_state(RunStatus::Implementing);
        alive.id = "20260101-000000-aliv".to_owned();
        alive.active.insert("impl-A".to_owned(), overrun_seat());
        alive.save_under(&home).unwrap();
        let mut status = Status::new();
        status.current = vec![Current {
            task: "20260101-000000-task".to_owned(),
            run: alive.id.clone(),
        }];
        write_status_to(&home.join("daemon.json"), &status).unwrap();

        // The abandoned seat left an open question behind: nobody is left to
        // read an answer once the run is failed, and this must not wait for
        // some later daemon startup's own sweep to notice that.
        let questions = Questions::at(home.join("questions"));
        let mut q = ask::Question::new(
            dead.id.clone(),
            "implement".to_owned(),
            "impl-A".to_owned(),
            "Which storage backend?".to_owned(),
            String::new(),
            vec!["SQLite".to_owned(), "Redis".to_owned()],
        );
        questions.put(&mut q).unwrap();

        let abandoned = reclaim_abandoned_runs_with(
            &home,
            now,
            |pid| if pid == 4242 { Some(false) } else { None },
            |_| panic!("a query answering Dead outright needs no identity corroboration"),
        );
        assert_eq!(abandoned, vec![dead.id.clone()]);

        let reloaded = read_run_under(&home, &dead.id);
        assert_eq!(reloaded.status, RunStatus::Failed);
        assert!(reloaded.active.is_empty());
        assert!(
            !questions.get(&q.id).unwrap().status.open(),
            "the failed run's own open question must be settled in the same pass"
        );

        let still_alive = read_run_under(&home, &alive.id);
        assert_eq!(
            still_alive.status,
            RunStatus::Implementing,
            "a live daemon's claim protects it"
        );
        assert!(!still_alive.active.is_empty());
    }

    /// The exact shape a review round flagged as broken: `magi serve` running
    /// in this same `home` scans *every* run on disk, including a manual
    /// `magi review` / `magi run` this daemon never started and that
    /// therefore claims no heartbeat of its own. Before this scan asked
    /// `liveness` rather than just `is_working_on`, a manual run whose active
    /// seat merely ran a little past its own timeout — the CLI finishing up,
    /// its result still being collected — got wiped and failed by a daemon
    /// that had nothing to do with it, out from under a process that was
    /// still very much running.
    #[test]
    fn reclaim_abandoned_runs_leaves_a_live_manual_run_alone_even_though_no_daemon_claims_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let now = Timestamp::now();

        let mut manual = run_state(RunStatus::Reviewing);
        manual.id = "20260101-000000-manl".to_owned();
        manual.active.insert(
            "review-1".to_owned(),
            crate::run::ActiveSeat {
                node: "review".to_owned(),
                started_at: now - jiff::SignedDuration::new(21_000, 0),
                timeout_secs: 3_600,
                attempt: 0,
                task: None,
                command: None,
                index: None,
                total: None,
            },
        );
        // Not claimed by any daemon (no `daemon.json` at all in this `home`),
        // but a real, still-running process: `liveness` must corroborate this
        // as `Live`, not read the missing daemon claim as death.
        manual.driver_pid = Some(4242);
        manual.driver_started_at = Some("2026-09-22T10:00:00Z".to_owned());
        manual.save_under(&home).unwrap();

        let abandoned = reclaim_abandoned_runs_with(
            &home,
            now,
            |pid| if pid == 4242 { Some(true) } else { None },
            |pid| {
                if pid == 4242 {
                    Some("2026-09-22T10:00:00Z".to_owned())
                } else {
                    None
                }
            },
        );
        assert!(
            abandoned.is_empty(),
            "a manual run a real process is still driving must never be reclaimed: {abandoned:?}"
        );

        let reloaded = read_run_under(&home, &manual.id);
        assert_eq!(reloaded.status, RunStatus::Reviewing);
        assert!(!reloaded.active.is_empty());
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
        status.current = vec![Current {
            task: "20260902-000000-t111".to_owned(),
            run: "20260902-000001-r111".to_owned(),
        }];
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
        status.current = vec![Current {
            task: "20260903-080340-0167".to_owned(),
            run: mine.to_owned(),
        }];
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
    fn is_working_on_short_matches_by_the_worktree_bays_own_name() {
        let dir = tempfile::tempdir().unwrap();
        let now = Timestamp::now();

        assert!(
            !is_working_on_short(dir.path(), "01c2", now),
            "no status file means nobody is working on anything"
        );

        let mut status = Status::new();
        status.current = vec![Current {
            task: "20260903-080340-0167".to_owned(),
            run: "20260903-080619-01c2".to_owned(),
        }];
        status.updated_at = now;
        write_status_to(&dir.path().join("daemon.json"), &status).unwrap();
        assert!(
            is_working_on_short(dir.path(), "01c2", now),
            "the run's short id is the last block of its full id"
        );
        assert!(
            !is_working_on_short(dir.path(), "3cbf", now),
            "a daemon busy with one worktree bay is not working on another"
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
        assert!(reading.current.is_empty());
    }

    #[test]
    fn an_older_daemons_single_object_current_still_reads_as_a_one_item_list() {
        // A daemon started before `current` became a list keeps writing this
        // shape on every heartbeat until it is restarted. A rolling upgrade
        // - a newer `magi web` or `magi doctor` reading an older `magi
        // serve`'s heartbeat - must still see the run it is on, not "no
        // daemon" from a type mismatch failing the whole struct.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("daemon.json"),
            serde_json::json!({
                "schema": 1,
                "pid": 4242,
                "updated_at": Timestamp::now().to_string(),
                "idle": false,
                "current": {"task": "20260902-140501-aaaa", "run": "20260902-140502-bbbb"},
                "completed": 3,
                "polls": 9,
            })
            .to_string(),
        )
        .unwrap();

        let reading = read_status(dir.path()).expect("an older shape must still parse");
        assert!(reading.running(Timestamp::now()));
        assert_eq!(
            reading.current,
            vec![Current {
                task: "20260902-140501-aaaa".to_owned(),
                run: "20260902-140502-bbbb".to_owned(),
            }]
        );
    }

    #[test]
    fn an_absent_or_null_current_reads_as_idle_not_a_parse_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("daemon.json"),
            serde_json::json!({
                "schema": 1,
                "updated_at": Timestamp::now().to_string(),
                "idle": true,
                "current": null,
            })
            .to_string(),
        )
        .unwrap();
        let with_null = read_status(dir.path()).expect("null must still parse");
        assert!(with_null.current.is_empty());

        std::fs::write(
            dir.path().join("daemon.json"),
            serde_json::json!({
                "schema": 1,
                "updated_at": Timestamp::now().to_string(),
                "idle": true,
            })
            .to_string(),
        )
        .unwrap();
        let absent = read_status(dir.path()).expect("a missing field must still parse");
        assert!(absent.current.is_empty());
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
    fn a_solo_task_runs_with_one_candidate_and_a_plain_task_keeps_the_configs() {
        // Three seats said out loud. What `solo` promises is one candidate
        // *whatever the config asks for*, so the contrast has to be a number
        // this test owns - it used to be `Config::default()`'s, which became
        // 1 when one implementation became the default and left the two
        // halves of this test asserting the same thing.
        let mut solo_cfg = Config::default();
        solo_cfg.graph.candidates = 3;
        let mut solo_task = task();
        solo_task.solo = true;
        apply_solo(&mut solo_cfg, &solo_task);
        assert_eq!(solo_cfg.graph.candidates, 1);

        let mut plain_cfg = Config::default();
        plain_cfg.graph.candidates = 3;
        let plain_task = task();
        assert!(!plain_task.solo);
        apply_solo(&mut plain_cfg, &plain_task);
        assert_eq!(
            plain_cfg.graph.candidates, 3,
            "a task that did not ask to run alone keeps the config's candidates"
        );
    }

    #[test]
    fn merge_overrides_are_parsed_or_refused() {
        assert_eq!(merge_mode("none").unwrap(), MergeMode::None);
        assert_eq!(merge_mode("local").unwrap(), MergeMode::Local);
        assert_eq!(merge_mode("pr").unwrap(), MergeMode::Pr);
        assert!(merge_mode("squash").is_err());
    }

    #[test]
    fn quota_wait_uses_a_future_reset_time_capped_and_falls_back_otherwise() {
        let now = Timestamp::now();
        let fallback = Duration::from_secs(300);
        let cap = Duration::from_secs(1800);

        // No reset hint at all: the fallback.
        assert_eq!(quota_wait(None, now, fallback, cap), fallback);

        // A reset ten minutes out, well inside the cap: waited for exactly.
        let soon = now + jiff::SignedDuration::from_secs(600);
        assert_eq!(
            quota_wait(Some(soon), now, fallback, cap),
            Duration::from_secs(600)
        );

        // A reset already in the past is not trusted: the fallback, not a
        // zero or negative wait that would spin the loop right back around.
        let past = now - jiff::SignedDuration::from_secs(60);
        assert_eq!(quota_wait(Some(past), now, fallback, cap), fallback);

        // A reset further out than the cap is trusted for direction but not
        // for magnitude: a parsing slip must not sleep the loop for a day.
        let far = now + jiff::SignedDuration::from_secs(3 * 3600);
        assert_eq!(quota_wait(Some(far), now, fallback, cap), cap);
    }

    #[test]
    fn parse_reset_hint_reads_the_claude_cli_shape_and_rolls_a_past_clock_to_tomorrow() {
        let now = "2026-09-07T02:50:00Z".parse::<Timestamp>().unwrap();

        let at = parse_reset_hint("4:50am (UTC)", now, now).expect("a recognised shape parses");
        assert_eq!(at.to_string(), "2026-09-07T04:50:00Z");

        // Same clock reading, but it has already gone by today: read as
        // tomorrow's, since the CLI would not still be reporting a limit past
        // its own stated reset.
        let already_past =
            parse_reset_hint("1:00am (UTC)", now, now).expect("a recognised shape parses");
        assert_eq!(already_past.to_string(), "2026-09-08T01:00:00Z");

        assert!(
            parse_reset_hint("session limit reached", now, now).is_none(),
            "free text with no recognised shape is not guessed at"
        );
        assert!(
            parse_reset_hint("4:50am (Nowhere/Fake)", now, now).is_none(),
            "an unresolvable zone name is not guessed at either"
        );
    }

    #[test]
    fn parse_reset_hint_reads_the_codex_cli_shape_with_no_year_rollover_needed() {
        let now = "2026-09-07T02:50:00Z".parse::<Timestamp>().unwrap();

        let at = parse_reset_hint(
            "You've hit your usage limit. Visit \
             https://chatgpt.com/codex/settings/usage to purchase more \
             credits or try again at Sep 19th, 2026 5:10 PM.",
            now,
            now,
        )
        .expect("the codex reset wording is a recognised shape");
        assert_eq!(at.to_string(), "2026-09-19T17:10:00Z");

        // The month is explicit, so a date already earlier in the same
        // sentence-implied year than `now` is trusted as written rather than
        // rolled forward a year the way the bracketed shape rolls a
        // same-day clock reading to tomorrow.
        let earlier = parse_reset_hint("try again at Jan 2nd, 2026 1:00 AM.", now, now)
            .expect("an explicit year needs no rollover");
        assert_eq!(earlier.to_string(), "2026-01-02T01:00:00Z");

        assert!(
            parse_reset_hint("try again at Sep 19th, 26 5:10 PM.", now, now).is_none(),
            "a two-digit year is not the documented shape and is not guessed at"
        );
        assert!(
            parse_reset_hint("try again at Sept 19th, 2026 5:10 PM.", now, now).is_none(),
            "a four-letter month name is not the documented three-letter abbreviation"
        );
        assert!(
            parse_reset_hint("try again at Sep 19th, 2026 5:10 PM (UTC).", now, now).is_none(),
            "an explicit zone on the dated shape is a format nobody has \
             documented, and is refused rather than guessed at as UTC"
        );
    }

    #[test]
    fn parse_reset_hint_reads_agys_relative_shape_from_when_the_loss_was_recorded() {
        let now = "2026-09-24T12:00:00Z".parse::<Timestamp>().unwrap();
        let recorded = "2026-09-24T08:00:00Z".parse::<Timestamp>().unwrap();

        let at = parse_reset_hint("in 1h2m49s", now, recorded).expect("agy's shape parses");
        assert_eq!(at.as_second() - recorded.as_second(), 3769);

        let partial = parse_reset_hint("in 45m", now, recorded).expect("units are optional");
        assert_eq!(partial.as_second() - recorded.as_second(), 45 * 60);

        for bad in ["in ", "in 45", "in 3x", "in m", "in 1h junk", "1h2m"] {
            assert!(
                parse_reset_hint(bad, now, recorded).is_none(),
                "{bad:?} must not be guessed at"
            );
        }
    }

    /// A loop whose queue lives in a temp tree and whose poll interval is far
    /// longer than the test's patience, so anything that waits out a poll
    /// instead of noticing the stop fails rather than merely being slow.
    fn idle_loop(dir: &Path) -> (Opts, Queue, PathBuf, PathBuf, PathBuf) {
        let config = dir.join("magi.toml");
        std::fs::write(
            &config,
            "[disk]\nmin_free_bytes = 0\nauto_fold = false\ncache_limit_bytes = 0\n",
        )
        .unwrap();
        let opts = Opts {
            poll: Duration::from_secs(30),
            config: Some(config),
            // The explicit fixture config keeps startup cleanup from reading
            // machine configuration. This fictional repository likewise
            // keeps any best-effort git cleanup away from this checkout.
            repo: dir.join("repo"),
            ..Opts::default()
        };
        // The status file goes in a directory that does not exist yet, so its
        // creation is itself evidence the loop published one. `worktrees`
        // must be just as fictional: the janitor reclaims worktrees under it
        // for real, and a test that let it fall through to
        // `crate::run::default_worktree_root()` would have it reclaim
        // worktrees out of the operator's real `~/wt/<repo>`, not a fixture -
        // which is exactly what happened before this function took the
        // parameter at all.
        let home = dir.join("home");
        let worktrees = dir.join("wt");
        (
            opts,
            Queue::at(dir.join("queue")),
            home.join("daemon.json"),
            home,
            worktrees,
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
        stop.enter();
        assert!(
            !stop.finishing(),
            "a busy loop nobody has asked to stop is just running"
        );

        stop.stop();
        assert!(
            stop.finishing(),
            "a stop asked for mid-run has not landed until the run is settled"
        );

        stop.exit();
        assert!(
            !stop.finishing(),
            "once the run is settled the stop has landed and there is nothing to finish"
        );
    }

    #[test]
    fn finishing_stays_true_until_the_last_of_several_runs_exits() {
        let stop = Stop::new();
        stop.enter();
        stop.enter();
        stop.stop();
        assert!(stop.finishing(), "two runs still in flight");

        stop.exit();
        assert!(
            stop.finishing(),
            "one run finished, but a sibling is still working"
        );

        stop.exit();
        assert!(
            !stop.finishing(),
            "the last run out is what actually lands the stop"
        );
    }

    #[tokio::test]
    async fn a_loop_already_asked_to_stop_returns_without_waiting_out_a_poll() {
        let dir = tempfile::tempdir().unwrap();
        let (opts, queue, status_file, home, worktrees) = idle_loop(dir.path());
        let stop = Stop::new();
        stop.stop();

        let began = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(2),
            drive(&opts, &queue, &status_file, &home, &worktrees, &stop),
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
        let (opts, queue, status_file, home, worktrees) = idle_loop(dir.path());
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
            drive(&opts, &queue, &status_file, &home, &worktrees, &stop),
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
        let (opts, queue, status_file, home, worktrees) = idle_loop(dir.path());
        let stop = Stop::new();
        stop.stop();

        tokio::time::timeout(
            Duration::from_secs(2),
            drive(&opts, &queue, &status_file, &home, &worktrees, &stop),
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

    #[tokio::test]
    async fn once_runs_startup_housekeeping_before_an_empty_queue_exits() {
        let dir = tempfile::tempdir().unwrap();
        let (mut opts, queue, status_file, home, worktrees) = idle_loop(dir.path());
        opts.once = true;

        let mut settled = RunState::new(
            dir.path().join("repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "fixture".to_owned(),
            Config::default(),
        );
        settled.status = RunStatus::Ready;
        let run_dir = home.join("runs").join(&settled.id);
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("run.json"),
            serde_json::to_string_pretty(&settled).unwrap(),
        )
        .unwrap();
        let questions = Questions::at(home.join("questions"));
        let mut question = ask::Question::new(
            settled.id.clone(),
            "review".to_owned(),
            "reviewer-1".to_owned(),
            "Continue?".to_owned(),
            String::new(),
            Vec::new(),
        );
        questions.put(&mut question).unwrap();

        drive(&opts, &queue, &status_file, &home, &worktrees, &Stop::new())
            .await
            .unwrap();

        assert_eq!(
            questions.get(&question.id).unwrap().status,
            ask::QuestionStatus::Abandoned,
            "an empty --once drain still performs startup question cleanup"
        );
    }

    #[test]
    fn cache_check_due_fires_immediately_then_waits_out_its_own_interval() {
        let t0 = "2026-09-15T00:00:00Z".parse::<Timestamp>().unwrap();

        assert!(
            cache_check_due(None, t0, CACHE_CHECK_INTERVAL_SECS),
            "never checked before: due at once"
        );

        let one_sec_later = t0 + jiff::SignedDuration::from_secs(1);
        assert!(
            !cache_check_due(Some(t0), one_sec_later, CACHE_CHECK_INTERVAL_SECS),
            "well inside the interval: not due yet"
        );

        let at_the_edge = t0 + jiff::SignedDuration::from_secs(CACHE_CHECK_INTERVAL_SECS as i64);
        assert!(
            !cache_check_due(Some(t0), at_the_edge, CACHE_CHECK_INTERVAL_SECS),
            "exactly at the edge: not yet due, same convention as `clean::due`"
        );

        let past_it = t0 + jiff::SignedDuration::from_secs(CACHE_CHECK_INTERVAL_SECS as i64 + 1);
        assert!(
            cache_check_due(Some(t0), past_it, CACHE_CHECK_INTERVAL_SECS),
            "past the interval: due again"
        );
    }

    /// A `magi.toml` whose `[verify] gate` names `cache_dir` as its shared
    /// `CARGO_TARGET_DIR`, capped at `limit_bytes`, plus a repository path
    /// that is never created - the fixtures [`maybe_prune_cache_between_runs`]
    /// and the congestion test below both need, and must not drift apart.
    fn cache_check_opts(dir: &Path, cache_dir: &Path, limit_bytes: u64) -> Opts {
        let config = dir.join("magi.toml");
        // A literal (single-quoted) TOML string, not a basic one: the cache
        // path is a Windows path full of backslashes, and a basic string
        // would have TOML try to interpret `\U` (from `\Users\...`) as a
        // Unicode escape and fail to parse - the same trap `magi.toml`'s own
        // `{{ vars.cache }}` rendering documents.
        std::fs::write(
            &config,
            format!(
                "[disk]\nmin_free_bytes = 0\nauto_fold = false\ncache_limit_bytes = {limit_bytes}\n\n\
                 [verify]\ngate = ['CARGO_TARGET_DIR={} cargo make check']\n",
                cache_dir.display()
            ),
        )
        .unwrap();
        Opts {
            config: Some(config),
            repo: dir.join("repo"),
            ..Opts::default()
        }
    }

    #[tokio::test]
    async fn maybe_prune_cache_between_runs_reprunes_only_once_its_own_interval_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("a"), vec![0u8; 10]).unwrap();
        let opts = cache_check_opts(dir.path(), &cache_dir, 1);

        // Nobody has asked this daemon to stop, which is the ordinary case;
        // the skip that a stop buys is asserted by its own test below.
        let running = Stop::new();
        let mut last_checked = None;
        let t0 = "2026-09-15T00:00:00Z".parse::<Timestamp>().unwrap();
        maybe_prune_cache_between_runs(&opts.repo, &opts, &home, &running, &mut last_checked, t0)
            .await;
        assert_eq!(
            crate::disk::dir_size(&cache_dir),
            0,
            "over the cap on the first check ever: pruned at once, no idle queue required"
        );
        assert_eq!(last_checked, Some(t0));

        // A fresh oversized file lands, but the next check is not due yet.
        std::fs::write(cache_dir.join("b"), vec![0u8; 10]).unwrap();
        let too_soon = t0 + jiff::SignedDuration::from_secs(1);
        maybe_prune_cache_between_runs(
            &opts.repo,
            &opts,
            &home,
            &running,
            &mut last_checked,
            too_soon,
        )
        .await;
        assert_eq!(
            crate::disk::dir_size(&cache_dir),
            10,
            "too soon since the last check: left alone rather than rescanned every call"
        );
        assert_eq!(
            last_checked,
            Some(t0),
            "an idle check does not reset the clock"
        );

        // Once the interval elapses, the same oversized cache is caught again.
        let due_again = t0 + jiff::SignedDuration::from_secs(CACHE_CHECK_INTERVAL_SECS as i64 + 1);
        maybe_prune_cache_between_runs(
            &opts.repo,
            &opts,
            &home,
            &running,
            &mut last_checked,
            due_again,
        )
        .await;
        assert_eq!(
            crate::disk::dir_size(&cache_dir),
            0,
            "due again: pruned back under the cap"
        );
    }

    /// A stop must not queue behind housekeeping. The prune below is a
    /// synchronous walk of the whole cache with no await point in it, so a
    /// loop that entered it could not get back to its own `stopped()` test
    /// until the walk finished - and because no run is in flight at this
    /// boundary, `Stop::finishing` would meanwhile tell the operator's screen
    /// the stop had already landed. The idle branch has always made this same
    /// check before reaching `janitor`; the between-runs path makes it too.
    #[tokio::test]
    async fn a_stop_already_asked_for_skips_the_between_runs_cache_walk() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("a"), vec![0u8; 10]).unwrap();
        let opts = cache_check_opts(dir.path(), &cache_dir, 1);

        let stop = Stop::new();
        stop.stop();
        assert!(
            !stop.finishing(),
            "no run is in flight at a between-runs boundary, so nothing else \
             would tell the operator this stop had not taken effect yet"
        );

        let mut last_checked = None;
        let t0 = "2026-09-15T00:00:00Z".parse::<Timestamp>().unwrap();
        maybe_prune_cache_between_runs(&opts.repo, &opts, &home, &stop, &mut last_checked, t0)
            .await;
        assert_eq!(
            crate::disk::dir_size(&cache_dir),
            10,
            "over its cap, and due for the first check ever, but a stop outranks \
             it: the cap is a standing policy the next start measures again"
        );
        assert_eq!(
            last_checked, None,
            "a check that never happened must not claim the interval"
        );
    }

    /// The regression this whole change exists for: gate timeouts on runs
    /// 52da/2f7f/5991/0915 traced back to the shared cache sitting at 81.8
    /// GiB against a 10 GiB cap, because the operator's queue never had a
    /// quiet moment for `poll`'s fully-idle branch to reach the ordinary
    /// `janitor` pass.
    ///
    /// Reproduced here with a task whose repository is never created:
    /// `Runner::start` fails at `git::toplevel` in a few milliseconds,
    /// spawning no agent CLI, so the task keeps failing and re-queuing
    /// (`Task::fail` with attempts still under the budget leaves it
    /// `Failed`, which `TaskStatus::runnable` still offers) for as long as
    /// the loop keeps polling - exactly the "queue with no idle moment"
    /// this task describes, produced without a real competition.
    #[tokio::test]
    async fn cache_prune_reaches_a_queue_that_never_goes_idle() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("stale"), vec![0u8; 4096]).unwrap();

        let mut opts = cache_check_opts(dir.path(), &cache_dir, 1);
        opts.poll = Duration::from_millis(20);
        opts.max_attempts = 1_000;

        let queue = Queue::at(dir.path().join("queue"));
        let mut t = Task::new(
            "x".to_owned(),
            "x".to_owned(),
            opts.repo.clone(),
            Source::Human,
        );
        queue.put(&mut t).unwrap();

        let home = dir.path().join("home");
        let worktrees = dir.path().join("wt");
        let status_file = home.join("daemon.json");
        let stop = Stop::new();
        let stopper = {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                stop.stop();
            })
        };

        tokio::time::timeout(
            Duration::from_secs(10),
            drive(&opts, &queue, &status_file, &home, &worktrees, &stop),
        )
        .await
        .expect("the loop must not hang on a queue that keeps producing failing work")
        .expect("the loop's own setup and teardown must not fail");
        stopper.await.unwrap();

        let after = queue.get(&t.id).unwrap();
        assert!(
            after.attempts >= 2,
            "the harness must actually have retried more than once, or this is not \
             exercising a busy queue at all (got {} attempt(s))",
            after.attempts
        );
        assert!(
            after.status.runnable(),
            "still under its attempt budget: the queue never reached a natural idle \
             on its own, only the external stop ended the test"
        );

        assert_eq!(
            crate::disk::dir_size(&cache_dir),
            0,
            "an oversized cache must not be left to grow unboundedly just because the \
             queue kept the loop busy the whole time"
        );
    }

    #[test]
    fn task_question_reconciliation_keeps_references_and_retires_manual_releases() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut task = task();
        queue.put(&mut task).unwrap();

        let mut task_question = ask::Question::new(
            task.id.clone(),
            crate::conduct::NODE.to_owned(),
            "conduct".to_owned(),
            "Which backend?".to_owned(),
            String::new(),
            Vec::new(),
        );
        questions.put(&mut task_question).unwrap();
        task.block(vec![task_question.id.clone()], None);
        queue.put(&mut task).unwrap();

        let mut run_question = ask::Question::new(
            "20260101-000000-run1".to_owned(),
            "review".to_owned(),
            "reviewer-1".to_owned(),
            "Run question".to_owned(),
            String::new(),
            Vec::new(),
        );
        questions.put(&mut run_question).unwrap();

        // A question from another node whose `run` happens to equal this
        // task's id — the same field, filled in for an unrelated reason. Only
        // `crate::conduct::NODE` questions use `run` as a task id; this one
        // must never be touched by this reconciliation, even after release.
        let mut coincidental = ask::Question::new(
            task.id.clone(),
            "review".to_owned(),
            "reviewer-1".to_owned(),
            "Unrelated review question".to_owned(),
            String::new(),
            Vec::new(),
        );
        questions.put(&mut coincidental).unwrap();

        reconcile_task_questions(&queue, &questions);
        assert!(questions.get(&task_question.id).unwrap().status.open());
        assert!(questions.get(&run_question.id).unwrap().status.open());
        assert!(questions.get(&coincidental.id).unwrap().status.open());

        task.release();
        queue.put(&mut task).unwrap();
        reconcile_task_questions(&queue, &questions);
        assert_eq!(
            questions.get(&task_question.id).unwrap().status,
            ask::QuestionStatus::Abandoned
        );
        assert!(
            questions.get(&run_question.id).unwrap().status.open(),
            "run questions remain the run janitor's responsibility"
        );
        assert!(
            questions.get(&coincidental.id).unwrap().status.open(),
            "a non-conductor question must not be abandoned just because its \
             run id coincides with a task id"
        );
    }

    #[test]
    fn a_freshly_started_running_task_is_never_stalled() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = task();
        t.start("run-1".to_owned());
        // `updated_at` is `Timestamp::now()`, left alone: no live daemon
        // named in `dir`, but nowhere near `STALLED_RUNNING` yet.
        assert!(!is_stalled(&t, dir.path(), Timestamp::now()));
    }

    #[test]
    fn a_long_running_task_with_no_live_daemon_is_stalled() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = task();
        t.start("run-1".to_owned());
        t.updated_at = Timestamp::now()
            - jiff::SignedDuration::from_secs(STALLED_RUNNING.as_secs() as i64 + 60);
        assert!(is_stalled(&t, dir.path(), Timestamp::now()));
        assert_eq!(
            stalled_tasks(
                &Queue::at(dir.path().join("q")),
                dir.path(),
                Timestamp::now()
            )
            .len(),
            0,
            "the task was never written to this queue"
        );
    }

    #[test]
    fn a_long_running_task_a_live_daemon_still_names_is_not_stalled() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = task();
        t.id = "20260903-080340-0167".to_owned();
        t.start("20260903-080619-01c2".to_owned());
        t.updated_at = Timestamp::now()
            - jiff::SignedDuration::from_secs(STALLED_RUNNING.as_secs() as i64 + 60);

        let mut status = Status::new();
        status.current = vec![Current {
            task: t.id.clone(),
            run: "20260903-080619-01c2".to_owned(),
        }];
        write_status_to(&dir.path().join("daemon.json"), &status).unwrap();

        assert!(
            !is_stalled(&t, dir.path(), Timestamp::now()),
            "a live daemon's own heartbeat rules out stalled, however long the task has run"
        );
    }

    /// Rewrite a task's `updated_at` on disk directly, bypassing
    /// `Queue::put`'s own `Timestamp::now()` stamping - the only way to make
    /// a fixture look like it has genuinely been `running` for a while.
    fn backdate_task(queue: &Queue, id: &str, seconds_ago: i64) {
        let path = queue.path_of(id);
        let body = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let old = Timestamp::now() - jiff::SignedDuration::from_secs(seconds_ago);
        v["updated_at"] = serde_json::Value::String(old.to_string());
        std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }

    #[test]
    fn stalled_tasks_still_reaches_a_task_reclaim_could_not_claim_yet() {
        // The realistic `poll()` ordering, not `is_stalled` in isolation:
        // `reclaim_orphaned_running` runs first, on every poll, and settles
        // any `running` task whose claim it can actually take. For most
        // crashes that is immediate - a dead pid is proof enough for
        // `sweep_stale_claims` to drop the lock the same tick, and the very
        // next claim attempt succeeds. But a lock whose pid cannot be parsed
        // at all falls back to `STALE_CLAIM`'s six-hour age instead (see
        // `sweep_stale_claims`'s own doc), so the lock - and the claim
        // failure behind it - can legitimately outlive many polls. This is
        // exactly the gap `stalled_tasks` exists to surface well before that
        // six-hour sweep would: reclaim leaves the task `running`, and it
        // must still reach the conductor as stalled.
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let home = dir.path().join("home");

        let mut t = task();
        t.id = "20260101-000001-lock".to_owned();
        t.start("run-1".to_owned());
        queue.put(&mut t).unwrap();
        backdate_task(&queue, &t.id, STALLED_RUNNING.as_secs() as i64 + 60);
        std::fs::write(
            dir.path().join("queue").join(format!("{}.lock", t.id)),
            "not a pid",
        )
        .unwrap();

        let now = Timestamp::now();
        assert!(
            reclaim_orphaned_running(&queue, 2).is_empty(),
            "the unparseable lock is still well within STALE_CLAIM, so the claim fails \
             and reclaim must leave the task alone"
        );
        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Running);

        let stalled = stalled_tasks(&queue, &home, now);
        assert_eq!(
            stalled.len(),
            1,
            "reclaim's inability to claim it yet must not hide it from the conductor"
        );
        assert_eq!(stalled[0].id, t.id);
    }

    #[test]
    fn ordinary_dead_daemon_task_is_shown_stalled_before_reclaim_and_can_be_requeued() {
        let dir = tempfile::tempdir().unwrap();
        crate::run::set_home(dir.path().join("run-home"));
        let queue = Queue::at(dir.path().join("queue"));
        let home = dir.path().join("home");
        let questions = Questions::at(dir.path().join("questions"));

        let mut t = task();
        t.id = "20260101-000003-dead".to_owned();
        t.start("missing-run".to_owned());
        queue.put(&mut t).unwrap();
        backdate_task(&queue, &t.id, STALLED_RUNNING.as_secs() as i64 + 60);

        // This is the real poll ordering: retain the deterministic stalled
        // input before a claim proves the owner is gone and reclaims it.
        let stalled = stalled_tasks(&queue, &home, Timestamp::now());
        assert_eq!(
            stalled.iter().map(|task| &task.id).collect::<Vec<_>>(),
            [&t.id]
        );
        assert_eq!(reclaim_orphaned_running(&queue, 2), [t.id.clone()]);
        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Held);

        // Reclaim drops its guard before conductor decisions are applied, so
        // the decision for the captured stalled input has a real write path.
        crate::conduct::apply(
            &queue,
            &questions,
            &crate::conduct::Verdict {
                decisions: vec![crate::conduct::Decision {
                    id: t.id.clone(),
                    recovery: Some(crate::conduct::Recovery::Requeue),
                    ..crate::conduct::Decision::default()
                }],
            },
        )
        .unwrap();
        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Queued);
    }

    #[test]
    fn stalled_tasks_reports_exactly_the_tasks_is_stalled_agrees_on() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let home = dir.path().join("home");

        let mut fresh = task();
        fresh.id = "20260101-000001-aaaa".to_owned();
        fresh.start("run-1".to_owned());
        queue.put(&mut fresh).unwrap();

        let mut old = task();
        old.id = "20260101-000002-bbbb".to_owned();
        old.start("run-2".to_owned());
        queue.put(&mut old).unwrap();
        backdate_task(&queue, &old.id, STALLED_RUNNING.as_secs() as i64 + 60);

        let stalled = stalled_tasks(&queue, &home, Timestamp::now());
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].id, old.id);
    }

    #[test]
    fn queued_and_finished_task_views_partition_by_status() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));

        let mut queued = task();
        queued.id = "20260101-000001-aaaa".to_owned();
        queue.put(&mut queued).unwrap();

        let mut failed = task();
        failed.id = "20260101-000002-bbbb".to_owned();
        failed.start("run-1".to_owned());
        failed.fail("gate red", 5);
        queue.put(&mut failed).unwrap();

        let mut held = task();
        held.id = "20260101-000003-cccc".to_owned();
        held.hold_machine(None);
        queue.put(&mut held).unwrap();

        let mut running = task();
        running.id = "20260101-000004-dddd".to_owned();
        running.start("run-2".to_owned());
        queue.put(&mut running).unwrap();

        let queued_ids: Vec<String> = queued_tasks(&queue).into_iter().map(|t| t.id).collect();
        assert_eq!(queued_ids, [queued.id.clone()]);

        let mut finished_ids: Vec<String> =
            finished_tasks(&queue).into_iter().map(|t| t.id).collect();
        finished_ids.sort_unstable();
        let mut want = vec![failed.id.clone(), held.id.clone()];
        want.sort_unstable();
        assert_eq!(finished_ids, want);
    }

    #[test]
    fn resolve_blockers_clears_a_done_dependency_and_keeps_an_unresolved_one() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = ask::Questions::at(dir.path().join("questions"));

        let mut dep = task();
        dep.id = "20260101-000001-dep0".to_owned();
        dep.succeed();
        queue.put(&mut dep).unwrap();

        let mut still_going = task();
        still_going.id = "20260101-000002-dep1".to_owned();
        queue.put(&mut still_going).unwrap();

        let mut blocked = task();
        blocked.id = "20260101-000003-main".to_owned();
        blocked.block(
            vec![dep.id.clone(), still_going.id.clone()],
            Some("waits on both".to_owned()),
        );
        queue.put(&mut blocked).unwrap();

        resolve_blockers(&queue, &questions);

        let after = queue.get(&blocked.id).unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Blocked,
            "one dependency is still outstanding"
        );
        assert_eq!(after.blocked_by, [still_going.id.clone()]);
    }

    #[test]
    fn resolve_blockers_carries_an_answers_content_onto_the_task_and_unblocks_it() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = ask::Questions::at(dir.path().join("questions"));

        let mut q = crate::ask::Question::new(
            "20260101-000001-main".to_owned(),
            crate::conduct::NODE.to_owned(),
            "conduct".to_owned(),
            "Which backend?".to_owned(),
            String::new(),
            Vec::new(),
        );
        questions.put(&mut q).unwrap();
        q.answer(crate::ask::Answer::Text("SQLite".to_owned()))
            .unwrap();
        questions.put(&mut q).unwrap();

        let mut blocked = task();
        blocked.id = "20260101-000001-main".to_owned();
        blocked.block(vec![q.id.clone()], Some("which backend?".to_owned()));
        queue.put(&mut blocked).unwrap();

        resolve_blockers(&queue, &questions);

        let after = queue.get(&blocked.id).unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Queued,
            "the only blocker resolved"
        );
        assert_eq!(after.answers.len(), 1);
        assert_eq!(after.answers[0].question, "Which backend?");
        assert_eq!(after.answers[0].answer, "SQLite");

        // And the run this task starts next is told about it.
        let instruction = instruction_for(&after);
        assert!(instruction.contains("Which backend?"));
        assert!(instruction.contains("SQLite"));
    }

    #[test]
    fn resolve_blockers_restores_a_held_task_to_held_instead_of_queuing_it() {
        // Reproduces the reported bug (task 3958): a task held out of
        // attempts, blocked on a `crate::conduct` follow-up question, must
        // come back `held` once that question is answered - never `queued`,
        // whatever the answer said - or it silently re-enters the
        // competition queue with its attempts already exhausted.
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = ask::Questions::at(dir.path().join("questions"));

        let mut q = crate::ask::Question::new(
            "20260101-000001-main".to_owned(),
            crate::conduct::NODE.to_owned(),
            "conduct".to_owned(),
            "How should this be handled?".to_owned(),
            String::new(),
            Vec::new(),
        );
        questions.put(&mut q).unwrap();
        q.answer(crate::ask::Answer::Text(
            "leave it held, a human will look at it later".to_owned(),
        ))
        .unwrap();
        questions.put(&mut q).unwrap();

        let mut held = task();
        held.id = "20260101-000001-main".to_owned();
        held.hold_machine(Some("out of attempts".to_owned()));
        held.block(vec![q.id.clone()], Some("what now?".to_owned()));
        queue.put(&mut held).unwrap();

        resolve_blockers(&queue, &questions);

        let after = queue.get(&held.id).unwrap();
        assert_eq!(after.status, TaskStatus::Held);
        assert_eq!(after.hold_reason.as_deref(), Some("out of attempts"));
        assert_eq!(
            after.answers[0].answer,
            "leave it held, a human will look at it later"
        );
    }

    #[test]
    fn resolve_blockers_holds_a_task_whose_dependency_was_deleted() {
        // Reproduces the reported bug: a task blocked on a task id that was
        // removed (`magi task rm`, or deleted by hand) can never see that id
        // reach `Done`, so the ordinary per-id loop has nothing to notice and
        // would otherwise leave the task `blocked` forever with no way for an
        // operator to find out why.
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = ask::Questions::at(dir.path().join("questions"));

        let mut still_going = task();
        still_going.id = "20260101-000002-dep1".to_owned();
        queue.put(&mut still_going).unwrap();

        let mut blocked = task();
        blocked.id = "20260101-000003-main".to_owned();
        blocked.block(
            vec!["20260101-000001-gone".to_owned(), still_going.id.clone()],
            Some("waits on both".to_owned()),
        );
        queue.put(&mut blocked).unwrap();

        resolve_blockers(&queue, &questions);

        let after = queue.get(&blocked.id).unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Held,
            "a missing dependency must not leave the task blocked forever"
        );
        assert_eq!(after.hold_source, Some(crate::queue::HoldSource::Machine));
        assert!(after.blocked_by.is_empty());
        let reason = after.hold_reason.as_deref().unwrap_or_default();
        assert!(
            reason.contains("20260101-000001-gone"),
            "the missing id must be named so an operator can tell what happened: {reason}"
        );
        assert!(
            reason.contains(&still_going.id),
            "the still-valid dependency must not silently vanish from the record: {reason}"
        );
    }

    #[test]
    fn instruction_for_is_unchanged_without_any_answers() {
        let t = task();
        assert_eq!(instruction_for(&t), t.instruction);
    }

    #[test]
    fn resumed_instruction_is_unchanged_without_any_answers() {
        let t = task();
        assert_eq!(resumed_instruction(&t.instruction, &t), t.instruction);
    }

    #[test]
    fn resumed_instruction_carries_a_new_answer_onto_the_old_run() {
        let mut t = task();
        t.record_answer("Which backend?".to_owned(), "SQLite".to_owned());
        // The run's own instruction on disk predates the answer: it is the
        // plain original text `Runner::start` saved before the operator was
        // ever asked anything.
        let old = t.instruction.clone();

        let refreshed = resumed_instruction(&old, &t);
        assert!(refreshed.starts_with(&old), "the original text is kept");
        assert!(refreshed.contains("Which backend?"));
        assert!(refreshed.contains("SQLite"));
    }

    #[test]
    fn resumed_instruction_keeps_an_original_answers_heading() {
        let mut t = task();
        t.instruction = "Context\n\n# Operator answers\n\nThis is part of the task.".to_owned();
        t.record_answer("Which backend?".to_owned(), "SQLite".to_owned());

        let refreshed = resumed_instruction(&t.instruction, &t);

        assert!(
            refreshed.starts_with(&t.instruction),
            "an answers heading in the original instruction is not the appended block"
        );
        assert_eq!(refreshed.matches(ANSWERS_HEADER).count(), 2);
        assert!(refreshed.contains("Which backend?"));
        assert!(refreshed.contains("SQLite"));

        let repeated = resumed_instruction(&refreshed, &t);
        assert_eq!(
            repeated, refreshed,
            "only the final appended block is refreshed"
        );
    }

    #[test]
    fn resumed_instruction_does_not_duplicate_across_repeated_resumes() {
        let mut t = task();
        t.record_answer("Which backend?".to_owned(), "SQLite".to_owned());

        // A first resume appends the block; a second resume of the same run,
        // with no new answer in between, must reproduce exactly the same
        // text rather than appending the block a second time.
        let once = resumed_instruction(&t.instruction, &t);
        let twice = resumed_instruction(&once, &t);
        assert_eq!(once, twice);
        assert_eq!(once.matches("Which backend?").count(), 1);

        // A later answer replaces the block wholesale rather than growing it.
        t.record_answer("Which cache?".to_owned(), "Redis".to_owned());
        let refreshed = resumed_instruction(&once, &t);
        assert_eq!(refreshed.matches(ANSWERS_HEADER).count(), 1);
        assert!(refreshed.contains("Which backend?"));
        assert!(refreshed.contains("Which cache?"));
    }

    #[test]
    fn prepare_instruction_covers_all_three_starters() {
        let mut t = task();
        t.record_answer("Which backend?".to_owned(), "SQLite".to_owned());

        // Start: a fresh run gets the task text plus every answer so far —
        // exactly `instruction_for`.
        assert_eq!(
            prepare_instruction(&Starter::Start, None, &t),
            Some(instruction_for(&t))
        );

        // Resume: the run's prior instruction is refreshed with the answer,
        // not discarded and not left stale.
        let old = t.instruction.clone();
        assert_eq!(
            prepare_instruction(&Starter::Resume("some-run".to_owned()), Some(&old), &t),
            Some(resumed_instruction(&old, &t))
        );

        // Review: a review-only pass builds its own instruction from the
        // branch's history in `crate::graph`, with no task statement at all -
        // this boundary must leave it alone.
        assert_eq!(
            prepare_instruction(&Starter::Review("magi/eba2/A".to_owned()), Some(&old), &t),
            None
        );
    }

    #[test]
    fn choose_starter_prefers_review_over_resume_when_the_branch_survived() {
        assert_eq!(
            choose_starter(Some("magi/eba2/A"), true, Some("some-run")),
            Starter::Review("magi/eba2/A".to_owned())
        );
    }

    #[test]
    fn choose_starter_falls_back_to_start_when_the_review_branch_is_gone() {
        assert_eq!(
            choose_starter(Some("magi/eba2/A"), false, Some("some-run")),
            Starter::Start,
            "a vanished review branch must not fall back to resuming the old run either"
        );
    }

    #[test]
    fn choose_starter_resumes_or_starts_when_there_is_no_review_choice_at_all() {
        assert_eq!(
            choose_starter(None, false, Some("some-run")),
            Starter::Resume("some-run".to_owned())
        );
        assert_eq!(choose_starter(None, false, None), Starter::Start);
    }

    #[test]
    fn an_explicit_release_forces_a_fresh_competition_even_with_a_resumable_run() {
        let mut released = task();
        released.start("stalled-run".to_owned());
        released.requeue();
        let unfinished = (!released.fresh_start)
            .then(|| Some("stalled-run".to_owned()))
            .flatten();
        assert_eq!(
            choose_starter(None, false, unfinished.as_deref()),
            Starter::Start,
            "release keeps run history but must not resume it"
        );
        assert_eq!(released.runs, ["stalled-run"]);
    }

    #[test]
    fn an_ordinary_release_keeps_a_resumable_run_available() {
        let mut released = task();
        released.start("stalled-run".to_owned());
        released.release();
        let unfinished = (!released.fresh_start)
            .then(|| Some("stalled-run".to_owned()))
            .flatten();
        assert_eq!(
            choose_starter(None, false, unfinished.as_deref()),
            Starter::Resume("stalled-run".to_owned()),
            "manual release must preserve the normal resume path"
        );
    }

    #[test]
    fn a_blocked_run_that_spent_every_review_round_has_exhausted_its_budget() {
        let mut state = run_state(RunStatus::Blocked);
        state.config.graph.review_rounds = 3;
        state.reviews = vec![review_round(1), review_round(2), review_round(3)];
        assert!(exhausted_review_budget(&state));

        // One round still unused: resuming can still ask a reviewer something.
        state.reviews.pop();
        assert!(!exhausted_review_budget(&state));

        // Exhausted rounds on a non-`Blocked` status (a stall, say) do not
        // count: only a `Blocked` run re-enters the review loop on resume.
        let mut stalled = run_state(RunStatus::Stalled);
        stalled.config.graph.review_rounds = 1;
        stalled.reviews = vec![review_round(1)];
        assert!(!exhausted_review_budget(&stalled));
    }

    fn review_round(round: usize) -> crate::run::ReviewRound {
        crate::run::ReviewRound {
            round,
            head: "deadbeef".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 1,
            expected: 1,
            clean: false,
            progressed: true,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }
    }

    #[test]
    fn unfinished_run_skips_a_round_exhausted_blocked_run_so_requeue_means_a_fresh_competition() {
        // Mirrors the failure this exists to close: a task's last run ended
        // `Blocked` with the review budget spent, `crate::conduct` chose
        // `Recovery::Requeue` (`Task::release`, which keeps `runs` as
        // evidence), and without this check `attempt` would go on treating
        // that exhausted run as "unfinished" and resume it - `graph::Runner`'s
        // review loop iterates zero times over an already-spent budget, so
        // the resumed run settles right back to `Blocked` having asked nobody
        // anything, and `Requeue`'s promised fresh competition never happens.
        let mut exhausted = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        );
        exhausted.status = RunStatus::Blocked;
        exhausted.config.graph.review_rounds = 1;
        exhausted.reviews = vec![review_round(1)];

        assert_eq!(
            unfinished_run_with(&[exhausted.id.clone()], "t", |_| Ok(exhausted.clone())),
            None,
            "an exhausted `Blocked` run must not be offered as resumable"
        );

        // A `Blocked` run with rounds still unused is genuinely worth
        // resuming, and must still be found.
        let mut has_budget_left = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        );
        has_budget_left.status = RunStatus::Blocked;
        has_budget_left.config.graph.review_rounds = 3;
        has_budget_left.reviews = vec![review_round(1)];

        assert_eq!(
            unfinished_run_with(&[has_budget_left.id.clone()], "t", |_| {
                Ok(has_budget_left.clone())
            }),
            Some(has_budget_left.id.clone())
        );
    }

    #[test]
    fn unfinished_run_never_falls_back_to_an_older_resumable_run() {
        // A task whose history holds an *older* run that still looks
        // resumable (say, a competition `Runner::review` was started
        // alongside after that older run went `Stalled`) and a *newest* run
        // that is `Blocked` with its review budget spent. `Recovery::Requeue`
        // on this task must mean a fresh competition — falling back to the
        // stale, superseded `Stalled` run instead would resurrect history
        // nothing asked to revisit and silently defeat the requeue.
        let mut older_stalled = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        );
        older_stalled.status = RunStatus::Stalled;

        let mut newest_exhausted = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        );
        newest_exhausted.status = RunStatus::Blocked;
        newest_exhausted.config.graph.review_rounds = 1;
        newest_exhausted.reviews = vec![review_round(1)];

        assert_eq!(
            unfinished_run_with(
                &[older_stalled.id.clone(), newest_exhausted.id.clone()],
                "t",
                |_| Ok(newest_exhausted.clone())
            ),
            None,
            "the newest run is exhausted, so nothing here is worth resuming - \
             least of all the older, already-superseded run"
        );
    }

    #[test]
    fn unfinished_run_warns_and_skips_a_run_it_cannot_read() {
        assert_eq!(
            unfinished_run_with(&["20260101-000000-gone".to_owned()], "t", |_| {
                Err(anyhow::anyhow!("fixture is absent"))
            }),
            None
        );
    }
}
