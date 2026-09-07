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

/// The interval `cfg` configures, or [`default_interval`] when unset or
/// unparsable.
///
/// Shared by [`Checker::new`], which throttles the network call itself
/// against it, and `web::recheck_poll_period`, which uses it to decide how
/// often to even ask - a background recheck task cannot track an interval it
/// never sees.
pub fn effective_interval(cfg: &Update) -> Duration {
    cfg.interval
        .as_deref()
        .and_then(|s| kaishin::parse_interval(s).ok())
        .unwrap_or_else(default_interval)
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

/// Persist `progress`, atomically.
///
/// Written to a sibling `.tmp` and renamed, the same reason
/// `daemon::write_status_to` does it: `/api/health` reads this file on every
/// poll and must never see a half-written one.
pub fn write_progress(home: &Path, progress: &Progress) -> Result<()> {
    let path = progress_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(progress).context("serialize upgrade progress")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// The last upgrade this deck recorded, if it has ever started one.
#[must_use]
pub fn read_progress(home: &Path) -> Option<Progress> {
    let body = std::fs::read_to_string(progress_path(home)).ok()?;
    serde_json::from_str(&body).ok()
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
    let _ = write_progress(home, &progress);
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
}
