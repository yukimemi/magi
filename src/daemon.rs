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
                Some(pid) => !crate::proc::pid_alive(pid),
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
fn reconcile_task_questions(queue: &Queue, questions: &Questions) {
    let tasks = queue.list();
    let referenced: std::collections::BTreeSet<&str> = tasks
        .iter()
        .flat_map(|task| task.blocked_by.iter().map(String::as_str))
        .collect();

    for task in &tasks {
        for mut question in questions.open_for(&task.id) {
            // Keep questions a task still names, including when the
            // reference moved to a dependent task.  A question whose `run`
            // is not a current task id is a normal run question and never
            // enters this loop.
            if referenced.contains(question.id.as_str()) {
                continue;
            }
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
/// | anything non-terminal                  | `Failed`, or `Held`  | yes          |
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
            task.hold(Some(why.to_owned()));
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
    let concurrency = max_concurrent(
        prepare(&opts.repo, opts)
            .map(|c| c.daemon.max_concurrent_runs)
            .unwrap_or(1),
    );

    tracing::info!(
        "magi serve: queue {} (poll {}s, {} attempts per task, {} run(s) at once)",
        queue.root().display(),
        opts.poll.as_secs(),
        opts.max_attempts,
        concurrency
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
        concurrency,
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

/// Poll the queue until stopped, factored out so [`drive`] owns only setup and
/// teardown and cannot skip the teardown on an early return.
///
/// `max_concurrent` bounds how many *ordinary* candidates run at once - see
/// [`crate::config::Daemon::max_concurrent_runs`]. A run parked on a land
/// approval that has since been answered is dispatched outside that bound
/// the moment [`land_resume_state`] reports it [`LandResume::Ready`]: the
/// whole point of parking there is that it must not queue behind whatever
/// else the loop happens to be running, even at the default of one.
async fn poll(
    opts: &Opts,
    queue: &Queue,
    status: &Arc<Mutex<Status>>,
    home: &Path,
    worktrees_root: &Path,
    stop: &Stop,
    max_concurrent: usize,
) -> Result<()> {
    // Only consulted by `once`, where a task that just failed is still
    // `runnable` and would otherwise be picked up again inside the same drain.
    // In the long-running mode a later poll retrying a failed task is the point,
    // and the attempt counter is what bounds it.
    let mut attempted: Vec<String> = Vec::new();
    let sem = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    // A quota hit is a fact about the machine, not the task that happened to
    // surface it, and every other *ordinary* candidate is no less likely to
    // hit the same wall - see the warning below. A land-merge resume is
    // exempt: it is a human decision finishing, not a fresh competition, and
    // must not sit out a quota cooldown it did not cause.
    let quota_cooldown_until: Arc<Mutex<Option<Timestamp>>> = Arc::new(Mutex::new(None));
    let mut inflight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    let mut conductor = Conductor::new();

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
            let permit = if priority {
                None
            } else {
                match Arc::clone(&sem).try_acquire_owned() {
                    Ok(p) => Some(p),
                    // No ordinary slot free right now. A later candidate in
                    // this same list might still be a priority resume, so
                    // keep looking rather than stopping here.
                    Err(_) => continue,
                }
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
                let quota = attempt(&opts, &queue, &status, &stop, &mut task).await;
                lock(&status).completed += 1;
                // A quota loss is a fact about the machine, not this task, and
                // the next ordinary candidate the loop offers is no less
                // likely to hit the same wall: without a cooldown here a
                // whole backlog can be run - and failed - in the seconds it
                // takes each attempt to notice the CLI is out of quota.
                if !quota.is_empty() {
                    let hint = quota.iter().find_map(|q| q.reset.as_deref());
                    let reset_at = hint.and_then(|h| parse_reset_hint(h, Timestamp::now()));
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
        task.hold(Some(reason.clone()));
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
            Runner::resume(id)
        }
        Starter::Start => {
            if let Some(branch) = &review_branch {
                tracing::warn!(
                    "conductor chose review for task {} but branch `{branch}` no longer \
                     exists; requeuing as a fresh competition instead",
                    task.short()
                );
            }
            Runner::start(&repo, instruction_for(task), config).await
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

/// The free-space gate: what stands between this task and a new run, if
/// anything. `Some(reason)` holds the task; `None` lets it start.
///
/// A zero [`Config::disk::min_free_bytes`] opens the gate unconditionally -
/// the operator opted out. A measurement failure is a gate, not a pass: both
/// sides of "cannot tell" are served by not starting.
fn disk_gate(repo: &Path, config: &Config) -> Option<String> {
    let min = config.disk.min_free_bytes;
    if min == 0 {
        return None;
    }
    match crate::disk::free_bytes(repo) {
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
/// recognises the one shape actually observed in the wild, `"H:MMam/pm
/// (Zone)"`, and returns `None` for anything else rather than guess at a
/// format nobody has seen. A clock reading already past today is read as
/// tomorrow's: a CLI naming a same-day reset that has already gone by means
/// the window rolled over while nothing was watching.
fn parse_reset_hint(text: &str, now: Timestamp) -> Option<Timestamp> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let zone = text[open + 1..close].trim();
    let clock = text[..open].trim().to_lowercase();
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

/// The instruction handed to `Runner::start`: the task's own text, plus any
/// operator answers `crate::conduct` collected for it (see
/// [`Task::answers`]), so a decision the operator actually made reaches the
/// implementers rather than only clearing the block that was waiting on it.
///
/// Appended rather than merged into [`Task::instruction`] itself, so the
/// task's own record stays exactly what its author wrote.
fn instruction_for(task: &Task) -> String {
    if task.answers.is_empty() {
        return task.instruction.clone();
    }
    let mut s = task.instruction.clone();
    s.push_str("\n\n# Operator answers\n\n");
    for a in &task.answers {
        s.push_str(&format!("- {}: {}\n", a.question, a.answer));
    }
    s
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
    // the job does not read as an unexplained failure.
    if state.viable().is_empty() {
        for c in &state.candidates {
            if !c.summary.trim().is_empty() {
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

/// Stable lower-case name for a run status, for logs and task errors.
/// One definition of a status's name, on the type that owns it: this table
/// used to live here as a second copy, and a status renamed in one place would
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
        held.hold(None);
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

    /// A pid past any real process table, but not `u32::MAX`: Windows'
    /// `tasklist` answers that one with "invalid query" rather than "no such
    /// process", which [`crate::proc::pid_alive`] - correctly - cannot tell
    /// apart from a check it simply could not run, so it would read as
    /// alive. See `proc::tests` for the same choice made for the same
    /// reason.
    const DEAD_PID: u32 = 999_999_999;

    #[test]
    fn a_lock_naming_a_dead_pid_is_swept_at_once_regardless_of_age() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().to_path_buf());
        let mut t = task();
        t.id = "20260101-000000-dead".to_owned();
        queue.put(&mut t).unwrap();

        // Written directly rather than through `Queue::claim`, which would
        // stamp this test process's own very much alive pid and defeat the
        // point: this is what a `.lock` left by a `SIGKILL`ed daemon looks
        // like moments after it died, not six hours later.
        std::fs::write(
            dir.path().join(format!("{}.lock", t.id)),
            DEAD_PID.to_string(),
        )
        .unwrap();

        let swept = sweep_stale_claims(&queue, Duration::from_secs(6 * 60 * 60));
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
            DEAD_PID.to_string(),
        )
        .unwrap();

        // Tick two, standing in for a poll long into this daemon's uptime:
        // the same function, called again, notices what only just appeared -
        // proving the sweep is not a one-shot startup check.
        let swept = sweep_stale_claims(&queue, Duration::from_secs(6 * 60 * 60));
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

        // The crashed daemon's own claim, naming a pid nothing on the
        // machine holds anymore.
        std::fs::write(
            dir.path().join(format!("{}.lock", t.id)),
            DEAD_PID.to_string(),
        )
        .unwrap();

        // Before the lock is swept the task looks claimed, and
        // `reclaim_orphaned_running` must leave it alone - this is exactly
        // the bug: a `running` task stranded behind a dead daemon's lock,
        // invisible to the claim-as-proof check because the lock outlived
        // the process that wrote it.
        assert!(reclaim_orphaned_running(&queue, 2).is_empty());
        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Running);

        let swept = sweep_stale_claims(&queue, Duration::from_secs(6 * 60 * 60));
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
            },
            CommandOutcome {
                command: "cargo test".to_owned(),
                code: Some(101),
                output_tail: "thread 'x' panicked: assertion failed".to_owned(),
                duration_ms: 0,
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
            },
            CommandOutcome {
                command: "cargo clippy".to_owned(),
                code: Some(1),
                output_tail: "y".repeat(50_000),
                duration_ms: 0,
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

        let at = parse_reset_hint("4:50am (UTC)", now).expect("a recognised shape parses");
        assert_eq!(at.to_string(), "2026-09-07T04:50:00Z");

        // Same clock reading, but it has already gone by today: read as
        // tomorrow's, since the CLI would not still be reporting a limit past
        // its own stated reset.
        let already_past =
            parse_reset_hint("1:00am (UTC)", now).expect("a recognised shape parses");
        assert_eq!(already_past.to_string(), "2026-09-08T01:00:00Z");

        assert!(
            parse_reset_hint("session limit reached", now).is_none(),
            "free text with no recognised shape is not guessed at"
        );
        assert!(
            parse_reset_hint("4:50am (Nowhere/Fake)", now).is_none(),
            "an unresolvable zone name is not guessed at either"
        );
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
    fn task_question_reconciliation_keeps_references_and_retires_manual_releases() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut task = task();
        queue.put(&mut task).unwrap();

        let mut task_question = ask::Question::new(
            task.id.clone(),
            "conductor".to_owned(),
            "conductor".to_owned(),
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

        reconcile_task_questions(&queue, &questions);
        assert!(questions.get(&task_question.id).unwrap().status.open());
        assert!(questions.get(&run_question.id).unwrap().status.open());

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
        held.hold(None);
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
    fn instruction_for_is_unchanged_without_any_answers() {
        let t = task();
        assert_eq!(instruction_for(&t), t.instruction);
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
            reviews: Vec::new(),
            e2e: Vec::new(),
            verify_retried: false,
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
