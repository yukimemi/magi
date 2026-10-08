//! Self-update, via `kaishin`.
//!
//! A magi run takes minutes of agent latency, so a background release check
//! costs nothing measurable: it is spawned on the same tokio runtime as the
//! command, overlaps it, and is drained with a bounded wait at shutdown. It
//! never delays the graph.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::config::{Update, UpdateMode};

/// Env kill-switch. Any non-empty value other than `0` / `false` disables the
/// background check, and it is read before the config so a broken `magi.toml`
/// cannot force a network call.
pub const NO_AUTOUPDATE_ENV: &str = "MAGI_NO_AUTOUPDATE";

/// Default interval between checks.
pub fn default_interval() -> Duration {
    kaishin::default_interval()
}

/// Floor under [`effective_interval`], matching GitHub's unauthenticated rate
/// limit for the releases API (60 requests/hour/address).
///
/// A one-off CLI invocation honouring a shorter configured interval could
/// only ever make one call per process, so it was never at risk of tripping
/// that limit on its own. The background recheck loop in `web.rs` is
/// different: it polls for as long as `magi web` stays up, so an interval
/// configured well below an hour would have it repeat the same call for as
/// long as the deck runs - a shared floor here is what keeps it, and every
/// other caller of [`Checker::new`], inside the budget regardless.
const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// The interval `cfg` configures, floored at [`MIN_INTERVAL`], or
/// [`default_interval`] when unset or unparsable.
///
/// Shared by [`Checker::new`], which throttles the network call itself
/// against it, and the background recheck loop in `web.rs`, which uses it to
/// decide how often to even ask - it cannot track an interval it never sees.
pub fn effective_interval(cfg: &Update) -> Duration {
    let interval = cfg
        .interval
        .as_deref()
        .and_then(|s| kaishin::parse_interval(s).ok())
        .unwrap_or_else(default_interval);
    interval.max(MIN_INTERVAL)
}

/// Is the background check switched off by the environment?
pub fn disabled_by_env() -> bool {
    match std::env::var(NO_AUTOUPDATE_ENV) {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

/// GitHub owner.
const OWNER: &str = "yukimemi";
/// GitHub repository — *not* `CARGO_PKG_NAME`, which is the published package.
const REPO: &str = "magi";
/// Binary inside the release asset.
const BIN: &str = "magi";
/// Published package name, for kaishin's `cargo install` fallback.
const CRATE: &str = "magi-cli";

/// This binary's own repository name, for `--repo .` auto-discovery
/// (`web::serve`, `main::resolve_repo`) to match a checkout against without
/// a second, driftable copy of [`REPO`] anywhere else.
pub fn repo_name() -> &'static str {
    REPO
}

/// kaishin options.
///
/// All four names are spelled out because three of them differ from
/// `CARGO_PKG_NAME`: the package is `magi-cli` (the short name is a squatted
/// placeholder on crates.io) while the repo, the binary and the library are
/// `magi`. Deriving any of these from `CARGO_PKG_NAME` would send the updater
/// looking for a `yukimemi/magi-cli` repository that does not exist.
fn options() -> kaishin::KaishinOptions {
    kaishin::KaishinOptions::new(OWNER, REPO, BIN, env!("CARGO_PKG_VERSION")).crate_name(CRATE)
}

/// Throttle bookkeeping is transient, so it belongs in the cache dir rather
/// than beside the run history in the data dir.
fn state_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("magi").join("last_update_check.json"))
}

/// `magi self-update`.
pub async fn run_self_update(yes: bool, check_only: bool, non_interactive: bool) -> Result<()> {
    let opts = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only)
        .non_interactive(non_interactive);
    kaishin::run_self_update(&options(), opts).await
}

/// A background update check, resolved at shutdown.
pub enum Pending {
    /// A previous run already found a newer release; just print the banner.
    Cached {
        /// For [`Checker::format_banner`].
        checker: Checker,
        /// The release found earlier.
        latest: kaishin::LatestRelease,
    },
    /// A notify-mode check is in flight.
    Notify {
        /// For [`Checker::format_banner`].
        checker: Checker,
        /// The spawned task.
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>>>,
    },
    /// An install-mode update is in flight.
    Install {
        /// The spawned task.
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>>>,
    },
}

/// Throttled release checker.
#[derive(Clone)]
pub struct Checker {
    inner: kaishin::Checker,
}

impl Checker {
    /// Build a checker honouring `cfg`, or `None` when checking is off.
    ///
    /// The `Option` had no `None` arm: every caller that asked for a checker
    /// got one, so `[update] mode = "off"` was honoured by the *notify* path
    /// alone (see [`cached_update`], which matches on the mode itself) and
    /// ignored everywhere else. `POST /api/upgrade` therefore called the
    /// GitHub releases API on a deck configured never to check - and so did
    /// every unit test that reached that route, unauthenticated, against
    /// GitHub's 60-per-hour-per-address limit.
    ///
    /// An operator who writes `mode = "off"` means it. The button is still
    /// theirs to press; what it may not do is go to the network behind a
    /// configuration that says not to.
    pub fn new(cfg: &Update) -> Option<Self> {
        if cfg.mode == UpdateMode::Off {
            return None;
        }
        let mut inner = kaishin::Checker::new(BIN, options());
        if let Some(path) = state_path() {
            inner = inner.state_path(path);
        }
        Some(Self {
            inner: inner.interval(effective_interval(cfg)),
        })
    }

    /// Is a check due?
    pub fn should_check(&self) -> bool {
        self.inner.should_check()
    }

    /// Ask the forge now: is there a release newer than this build?
    ///
    /// Unlike [`Checker::cached_update`] this method does not consult
    /// [`Checker::should_check`] itself - it has two callers, and they throttle
    /// differently. `POST /api/upgrade` calls it unconditionally, because the
    /// caller there is an operator who just pressed a button and is owed an
    /// answer about the state of the world rather than about the last time
    /// magi looked. `magi web`'s background recheck (`web::run_update_recheck`)
    /// calls [`Checker::should_check`] itself first and only reaches here when
    /// it says yes, which is what keeps that task's network use to at most
    /// once per `[update] interval` no matter how often it polls.
    pub async fn newer_release(&self) -> Result<Option<kaishin::LatestRelease>> {
        self.inner.check_and_save().await
    }

