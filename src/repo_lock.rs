//! Launcher-held coordination for read-only seats in registered repositories.
//!
//! A read-only sandbox must not gain Git write access merely to take a lock.
//! The launcher owns the real OS lock for the entire invocation instead. Its
//! concurrent read-only seats borrow the same handle; another process (including
//! the repository updater) still contends with that exclusive handle.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use anyhow::{Context as _, Result, bail};

static READERS: LazyLock<Mutex<BTreeMap<PathBuf, Weak<Lease>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static GATES: LazyLock<Mutex<BTreeMap<PathBuf, Weak<tokio::sync::RwLock<()>>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// A local reservation prevents a launcher-held reader from blocking this
/// launcher's editing agents when they take their own .NET lock. The fair
/// RwLock queues editors ahead of subsequent readers, before any CLI starts.
pub(crate) struct SeatLease {
    pub(crate) path: PathBuf,
    pub(crate) delegated: bool,
    // Field drop order matters: close the OS lock before waking an editor.
    _lock: Option<Arc<Lease>>,
    _read: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    _write: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
}

struct Lease {
    _file: File,
}

impl SeatLease {
    pub(crate) async fn for_checkout(cwd: &Path, allow_write: bool) -> Result<Option<Self>> {
        let out = crate::git::git_raw(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        if !out.ok() {
            // Some non-repository conversations and command adapters have no
            // checkout. Do not hide failures resolving a real repository.
            if out.stderr.contains("not a git repository") {
                return Ok(None);
            }
            bail!(
                "resolve read-only seat shared Git directory: {}",
                out.stderr
            );
        }
        let common = std::fs::canonicalize(out.stdout)?;
        match std::fs::read_to_string(common.join("kanade-sync.contract")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("read repository coordination contract"),
            Ok(contract) if contract.trim() == "shared-lock-v1" => {}
            Ok(_) => bail!("unsupported repository coordination contract"),
        }
        // Group termination is required to safely release coordination after
        // cancellation. Until Windows job containment is implemented here,
        // registered seats fail closed before spawning on that platform.
        if !cfg!(unix) {
            bail!("supervised repository coordination currently requires Unix process groups");
        }
        let path = common.join("kanade-sync.lock");
        let gate = {
            let mut gates = GATES
                .lock()
                .map_err(|_| anyhow::anyhow!("repository gate registry poisoned"))?;
            let gate = gates
                .get(&path)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| Arc::new(tokio::sync::RwLock::new(())));
            gates.retain(|_, gate| gate.strong_count() != 0);
            gates.insert(path.clone(), Arc::downgrade(&gate));
            gate
        };
        if allow_write {
            let permit = gate.write_owned().await;
            return Ok(Some(Self {
                path,
                delegated: false,
                _read: None,
                _write: Some(permit),
                _lock: None,
            }));
        }
        let mut announced = false;
        loop {
            let permit = Arc::clone(&gate).read_owned().await;
            match Lease::acquire(path.clone()) {
                Ok(lease) => {
                    return Ok(Some(Self {
                        path,
                        delegated: true,
                        _read: Some(permit),
                        _write: None,
                        _lock: Some(lease),
                    }));
                }
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
                {
                    // Infrastructure scheduling, not an agent attempt. Do not
                    // start a CLI, its timeout, a fallback or a handover. Drop
                    // the local read permit so a queued editor can proceed.
                    drop(permit);
                    if !announced {
                        tracing::info!(path = %path.display(), "waiting for repository coordination before starting seat");
                        announced = true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Lease {
    fn acquire(path: PathBuf) -> Result<Arc<Self>> {
        let mut readers = READERS
            .lock()
            .map_err(|_| anyhow::anyhow!("repository lock registry poisoned"))?;
        if let Some(lease) = readers.get(&path).and_then(Weak::upgrade) {
            return Ok(lease);
        }
        let file = exclusive(&path).with_context(|| {
            format!(
                "acquire mandatory repository lock {} before starting seat",
                path.display()
            )
        })?;
        let lease = Arc::new(Self { _file: file });
        readers.retain(|_, lease| lease.strong_count() != 0);
        readers.insert(path, Arc::downgrade(&lease));
        Ok(lease)
    }
}

// FileShare.None uses flock on Unix and a zero sharing mode on Windows. Keep
// this interoperable with the repository's .NET protocol, not a separate lock.
fn exclusive(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.share_mode(0);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor belongs to `file` and remains open here.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "repository locking is unsupported on this platform",
    ));
    #[cfg(any(unix, windows))]
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_share_real_lock_until_the_last_seat_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanade-sync.lock");
        let first = Lease::acquire(path.clone()).unwrap();
        let second = Lease::acquire(path.clone()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(exclusive(&path).is_err());
        drop(first);
        assert!(exclusive(&path).is_err());
        drop(second);
        drop(exclusive(&path).unwrap());
        assert!(path.exists(), "never unlink the shared lock");
    }

    #[test]
    fn external_contention_refuses_a_seat_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanade-sync.lock");
        let external = exclusive(&path).unwrap();
        assert!(Lease::acquire(path.clone()).is_err());
        drop(external);
        assert!(Lease::acquire(path).is_ok());
    }
}
