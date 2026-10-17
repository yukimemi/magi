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

pub(crate) struct Lease {
    _file: File,
    pub(crate) path: PathBuf,
}

impl Lease {
    pub(crate) async fn for_checkout(cwd: &Path) -> Result<Option<Arc<Self>>> {
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
        Self::acquire(common.join("kanade-sync.lock")).map(Some)
    }

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
        let lease = Arc::new(Self {
            _file: file,
            path: path.clone(),
        });
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