    /// A newer release already known from a previous run.
    pub fn cached_update(&self) -> Option<kaishin::LatestRelease> {
        self.inner.cached_update()
    }

    /// One-line "a newer version exists" banner.
    pub fn format_banner(&self, latest: &kaishin::LatestRelease) -> String {
        self.inner.format_banner(latest)
    }

    /// A checker over an explicit state path and interval, for a test that
    /// must control throttle timing without touching the operator's real
    /// cache directory - see [`state_path`] for why sharing it would be
    /// unsafe.
    #[cfg(test)]
    pub(crate) fn for_test(interval: Duration, state_path: PathBuf) -> Self {
        let opts = kaishin::KaishinOptions::new(OWNER, REPO, BIN, env!("CARGO_PKG_VERSION"));
        Self {
            inner: kaishin::Checker::new(BIN, opts)
                .state_path(state_path)
                .interval(interval),
        }
    }
}

/// How far a self-upgrade this deck set in motion has gotten.
///
/// `POST /api/upgrade` answers `202` and returns immediately - see its own
/// doc for why - so [`Progress`] is the only way a phone that asked for an
/// upgrade learns anything about it afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Downloading the release asset and replacing the binary. This bundles
    /// what would otherwise be two stages: kaishin re-confirms the release
    /// and downloads it inside one `await` with no hook to split, so from
    /// here a phone cannot tell "still checking" from "still downloading" -
    /// only that nothing has been swapped in yet. The confirmation that ran
    /// *before* this stage started is already known to the phone: it is what
    /// the `to` version in the `202` answered with.
    Downloading,
    /// The new binary is in place and [`crate::web`]'s handover has been
    /// signalled, but has not acted yet.
    Replaced,
    /// The handover is waiting for the run in flight, if any, to reach its
    /// next node boundary. The deck answers throughout this - it is not the
    /// unreachable gap, see `web::hand_over`.
    Parking,
    /// The listener has been released and the successor is starting. This is
    /// the one genuinely unreachable moment, and it is meant to be
    /// sub-second - see `web::bind_waiting`.
    Restarting,
    /// A successor came up and confirmed it is running the release this
    /// upgrade asked for.
    Done,
    /// The upgrade did not reach [`Stage::Done`]. `detail` on [`Progress`]
    /// says why.
    Failed,
}

impl Stage {
    /// Position in the upgrade's progression. Only meaningful for the
    /// non-terminal stages; a terminal one ends the upgrade.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Downloading => 0,
            Self::Replaced => 1,
            Self::Parking => 2,
            Self::Restarting => 3,
            Self::Done | Self::Failed => 4,
        }
    }

    /// Finished, one way or the other - nothing is still moving.
    #[must_use]
    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// One upgrade's progress, persisted at [`progress_path`].
///
/// Kept on disk rather than in memory because the process that finishes an
/// upgrade is never the one that started it: the successor is a fresh binary
/// (see `web::spawn_successor`), and the only thing the two share is the
/// disk. Beside the run history rather than under the cache dir alongside
/// [`state_path`]: this is not throttle bookkeeping, it is the record of one
/// upgrade the operator asked for, and - like a parked run - it is meant to
/// outlive the process that wrote it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    /// Where this upgrade has gotten to.
    pub stage: Stage,
    /// Version this upgrade started from.
    pub from: String,
    /// Version it is replacing itself with.
    pub to: Option<String>,
    /// The run [`Stage::Parking`] is waiting on, when one was in flight.
    #[serde(default)]
    pub parked_run: Option<String>,
    /// When this upgrade was asked for.
    pub started_at: Timestamp,
    /// Last time `stage` changed.
    pub updated_at: Timestamp,
    /// Why [`Stage::Failed`] happened; `None` for every other stage.
    #[serde(default)]
    pub detail: Option<String>,
}

impl Progress {
    /// A fresh record for an upgrade that is about to replace the binary.
    #[must_use]
    pub fn new(from: String, to: String) -> Self {
        let now = Timestamp::now();
        Self {
            stage: Stage::Downloading,
            from,
            to: Some(to),
            parked_run: None,
            started_at: now,
            updated_at: now,
            detail: None,
        }
    }

    /// Move to `stage`, stamping when it changed.
    pub fn advance(&mut self, stage: Stage) {
        self.stage = stage;
        self.updated_at = Timestamp::now();
        // A note belongs to the stage it was written for.
        self.detail = None;
    }

    /// Stop at [`Stage::Failed`], with a reason a human can read.
    pub fn fail(&mut self, detail: impl Into<String>) {
        self.stage = Stage::Failed;
        self.updated_at = Timestamp::now();
        self.detail = Some(detail.into());
    }
}

/// Where [`Progress`] is recorded: beside `daemon.json`, not under the cache
/// dir - see [`Progress`]'s own doc for why the two are not the same place.
#[must_use]
pub fn progress_path(home: &Path) -> PathBuf {
    home.join("upgrade.json")
}

/// Serialises the read-compare-rename of [`write_progress`], so two writers
/// in this process cannot each compare against the same stale record.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What to persist when `candidate` is written over `current`.
///
/// An upgrade in progress never goes backwards: a candidate whose stage is
/// not past the recorded one (a second `POST /api/upgrade` landing on a live
/// handover wrote `replaced` over `parking`) keeps the recorded stage and
/// timestamps and only refreshes `to`. A terminal record is over, so whatever
/// comes next starts a new upgrade, and a candidate that ends the upgrade
/// (`Done` / `Failed`) always goes through.
#[must_use]
pub fn monotonic(current: Option<&Progress>, candidate: &Progress) -> Progress {
    match current {
        Some(cur)
            if !cur.stage.terminal()
                && !candidate.stage.terminal()
                && candidate.stage.rank() <= cur.stage.rank() =>
        {
            let mut kept = cur.clone();
            if candidate.to.is_some() {
                kept.to.clone_from(&candidate.to);
            }
            kept
        }
        _ => candidate.clone(),
    }
}

