//! Self-update, via `kaishin`.
//!
//! A magi run takes minutes of agent latency, so a background release check
//! costs nothing measurable: it is spawned on the same tokio runtime as the
//! command, overlaps it, and is drained with a bounded wait at shutdown. It
//! never delays the graph.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::config::{Update, UpdateMode};

/// Env kill-switch. Any non-empty value other than `0` / `false` disables the
/// background check, and it is read before the config so a broken `magi.toml`
/// cannot force a network call.
pub const NO_AUTOUPDATE_ENV: &str = "MAGI_NO_AUTOUPDATE";

/// Default interval between checks.
pub fn default_interval() -> Duration {
    kaishin::default_interval()
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
    /// Build a checker honouring `cfg`.
    pub fn new(cfg: &Update) -> Option<Self> {
        let mut inner = kaishin::Checker::new(BIN, options());
        if let Some(path) = state_path() {
            inner = inner.state_path(path);
        }
        let interval = cfg
            .interval
            .as_deref()
            .and_then(|s| kaishin::parse_interval(s).ok())
            .unwrap_or_else(default_interval);
        Some(Self {
            inner: inner.interval(interval),
        })
    }

    /// Is a check due?
    pub fn should_check(&self) -> bool {
        self.inner.should_check()
    }

    /// A newer release already known from a previous run.
    pub fn cached_update(&self) -> Option<kaishin::LatestRelease> {
        self.inner.cached_update()
    }

    /// One-line "a newer version exists" banner.
    pub fn format_banner(&self, latest: &kaishin::LatestRelease) -> String {
        self.inner.format_banner(latest)
    }
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
}
