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
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

static READERS: LazyLock<Mutex<BTreeMap<PathBuf, Weak<Lease>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static GATES: LazyLock<Mutex<BTreeMap<PathBuf, Weak<tokio::sync::RwLock<()>>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Repository scheduling is infrastructure work, never an agent failure.
#[derive(Debug)]
pub(crate) struct ResourceBlocked(pub(crate) String);

impl std::fmt::Display for ResourceBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ResourceBlocked {}

#[derive(Clone)]
pub(crate) struct WaitPolicy {
    limit: Duration,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

tokio::task_local! {
    static WAIT_POLICY: WaitPolicy;
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self::new(
            Duration::from_secs(crate::config::Graph::default().timeout_resource_wait),
            || false,
        )
    }
}

impl WaitPolicy {
    pub(crate) fn current() -> Self {
        WAIT_POLICY.try_with(Clone::clone).unwrap_or_default()
    }

    /// Spawned wave tasks must scope their own copy; task locals are not
    /// implicitly inherited by spawned tasks.
    pub(crate) async fn scope<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        WAIT_POLICY.scope(self.clone(), future).await
    }
    pub(crate) fn new(
        limit: Duration,
        cancelled: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            limit,
            cancelled: Arc::new(cancelled),
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        (self.cancelled)()
    }

    async fn wait<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        self.wait_inner(future, true).await
    }

    async fn local<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        self.wait_inner(future, false).await
    }

    async fn wait_inner<T>(
        &self,
        future: impl std::future::Future<Output = Result<T>>,
        bounded: bool,
    ) -> Result<T> {
        tokio::pin!(future);
        let deadline = async {
            if bounded {
                tokio::time::sleep(self.limit).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(deadline);
        let cancelled = async {
            while !self.cancelled() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::pin!(cancelled);
        tokio::select! {
            biased;
            result = &mut future => result,
            () = &mut cancelled => Err(ResourceBlocked(
                "repository coordination wait cancelled for parking".into(),
            ).into()),
            () = &mut deadline => Err(ResourceBlocked(
                format!("repository coordination remained busy for {}s", self.limit.as_secs())
            ).into()),
        }
    }
}

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

impl Drop for Lease {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // A concurrent fork can briefly inherit this open file description
            // before exec closes it. Closing our descriptor alone would leave
            // the lock held until that unrelated child closes its copy too.
            // The last lease owns the lock lifetime, not inherited descriptors.
            // SAFETY: the file remains open throughout this drop method.
            unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// Distinguish a non-checkout without interpreting Git's localized stderr.
/// Once a checkout marker is present, resolution errors remain fatal: a broken
/// Git file or inaccessible shared directory must never bypass coordination.
pub(crate) async fn common_dir(cwd: &Path) -> Result<Option<PathBuf>> {
    let cwd = match std::fs::canonicalize(cwd) {
        Ok(cwd) => cwd,
        // Preserve normal spawn failure for a missing command working directory.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("resolve seat working directory"),
    };
    let mut checkout = false;
    for ancestor in cwd.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => {
                checkout = true;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("inspect seat checkout marker"),
        }
    }
    if !checkout {
        return Ok(None);
    }
    let common = crate::git::git(
        &cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .context("resolve seat shared Git directory")?;
    Ok(Some(
        std::fs::canonicalize(common).context("canonicalize seat shared Git directory")?,
    ))
}

impl SeatLease {
    /// Launcher-owned VCS/setup work must exclude both seats and external
    /// updaters. Unlike an editing seat, it has no child agent to take the OS
    /// lock. Release this lease before invoking a seat (the gate is not reentrant).
    pub(crate) async fn operation_with(cwd: &Path, wait: &WaitPolicy) -> Result<Option<Self>> {
        let Some(mut permit) = Self::checkout(cwd, true, wait).await? else {
            return Ok(None);
        };
        permit._lock = Some(
            wait.wait(async {
                loop {
                    match Lease::acquire(permit.path.clone()) {
                        Ok(lock) => return Ok(lock),
                        Err(e) if contended(&e) => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
            .await?,
        );
        permit.delegated = true;
        Ok(Some(permit))
    }

    pub(crate) async fn for_checkout(cwd: &Path, allow_write: bool) -> Result<Option<Self>> {
        Self::checkout_with(cwd, allow_write, &WaitPolicy::current()).await
    }

    pub(crate) async fn checkout_with(
        cwd: &Path,
        allow_write: bool,
        wait: &WaitPolicy,
    ) -> Result<Option<Self>> {
        Self::checkout(cwd, allow_write, wait).await
    }

    async fn checkout(cwd: &Path, allow_write: bool, wait: &WaitPolicy) -> Result<Option<Self>> {
        let Some(common) = common_dir(cwd).await? else {
            return Ok(None);
        };
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
            let permit = wait.local(async { Ok(gate.write_owned().await) }).await?;
            return Ok(Some(Self {
                path,
                delegated: false,
                _read: None,
                _write: Some(permit),
                _lock: None,
            }));
        }
        let mut announced = false;
        let mut remaining = wait.limit;
        loop {
            let permit = wait
                .local(async { Ok(Arc::clone(&gate).read_owned().await) })
                .await?;
            let started = std::time::Instant::now();
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
                Err(e) if contended(&e) => {
                    // Infrastructure scheduling, not an agent attempt. Do not
                    // start a CLI, its timeout, a fallback or a handover. Drop
                    // the local read permit so a queued editor can proceed.
                    drop(permit);
                    if !announced {
                        tracing::info!(path = %path.display(), "waiting for repository coordination before starting seat");
                        announced = true;
                    }
                    let external_wait = WaitPolicy {
                        limit: remaining,
                        ..wait.clone()
                    };
                    external_wait
                        .wait(async {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            Ok(())
                        })
                        .await?;
                    remaining = remaining.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(ResourceBlocked(
                            "repository coordination remained busy".into(),
                        )
                        .into());
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn contended(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
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

    #[tokio::test]
    #[cfg(unix)]
    async fn queued_editors_outlive_the_external_wait_limit() {
        let dir = tempfile::tempdir().unwrap();
        crate::git::git(dir.path(), &["init"]).await.unwrap();
        let common = common_dir(dir.path()).await.unwrap().unwrap();
        std::fs::write(common.join("kanade-sync.contract"), "shared-lock-v1\n").unwrap();
        let short = WaitPolicy::new(Duration::from_millis(20), || false);
        let first = SeatLease::checkout_with(dir.path(), true, &short)
            .await
            .unwrap();
        let (second, ()) =
            tokio::join!(SeatLease::checkout_with(dir.path(), true, &short), async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                drop(first);
            });
        drop(second.unwrap());
        // A parked node can save a completed seat when coordination is free.
        let parked = WaitPolicy::new(Duration::from_millis(20), || true);
        drop(
            SeatLease::operation_with(dir.path(), &parked)
                .await
                .unwrap(),
        );
        // No contract means no resource wait, even after a park request.
        std::fs::remove_file(common.join("kanade-sync.contract")).unwrap();
        assert!(
            SeatLease::operation_with(dir.path(), &parked)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn external_waits_are_bounded_and_local_contention_is_cancellable() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        crate::git::git(dir.path(), &["init"]).await.unwrap();
        let common = common_dir(dir.path()).await.unwrap().unwrap();
        std::fs::write(common.join("kanade-sync.contract"), "shared-lock-v1\n").unwrap();
        let path = common.join("kanade-sync.lock");
        let short = WaitPolicy::new(Duration::from_millis(100), || false);
        let external = exclusive(&path).unwrap();
        assert!(
            SeatLease::checkout_with(dir.path(), false, &short)
                .await
                .err()
                .expect("busy")
                .is::<ResourceBlocked>()
        );
        assert!(
            SeatLease::operation_with(dir.path(), &short)
                .await
                .err()
                .expect("busy")
                .is::<ResourceBlocked>()
        );
        drop(external);
        let editor = SeatLease::checkout_with(dir.path(), true, &short)
            .await
            .unwrap();
        for operation in [false, true] {
            let cancelled = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&cancelled);
            let wait =
                WaitPolicy::new(Duration::from_secs(60), move || flag.load(Ordering::SeqCst));
            let (result, ()) = tokio::join!(
                async {
                    if operation {
                        SeatLease::operation_with(dir.path(), &wait).await
                    } else {
                        SeatLease::checkout_with(dir.path(), false, &wait).await
                    }
                },
                async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    cancelled.store(true, Ordering::SeqCst);
                }
            );
            let error = result
                .err()
                .expect("cancelled before obtaining the local gate");
            assert!(error.is::<ResourceBlocked>());
            assert!(error.to_string().contains("parking"));
        }
        drop(editor);
        // Timed-out and cancelled waiters must not retain a reservation.
        drop(SeatLease::operation_with(dir.path(), &short).await.unwrap());
        assert!(exclusive(&path).is_ok());
    }

    #[tokio::test]
    async fn non_checkout_needs_no_git_resolution_but_broken_checkout_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(common_dir(dir.path()).await.unwrap().is_none());
        assert!(
            common_dir(&dir.path().join("missing"))
                .await
                .unwrap()
                .is_none()
        );
        std::fs::write(dir.path().join(".git"), "gitdir: missing-git-directory\n").unwrap();
        assert!(common_dir(dir.path()).await.is_err());
        assert!(SeatLease::for_checkout(dir.path(), false).await.is_err());
    }

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
    #[cfg(unix)]
    fn last_lease_unlocks_even_with_an_inherited_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanade-sync.lock");
        let lease = Lease::acquire(path.clone()).unwrap();
        // dup shares the open file description, just like a descriptor
        // inherited across fork before CLOEXEC takes effect.
        let inherited = lease._file.try_clone().unwrap();
        assert!(exclusive(&path).is_err());
        drop(lease);
        let outsider = exclusive(&path).expect("last lease must release the real lock");
        drop(inherited);
        assert!(
            exclusive(&path).is_err(),
            "old copies cannot unlock a new owner"
        );
        drop(outsider);
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