/// Persist `progress`, atomically, never moving an upgrade in progress back
/// to an earlier stage - see [`monotonic`].
///
/// Written to a sibling `.tmp` and renamed, the same reason
/// `daemon::write_status_to` does it: `/api/health` reads this file on every
/// poll and must never see a half-written one.
pub fn write_progress(home: &Path, progress: &Progress) -> Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    write_progress_locked(home, progress)
}

fn write_progress_locked(home: &Path, progress: &Progress) -> Result<()> {
    let path = progress_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let to_write = monotonic(read_progress(home).as_ref(), progress);
    let body = serde_json::to_string_pretty(&to_write).context("serialize upgrade progress")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Record `detail` as the failure of the upgrade request that is asking,
/// unless a handover is already in flight (`parking` / `restarting`): that
/// handover is not this request's to fail. The check and the write happen
/// under the same lock as [`write_progress`], so a handover that records
/// `parking` in between cannot be overwritten. Returns whether it was written.
pub fn fail_progress(home: &Path, detail: &str) -> Result<bool> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut progress) = read_progress(home) else {
        return Ok(false);
    };
    if matches!(progress.stage, Stage::Parking | Stage::Restarting) {
        return Ok(false);
    }
    progress.fail(detail);
    write_progress_locked(home, &progress)?;
    Ok(true)
}

/// The last upgrade this deck recorded, if it has ever started one.
#[must_use]
pub fn read_progress(home: &Path) -> Option<Progress> {
    let body = std::fs::read_to_string(progress_path(home)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Where every handover step is appended, beside `upgrade.json`. Written by
/// this module directly, so no supervisor redirection of stderr can orphan it.
#[must_use]
pub fn log_path(home: &Path) -> PathBuf {
    home.join("upgrade.log")
}

/// The log is rotated to `upgrade.log.1` (one generation) past this size.
pub const LOG_MAX_BYTES: u64 = 256 * 1024;

/// A non-terminal `replaced` / `restarting` stage older than this is stuck.
pub const STALL_AFTER_SECS: i64 = 120;

/// A handover lease with no beat for this long is not alive. Same idiom as
/// `ask::LEASE_TTL`; no pid is consulted.
pub const LEASE_TTL_SECS: i64 = 90;

/// How often the watchdog repeats itself for one stage.
pub const HEARTBEAT_SECS: i64 = 60;

/// How often the watchdog thread looks at `upgrade.json`.
pub const WATCHDOG_POLL: Duration = Duration::from_secs(30);

/// Append `line` to `path`, rotating to `<path>.1` first when the file has
/// reached `max` bytes. Open-append-close every time, so a rename or a
/// deleted file never leaves a stale handle.
pub fn append_bounded(path: &Path, line: &str, max: u64) -> std::io::Result<()> {
    use std::io::Write as _;
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= max) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        std::fs::rename(path, PathBuf::from(old))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

/// One handover step: to `tracing` at INFO and to `<home>/upgrade.log`, with
/// a UTC timestamp and this process's pid. Best-effort - a failed write is a
/// warning, never a failed upgrade.
pub fn log_step(home: &Path, msg: &str) {
    tracing::info!("handover: {msg}");
    log_line(home, "INFO", msg);
}

/// Like [`log_step`], at WARN.
pub fn log_warn(home: &Path, msg: &str) {
    tracing::warn!("handover: {msg}");
    log_line(home, "WARN", msg);
}

fn log_line(home: &Path, level: &str, msg: &str) {
    let line = format!(
        "{} pid={} {level} {msg}",
        Timestamp::now(),
        std::process::id()
    );
    if let Err(e) = append_bounded(&log_path(home), &line, LOG_MAX_BYTES) {
        tracing::warn!("could not append to {}: {e}", log_path(home).display());
    }
}

/// [`write_progress`] that says so when it fails, instead of dropping the
/// error: a progress file that silently stops moving is the symptom this
/// module exists to explain.
pub fn write_progress_logged(home: &Path, progress: &Progress) {
    if let Err(e) = write_progress(home, progress) {
        log_warn(home, &format!("could not write upgrade.json: {e:#}"));
    }
}

/// Proof that `hand_over` is running: written when it is entered, beaten
/// while it waits on the loop, removed when it leaves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoverLease {
    /// When `hand_over` was entered.
    pub entered_at: Timestamp,
    /// Last time it said it was alive.
    pub beat_at: Timestamp,
    /// The run it is waiting on, when one was in flight.
    #[serde(default)]
    pub parked_run: Option<String>,
}

impl HandoverLease {
    /// Whether the last beat is recent enough at `now`.
    #[must_use]
    pub fn fresh(&self, now: Timestamp) -> bool {
        now.as_second() - self.beat_at.as_second() <= LEASE_TTL_SECS
    }
}

/// Where the [`HandoverLease`] lives, beside `upgrade.json`.
#[must_use]
pub fn lease_path(home: &Path) -> PathBuf {
    home.join("upgrade.handover.json")
}

/// The lease on disk, if there is one.
#[must_use]
pub fn read_lease(home: &Path) -> Option<HandoverLease> {
    let body = std::fs::read_to_string(lease_path(home)).ok()?;
    serde_json::from_str(&body).ok()
}

fn write_lease(home: &Path, lease: &HandoverLease) {
    let path = lease_path(home);
    let tmp = path.with_extension("json.tmp");
    let written = serde_json::to_string(lease)
        .map_err(std::io::Error::other)
        .and_then(|body| std::fs::write(&tmp, body))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        log_warn(home, &format!("could not write the handover lease: {e}"));
    }
}

/// Held by `hand_over` for as long as it runs; removes the lease on drop.
#[derive(Debug)]
pub struct LeaseGuard {
    home: PathBuf,
    lease: HandoverLease,
}

impl LeaseGuard {
    /// Record that `hand_over` has been entered.
    #[must_use]
    pub fn enter(home: &Path, parked_run: Option<String>) -> Self {
        let now = Timestamp::now();
        let lease = HandoverLease {
            entered_at: now,
            beat_at: now,
            parked_run,
        };
        write_lease(home, &lease);
        Self {
            home: home.to_owned(),
            lease,
        }
    }

    /// Enter the handover: write the lease and the `parking` stage as one step
    /// under the progress lock, so a failing upgrade request ([`fail_progress`])
    /// or a fresh one ([`write_progress`]) cannot land between the two and leave
    /// a record that is newer than the lease. `None` for the stage means
    /// `upgrade.json` was unreadable (the lease is still written).
    #[must_use]
    pub fn enter_parking(home: &Path) -> (Self, bool) {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let progress = read_progress(home);
        let this = Self::enter(home, progress.as_ref().and_then(|p| p.parked_run.clone()));
        let recorded = match progress {
            Some(mut p) => {
                p.advance(Stage::Parking);
                if let Err(e) = write_progress_locked(home, &p) {
                    log_warn(home, &format!("could not write upgrade.json: {e:#}"));
                }
                true
            }
            None => false,
        };
        (this, recorded)
    }

    /// Say it is still alive.
    pub fn beat(&mut self) {
        self.lease.beat_at = Timestamp::now();
        write_lease(&self.home, &self.lease);
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(lease_path(&self.home));
    }
}

/// How a non-terminal stage came to be called stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StallKind {
    /// The handover was signalled but `hand_over` left no record of entering.
    NeverEntered,
    /// `hand_over` did enter (or the successor is starting) but nothing has
    /// moved or beaten for longer than allowed.
    StoppedBeating,
}

/// A non-terminal stage that has outlived what it should take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    /// The stage that is stuck.
    pub stage: Stage,
    /// Which kind of stuck, which decides what the operator is told.
    pub kind: StallKind,
    /// How long it has been without progress.
    pub age_secs: i64,
    /// What it is waiting on, in words.
    pub waiting_on: String,
}

/// Seconds `progress` has been in its stage at `now`. A clock that went
/// backwards counts as zero, never as a negative age.
#[must_use]
pub fn stage_age_secs(progress: &Progress, now: Timestamp) -> i64 {
    (now.as_second() - progress.updated_at.as_second()).max(0)
}

/// The lease, when it proves `hand_over` of *this* upgrade is alive at `now`.
#[must_use]
pub fn live_lease<'a>(
    progress: &Progress,
    lease: Option<&'a HandoverLease>,
    now: Timestamp,
) -> Option<&'a HandoverLease> {
    lease.filter(|l| {
        !progress.stage.terminal() && l.fresh(now) && l.entered_at >= progress.started_at
    })
}

/// What a stage is waiting on, in words.
fn waiting_on(progress: &Progress, lease: Option<&HandoverLease>) -> String {
    match progress.stage {
        Stage::Replaced => "hand_over to start (HANDOVER was signalled; hand_over has left no \
                            record that it was entered)"
            .to_owned(),
        Stage::Parking => {
            let run = lease
                .and_then(|l| l.parked_run.as_ref())
                .or(progress.parked_run.as_ref());
            match run {
                Some(run) => format!("the loop to finish run {run} at its next node boundary"),
                None => "the loop to stop (no run was recorded as in flight)".to_owned(),
            }
        }
        Stage::Restarting => "spawn_successor returning and this process exiting".to_owned(),
        Stage::Downloading => "the release download and binary replacement".to_owned(),
        Stage::Done | Stage::Failed => String::new(),
    }
}

/// Seconds `hand_over` has been alive and waiting, when `lease` proves it.
#[must_use]
pub fn waited_secs(lease: &HandoverLease, now: Timestamp) -> i64 {
    (now.as_second() - lease.entered_at.as_second()).max(0)
}

/// Pure: whether `progress` is stuck at `now`.
///
/// A fresh lease means `hand_over` is alive and waiting on the loop, which is
/// legitimate for as long as the run's node takes, so it is never stuck.
#[must_use]
pub fn stall(progress: &Progress, lease: Option<&HandoverLease>, now: Timestamp) -> Option<Stall> {
    if progress.stage.terminal() || progress.stage == Stage::Downloading {
        return None;
    }
    if live_lease(progress, lease, now).is_some() {
        return None;
    }
    // A stale lease for this upgrade means hand_over was alive and went quiet.
    let stale = lease.filter(|l| l.entered_at >= progress.started_at);
    let (kind, since) = match (progress.stage, stale) {
        (_, Some(l)) => (StallKind::StoppedBeating, l.beat_at),
        (Stage::Replaced, None) => (StallKind::NeverEntered, progress.updated_at),
        _ => (StallKind::StoppedBeating, progress.updated_at),
    };
    let age_secs = (now.as_second() - since.as_second()).max(0);
    let limit = if stale.is_some() {
        LEASE_TTL_SECS
    } else {
        STALL_AFTER_SECS
    };
    (age_secs > limit).then(|| Stall {
        stage: progress.stage,
        kind,
        age_secs,
        waiting_on: waiting_on(progress, lease),
    })
}

/// What the watchdog wants said this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beat {
    /// The stage this beat is about.
    pub stage: Stage,
    /// Whether to say it as a warning (stuck) rather than a heartbeat.
    pub warn: bool,
    /// The line to log and to persist as `detail`.
    pub message: String,
}

/// Decides when the watchdog speaks. Pure: the caller supplies `now`.
#[derive(Debug, Default)]
pub struct Watchdog {
    last: Option<(Stage, Timestamp)>,
}

impl Watchdog {
    /// Look at the record at `now`. `None` means stay quiet: terminal, too
    /// early, or already spoken within [`HEARTBEAT_SECS`] for this stage.
    pub fn tick(
        &mut self,
        progress: &Progress,
        lease: Option<&HandoverLease>,
        now: Timestamp,
    ) -> Option<Beat> {
        if progress.stage.terminal() {
            self.last = None;
            return None;
        }
        if self.last.is_some_and(|(stage, _)| stage != progress.stage) {
            self.last = None;
        }
        let stalled = stall(progress, lease, now);
        let alive = live_lease(progress, lease, now);
        // A live `hand_over` is allowed to wait long, but says what it waits
        // on; the other stages are silent until they are stalled.
        if stalled.is_none() && alive.is_none() && progress.stage != Stage::Parking {
            return None;
        }
        if let Some((_, at)) = self.last
            && now.as_second() - at.as_second() < HEARTBEAT_SECS
        {
            return None;
        }
        self.last = Some((progress.stage, now));
        let age = alive.map_or_else(|| stage_age_secs(progress, now), |l| waited_secs(l, now));
        let (warn, message) = match &stalled {
            Some(s) => (
                true,
                format!(
                    "stuck in {:?} for {} min {} s without progress, waiting on {}",
                    s.stage,
                    s.age_secs / 60,
                    s.age_secs % 60,
                    s.waiting_on
                ),
            ),
            None => (
                false,
                format!(
                    "parking for {} min {} s, waiting on {}",
                    age / 60,
                    age % 60,
                    waiting_on(progress, lease)
                ),
            ),
        };
        Some(Beat {
            stage: progress.stage,
            warn,
            message,
        })
    }
}

/// The watchdog's latest message, kept apart from `upgrade.json` so a
/// diagnostic write can never race a stage transition.
#[derive(Debug, Serialize, Deserialize)]
struct Note {
    stage: Stage,
    stage_since: Timestamp,
    message: String,
}

fn note_path(home: &Path) -> PathBuf {
    home.join("upgrade.note.json")
}

fn write_note(home: &Path, progress: &Progress, message: &str) {
    let note = Note {
        stage: progress.stage,
        stage_since: progress.updated_at,
        message: message.to_owned(),
    };
    let path = note_path(home);
    let tmp = path.with_extension("json.tmp");
    let written = serde_json::to_string(&note)
        .map_err(std::io::Error::other)
        .and_then(|body| std::fs::write(&tmp, body))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        tracing::warn!("could not write {}: {e}", path.display());
    }
}

/// The watchdog's message for exactly this stage of this upgrade, if any;
/// a note from another stage or another upgrade is ignored.
#[must_use]
pub fn read_note(home: &Path, progress: &Progress) -> Option<String> {
    let body = std::fs::read_to_string(note_path(home)).ok()?;
    let note: Note = serde_json::from_str(&body).ok()?;
    (note.stage == progress.stage && note.stage_since == progress.updated_at)
        .then_some(note.message)
}

/// Run the watchdog on a thread of its own, so a blocked runtime, or a
/// process half-way through dropping one, still speaks. It never ends; it is
/// a daemon thread and dies with the process.
pub fn spawn_watchdog(home: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("upgrade-watchdog".to_owned())
        .spawn(move || {
            let mut dog = Watchdog::default();
            loop {
                std::thread::sleep(WATCHDOG_POLL);
                let Some(progress) = read_progress(&home) else {
                    continue;
                };
                let lease = read_lease(&home);
                let Some(beat) = dog.tick(&progress, lease.as_ref(), Timestamp::now()) else {
                    continue;
                };
                if beat.warn {
                    log_warn(&home, &beat.message);
                } else {
                    log_step(&home, &beat.message);
                }
                // Never rewrites upgrade.json: a read-modify-write here could
                // overwrite a stage the handover or the successor saved in
                // between. The note goes to its own file, which only this
                // thread writes, and is matched to the record when read.
                write_note(&home, &progress, &beat.message);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("could not start the upgrade watchdog: {e}");
    }
}

/// Reconcile a leftover progress record on startup, before the server starts
/// answering requests.
///
/// A non-terminal record on disk when a process starts can only mean one of
/// two things: this *is* the successor `spawn_successor` started, or the
/// predecessor died before finishing the handover (a crash, a reboot, an
/// operator killing it by hand). Either way waiting longer will not resolve
/// it - this process is already up - so it is settled immediately:
/// [`Stage::Done`] when the running version matches what was asked for,
/// [`Stage::Failed`] otherwise, so the operator is told rather than left
/// watching a stage that will never move again.
pub fn reconcile_after_restart(home: &Path) {
    let Some(mut progress) = read_progress(home) else {
        return;
    };
    // Whoever held it is not this process.
    let _ = std::fs::remove_file(lease_path(home));
    if progress.stage.terminal() {
        return;
    }
    let running = env!("CARGO_PKG_VERSION");
    // `progress.to` is `latest.tag_name` from the forge, which - like every
    // tag in this repository - carries a `v` prefix `CARGO_PKG_VERSION` does
    // not. kaishin's own `is_update_available` strips it before comparing;
    // an exact-string match here would call a successful upgrade `Failed`
    // every time, because "v0.5.2" is never equal to "0.5.2".
    if progress
        .to
        .as_deref()
        .is_some_and(|to| to.trim_start_matches('v') == running)
    {
        progress.advance(Stage::Done);
    } else {
        let to = progress
            .to
            .clone()
            .unwrap_or_else(|| "the expected release".to_owned());
        progress.fail(format!(
            "this process came up on {running}, not {to} - the upgrade may \
             not have replaced the binary"
        ));
    }
    write_progress_logged(home, &progress);
}

/// Spawn the background check for `cfg`, unless it is switched off.
pub fn spawn(cfg: &Update, rt: &tokio::runtime::Handle) -> Option<Pending> {
    if disabled_by_env() || cfg.mode == UpdateMode::Off {
        return None;
    }
    let checker = Checker::new(cfg)?;
    match cfg.mode {
        UpdateMode::Off => None,
        UpdateMode::Notify => {
            if !checker.should_check() {
                let latest = checker.cached_update()?;
                return Some(Pending::Cached { checker, latest });
            }
            let inner = checker.inner.clone();
            let handle = rt.spawn(async move { inner.check_and_save().await });
            Some(Pending::Notify { checker, handle })
        }
        UpdateMode::Install => {
            let inner = checker.inner.clone();
            let handle = rt.spawn(async move { inner.auto_update().await });
            Some(Pending::Install { handle })
        }
    }
}

/// Drain a pending check and print at most one line.
///
/// Bounded on purpose: a slow network must never hold up the exit of a command
/// that already did its work.
pub async fn finalize(pending: Option<Pending>, budget: Duration) {
    let Some(pending) = pending else {
        return;
    };
    match pending {
        Pending::Cached { checker, latest } => {
            eprintln!("{}", checker.format_banner(&latest));
        }
        Pending::Notify { checker, handle } => {
            if let Ok(Ok(Ok(Some(latest)))) = tokio::time::timeout(budget, handle).await {
                eprintln!("{}", checker.format_banner(&latest));
            }
        }
        Pending::Install { handle } => {
            if let Ok(Ok(Ok(Some(latest)))) = tokio::time::timeout(budget, handle).await {
                eprintln!("magi updated itself to {}", latest.tag_name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An interval configured below GitHub's unauthenticated 60 req/hour/IP
    /// limit must be floored, or `web`'s background recheck loop - which,
    /// unlike a one-off CLI invocation, keeps polling for as long as `magi
    /// web` stays up - would repeat the same call far past that limit.
    #[test]
    fn effective_interval_floors_a_configured_interval_below_githubs_rate_limit() {
        let cfg = Update {
            mode: UpdateMode::Notify,
            interval: Some("1s".to_owned()),
        };
        assert_eq!(
            effective_interval(&cfg),
            MIN_INTERVAL,
            "an interval that would exceed GitHub's rate limit under continuous \
             polling must be floored rather than honoured verbatim"
        );

        let sane = Update {
            mode: UpdateMode::Notify,
            interval: Some("2h".to_owned()),
        };
        assert_eq!(
            effective_interval(&sane),
            Duration::from_secs(2 * 60 * 60),
            "an interval already above the floor must pass through unchanged"
        );
    }

    #[test]
    fn env_kill_switch_semantics() {
        // SAFETY: single-threaded test, no other thread reads the variable.
        unsafe {
            std::env::remove_var(NO_AUTOUPDATE_ENV);
        }
        assert!(!disabled_by_env());
        for (value, disabled) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            ("0", false),
            ("false", false),
            ("FALSE", false),
            ("", false),
            ("  ", false),
        ] {
            unsafe {
                std::env::set_var(NO_AUTOUPDATE_ENV, value);
            }
            assert_eq!(
                disabled_by_env(),
                disabled,
                "MAGI_NO_AUTOUPDATE={value:?} should {} disable",
                if disabled { "" } else { "not" }
            );
        }
        unsafe {
            std::env::remove_var(NO_AUTOUPDATE_ENV);
        }
    }

    #[test]
    fn off_mode_never_spawns() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cfg = Update {
            mode: UpdateMode::Off,
            interval: None,
        };
        assert!(spawn(&cfg, rt.handle()).is_none());
    }

    #[test]
    fn state_path_lives_under_the_cache_dir() {
        let path = state_path().expect("a cache dir on every supported platform");
        assert!(path.ends_with("magi/last_update_check.json"));
        let data = dirs::data_local_dir().unwrap_or_default();
        assert!(
            !path.starts_with(&data) || dirs::cache_dir() == dirs::data_local_dir(),
            "throttle state must not sit in the run history directory"
        );
    }

    #[tokio::test]
    async fn finalize_of_nothing_is_a_no_op() {
        finalize(None, Duration::from_millis(1)).await;
    }

    /// `mode = "off"` means no checker, for every caller.
    ///
    /// It used to mean it only for the notify path: `Checker::new` returned
    /// `Some` unconditionally, so `POST /api/upgrade` went to the GitHub
    /// releases API on a deck configured never to check. A test that reached
    /// that route made a live, unauthenticated request, and GitHub's
    /// 60-per-hour-per-address limit then turned the suite red on one runner
    /// at a time - for as long as anyone kept re-running it, since each
    /// attempt spent another request.
    #[test]
    fn checking_is_off_for_every_caller_when_the_config_says_off() {
        assert!(
            Checker::new(&Update {
                mode: UpdateMode::Off,
                interval: None,
            })
            .is_none(),
            "an operator who writes mode = \"off\" means it"
        );
        for mode in [UpdateMode::Notify, UpdateMode::Install] {
            assert!(
                Checker::new(&Update {
                    mode,
                    interval: None,
                })
                .is_some(),
                "{mode:?} still asks the forge"
            );
        }
    }

    /// `cached_update` never touches the network: a state file written the
    /// way `check_and_save` writes one is enough to answer, and no file at
    /// all answers "unknown" rather than blocking or erroring.
    ///
    /// Built from `kaishin::Checker` directly, with an explicit state path,
    /// rather than through [`Checker::new`]: that constructor always points
    /// at the real cache directory, which is right for production - every
    /// `magi` invocation on the machine shares one throttle file - but wrong
    /// for a test, which must never read or write the operator's actual
    /// state.
    #[test]
    fn cached_update_answers_from_disk_with_no_network_call() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state.json");
        let opts = kaishin::KaishinOptions::new("yukimemi", "magi", "magi", "0.1.0");
        let checker = Checker {
            inner: kaishin::Checker::new("magi", opts).state_path(path.clone()),
        };

        assert!(
            checker.cached_update().is_none(),
            "no state file yet must read as \"unknown\", not an error"
        );

        let state = kaishin::UpdateCheckState {
            last_checked_unix: 0,
            last_known_latest: Some("v9.9.9".to_owned()),
            last_known_url: Some("https://example.invalid/9.9.9".to_owned()),
        };
        kaishin::save_check_state(&path, &state).expect("seed the state file");

        let latest = checker.cached_update().expect("a newer release was cached");
        assert_eq!(latest.tag_name, "v9.9.9");
    }

    #[test]
    fn reconcile_after_restart_confirms_a_matching_version() {
        // `to` is `latest.tag_name` as the forge and this repository's own
        // tags spell it - with a `v` - which `CARGO_PKG_VERSION` never
        // carries. A test that leaves the `v` off would not have caught the
        // exact-string-equality bug this function used to have.
        let home = tempfile::tempdir().expect("temp home");
        let mut progress = Progress::new(
            "0.1.0".to_owned(),
            format!("v{}", env!("CARGO_PKG_VERSION")),
        );
        progress.advance(Stage::Restarting);
        write_progress(home.path(), &progress).expect("seed progress");

        reconcile_after_restart(home.path());

        let after = read_progress(home.path()).expect("progress on disk");
        assert_eq!(
            after.stage,
            Stage::Done,
            "the successor is running exactly the release that was asked for, \
             `v` prefix and all"
        );
    }

    #[test]
    fn reconcile_after_restart_flags_a_mismatched_version() {
        let home = tempfile::tempdir().expect("temp home");
        let mut progress = Progress::new("0.1.0".to_owned(), "v9.9.9".to_owned());
        progress.advance(Stage::Restarting);
        write_progress(home.path(), &progress).expect("seed progress");

        reconcile_after_restart(home.path());

        let after = read_progress(home.path()).expect("progress on disk");
        assert_eq!(after.stage, Stage::Failed);
        assert!(
            after.detail.is_some_and(|d| d.contains("9.9.9")),
            "the operator needs to know which release it did not come back on"
        );
    }

    #[test]
    fn reconcile_after_restart_leaves_a_settled_record_alone() {
        let home = tempfile::tempdir().expect("temp home");
        let mut progress = Progress::new("0.1.0".to_owned(), "9.9.9".to_owned());
        progress.advance(Stage::Done);
        write_progress(home.path(), &progress).expect("seed progress");

        reconcile_after_restart(home.path());

        let after = read_progress(home.path()).expect("progress on disk");
        assert_eq!(
            after.stage,
            Stage::Done,
            "an already-settled record must not be rewritten by a later, unrelated start"
        );
    }

    #[test]
    fn reconcile_after_restart_with_nothing_on_disk_is_a_quiet_no_op() {
        let home = tempfile::tempdir().expect("temp home");
        reconcile_after_restart(home.path());
        assert!(read_progress(home.path()).is_none());
    }

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).expect("timestamp")
    }

    fn staged(stage: Stage, since: i64) -> Progress {
        let mut p = Progress::new("0.1.0".to_owned(), "v0.2.0".to_owned());
        p.stage = stage;
        p.started_at = at(since);
        p.updated_at = at(since);
        p
    }

    fn lease(entered: i64, beat: i64) -> HandoverLease {
        HandoverLease {
            entered_at: at(entered),
            beat_at: at(beat),
            parked_run: Some("r1".to_owned()),
        }
    }

    #[test]
    fn a_handover_never_entered_is_stuck_and_says_only_what_is_known() {
        let p = staged(Stage::Replaced, 1000);
        assert!(stall(&p, None, at(1000 + STALL_AFTER_SECS)).is_none());
        let s = stall(&p, None, at(1000 + STALL_AFTER_SECS + 1)).expect("stalled");
        assert_eq!(s.stage, Stage::Replaced);
        assert_eq!(s.kind, StallKind::NeverEntered);
        assert_eq!(s.age_secs, STALL_AFTER_SECS + 1);
        assert!(s.waiting_on.contains("hand_over"), "{}", s.waiting_on);

        let p = staged(Stage::Restarting, 1000);
        let s = stall(&p, None, at(1000 + STALL_AFTER_SECS + 1)).expect("stalled");
        assert_eq!(s.kind, StallKind::StoppedBeating);

        for stage in [Stage::Done, Stage::Failed, Stage::Downloading] {
            assert!(stall(&staged(stage, 0), None, at(1_000_000)).is_none());
        }
    }

    #[test]
    fn a_live_parking_wait_is_never_stuck_however_long_it_lasts() {
        let mut p = staged(Stage::Parking, 1000);
        p.started_at = at(900);
        let hours = 5 * 3600;
        let l = lease(1000, 1000 + hours);
        assert!(stall(&p, Some(&l), at(1000 + hours + 10)).is_none());
        // Even a record that regressed to `replaced` is read through the lease.
        let r = staged(Stage::Replaced, 1000);
        assert!(stall(&r, Some(&l), at(1000 + hours + 10)).is_none());
        // Once the beat stops, it is stuck, and says so.
        let s = stall(&p, Some(&l), at(1000 + hours + LEASE_TTL_SECS + 1)).expect("stuck");
        assert_eq!(s.kind, StallKind::StoppedBeating);
    }

    #[test]
    fn a_lease_from_an_earlier_upgrade_proves_nothing() {
        let p = staged(Stage::Replaced, 2000);
        let old = lease(10, 3000);
        assert!(live_lease(&p, Some(&old), at(3001)).is_none());
    }

    #[test]
    fn a_stage_never_goes_backwards_but_a_new_upgrade_after_a_terminal_one_starts() {
        let parking = staged(Stage::Parking, 1000);
        for back in [Stage::Replaced, Stage::Downloading, Stage::Parking] {
            let mut cand = staged(back, 5000);
            cand.to = Some("v9.9.9".to_owned());
            let kept = monotonic(Some(&parking), &cand);
            assert_eq!(kept.stage, Stage::Parking);
            assert_eq!(kept.updated_at, at(1000));
            assert_eq!(kept.started_at, parking.started_at);
            assert_eq!(kept.to.as_deref(), Some("v9.9.9"), "data is refreshed");
        }
        assert_eq!(
            monotonic(Some(&parking), &staged(Stage::Restarting, 5000)).stage,
            Stage::Restarting
        );
        assert_eq!(
            monotonic(Some(&parking), &staged(Stage::Failed, 5000)).stage,
            Stage::Failed
        );
        let done = staged(Stage::Done, 1000);
        assert_eq!(
            monotonic(Some(&done), &staged(Stage::Downloading, 5000)).stage,
            Stage::Downloading
        );
    }

    #[test]
    fn write_progress_refuses_a_regression_on_disk() {
        let home = tempfile::tempdir().expect("temp home");
        write_progress(home.path(), &staged(Stage::Parking, 1000)).expect("write");
        write_progress(home.path(), &staged(Stage::Replaced, 5000)).expect("write");
        let on_disk = read_progress(home.path()).expect("record");
        assert_eq!(on_disk.stage, Stage::Parking);
        assert_eq!(on_disk.updated_at, at(1000));
    }

    #[test]
    fn a_failed_request_cannot_overwrite_a_live_handover() {
        let home = tempfile::tempdir().expect("temp home");
        write_progress(home.path(), &staged(Stage::Parking, 1000)).expect("write");
        assert!(!fail_progress(home.path(), "boom").expect("fail"));
        assert_eq!(read_progress(home.path()).unwrap().stage, Stage::Parking);
        write_progress(home.path(), &staged(Stage::Replaced, 1000)).ok();
        let fresh = tempfile::tempdir().expect("temp home");
        write_progress(fresh.path(), &staged(Stage::Downloading, 1000)).expect("write");
        assert!(fail_progress(fresh.path(), "boom").expect("fail"));
        assert_eq!(read_progress(fresh.path()).unwrap().stage, Stage::Failed);
    }

    #[test]
    fn entering_parking_is_one_step_that_keeps_the_lease_newer_than_the_record() {
        let home = tempfile::tempdir().expect("temp home");
        write_progress(home.path(), &staged(Stage::Replaced, 1000)).expect("write");
        let (guard, recorded) = LeaseGuard::enter_parking(home.path());
        assert!(recorded);
        let p = read_progress(home.path()).expect("record");
        assert_eq!(p.stage, Stage::Parking);
        let l = read_lease(home.path()).expect("lease");
        assert!(l.entered_at >= p.started_at);
        assert!(!fail_progress(home.path(), "boom").expect("fail"));
        drop(guard);
    }

    #[test]
    fn the_lease_guard_writes_beats_and_removes_the_lease() {
        let home = tempfile::tempdir().expect("temp home");
        {
            let mut guard = LeaseGuard::enter(home.path(), Some("r1".to_owned()));
            let first = read_lease(home.path()).expect("lease");
            assert_eq!(first.parked_run.as_deref(), Some("r1"));
            guard.beat();
            assert!(read_lease(home.path()).is_some());
        }
        assert!(read_lease(home.path()).is_none());
    }

    #[test]
    fn a_clock_that_went_backwards_is_age_zero() {
        let p = staged(Stage::Replaced, 5000);
        assert_eq!(stage_age_secs(&p, at(100)), 0);
        assert!(stall(&p, None, at(100)).is_none());
    }

    #[test]
    fn a_note_is_kept_beside_the_record_and_matches_only_its_own_stage() {
        let home = tempfile::tempdir().expect("temp home");
        let p = staged(Stage::Replaced, 1000);
        write_progress(home.path(), &p).expect("write");
        write_note(home.path(), &p, "stuck");
        assert_eq!(read_note(home.path(), &p).as_deref(), Some("stuck"));
        assert_eq!(read_progress(home.path()).unwrap().updated_at, at(1000));
        assert!(read_note(home.path(), &staged(Stage::Parking, 1000)).is_none());
        assert!(read_note(home.path(), &staged(Stage::Replaced, 2000)).is_none());
    }

    #[test]
    fn the_watchdog_speaks_once_a_minute_and_resets_on_a_new_stage() {
        let mut dog = Watchdog::default();
        let p = staged(Stage::Replaced, 1000);
        assert!(dog.tick(&p, None, at(1060)).is_none(), "not stalled yet");
        let beat = dog.tick(&p, None, at(1200)).expect("stalled");
        assert!(beat.warn);
        assert!(dog.tick(&p, None, at(1230)).is_none(), "spoke 30 s ago");
        assert!(dog.tick(&p, None, at(1260)).is_some(), "a minute later");

        let parking = staged(Stage::Parking, 1260);
        let l = lease(1260, 1270);
        let beat = dog
            .tick(&parking, Some(&l), at(1275))
            .expect("parking heartbeat at once");
        assert!(!beat.warn, "a live wait is not a warning");
        assert!(dog.tick(&parking, Some(&l), at(1300)).is_none());
        let l = lease(1260, 1260 + 4 * 3600);
        let later = dog
            .tick(&parking, Some(&l), at(1260 + 4 * 3600 + 5))
            .expect("heartbeat");
        assert!(!later.warn, "hours of waiting on a run is still not stuck");
        assert!(later.message.contains("r1"), "{}", later.message);

        let done = staged(Stage::Done, 0);
        assert!(dog.tick(&done, None, at(9_999_999)).is_none());
    }

    #[test]
    fn the_upgrade_log_appends_and_rotates_to_one_generation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("upgrade.log");
        append_bounded(&path, "one", 16).expect("append");
        append_bounded(&path, "two", 16).expect("append");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        append_bounded(&path, "three-and-more", 16).expect("append");
        append_bounded(&path, "four", 16).expect("append");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "four\n");
        let old = dir.path().join("upgrade.log.1");
        assert!(
            std::fs::read_to_string(old)
                .unwrap()
                .contains("three-and-more")
        );
    }
}
