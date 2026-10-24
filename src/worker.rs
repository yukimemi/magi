//! Durable detached execution, independent of the scheduler and web lifetimes.
//!
//! A reservation is published before spawn, and the child must take its execution
//! lease and acknowledge that reservation before dispatch. Reconciliation cancels
//! an unacknowledged startup under the same store lock the child needs: a late
//! child can never execute a cancelled operation. Process IDs are diagnostic only.
//! Metadata publication closes its staging handle before atomic replacement.
//! Windows replacement/delete-pending handoffs use the short, bounded lock-file
//! retry policy; failed publication never releases an uncertain reservation.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const STARTUP_SECONDS: u64 = 30;
const RETAIN_FINISHED: usize = 256;

/// Durable execution state. Starting reservations count as occupied slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Published but not yet acknowledged by the detached child.
    Starting,
    /// The child acknowledged startup and owns the execution lease.
    Running,
    /// The handler returned successfully.
    Finished,
    /// Startup, handler execution, or the owning process failed.
    Failed,
}

/// An explicit owner control; upgrades do not send these to task workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// Park at the handler's existing safe cancellation boundary.
    Park,
    /// Stop the operation through its handler's cancellation path.
    Stop,
}

#[derive(Debug)]
struct NotStarted(anyhow::Error);

impl std::fmt::Display for NotStarted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.0)
    }
}

impl std::error::Error for NotStarted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn not_started_error(error: impl Into<anyhow::Error>) -> anyhow::Error {
    anyhow::Error::new(NotStarted(error.into()))
}

/// Whether this spawn invocation provably did not execute its handler. This
/// covers preparation failures and a failed OS spawn whose fresh reservation
/// was exclusively leased and durably fenced before this error was returned.
/// Only a fresh operation key makes this proof sufficient to restore a caller's
/// publication. Unknown startup, existing work, or a failed fence is never tagged.
pub fn not_started(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NotStarted>().is_some()
}

/// A durable operation record. Its payload belongs to the started generation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    /// Unique invocation identity.
    pub id: String,
    /// Durable total reservation order, independent of wall clock resolution.
    #[serde(default)]
    pub sequence: u64,
    /// Dispatch handler, such as task, chat, scheduler, or service.
    pub kind: String,
    /// Exclusive logical operation identity, independent of executable generation.
    pub key: String,
    /// Handler-owned request.
    pub payload: serde_json::Value,
    /// SHA-256 identity of the immutable executable.
    pub generation: String,
    /// Version reported by the started executable, unknown before acknowledgement.
    #[serde(default)]
    pub version: Option<String>,
    /// Last durable execution state.
    pub status: Status,
    /// Successful completion note or failure diagnostic.
    pub result: Option<String>,
    /// Diagnostic process ID, never used as an execution witness.
    pub pid: Option<u32>,
    /// Reservation publication time, seconds since the Unix epoch.
    pub created_at: u64,
    /// Terminal state publication time.
    pub finished_at: Option<u64>,
}

/// An immutable, content-addressed executable.
#[derive(Clone, Debug)]
pub struct Generation {
    /// SHA-256 digest of the executable bytes.
    pub id: String,
    /// Absolute path to the retained executable.
    pub executable: PathBuf,
}

/// OS-held lock. Closing it, including on a crash, releases ownership.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}

impl Drop for Lease {
    fn drop(&mut self) {
        // flock's lock belongs to the open-file description; explicitly unlock
        // so a temporarily inherited descriptor cannot prolong ownership.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn root(home: &Path) -> PathBuf {
    home.join("workers")
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn nonce() -> String {
    crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4()
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}
fn job_dir(home: &Path, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        bail!("invalid worker id");
    }
    Ok(root(home).join("jobs").join(id))
}

fn open_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worker lock directory {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open worker lock {}", path.display()))
}

fn try_lock(path: &Path) -> Result<Option<Lease>> {
    let file = open_lock(path)?;
    match crate::pr_head::try_lock_file(&file) {
        Ok(()) => Ok(Some(Lease { _file: file })),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || matches!(e.raw_os_error(), Some(33)) =>
        {
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| format!("acquire worker lock {}", path.display())),
    }
}

#[cfg(test)]
pub(crate) fn execution_lease_for_test(path: &Path) -> Lease {
    try_lock(path)
        .expect("execution lease")
        .expect("free execution lease")
}

/// Try to own a named cross-process singleton, without PID-based stale handling.
/// Names are hashed, so callers may use logical keys containing paths or colons.
pub fn try_lease(home: &Path, name: &str) -> Result<Option<Lease>> {
    try_lock(
        &root(home)
            .join("leases")
            .join(format!("{}.lock", digest(name.as_bytes()))),
    )
}

fn store_lock(home: &Path) -> Result<Lease> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(lease) = try_lock(&root(home).join("store.lock"))? {
            return Ok(lease);
        }
        if std::time::Instant::now() >= deadline {
            bail!("worker store is busy; retry later");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open worker directory for sync {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync worker directory {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("worker record has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create worker record directory {}", parent.display()))?;
    let temp = parent.join(format!(".pending-{}", nonce()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .with_context(|| format!("create staged worker record {}", temp.display()))?;
        serde_json::to_writer(&mut file, value)
            .with_context(|| format!("serialize worker record {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flush staged worker record {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync staged worker record {}", temp.display()))?;
        // Publication must not retain our own writable staging handle. Windows
        // readers may briefly deny deletion; only the atomic handoff retries.
        drop(file);
        replace(&temp, path).with_context(|| {
            format!(
                "publish worker record {} -> {}",
                temp.display(),
                path.display()
            )
        })?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> Result<()> {
    Ok(std::fs::rename(from, to)?)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> Result<()> {
    // std uses FileRenameInfoEx/POSIX semantics where supported, unlike
    // MoveFileExW: a FILE_SHARE_DELETE reader may retain the previous record
    // while the replacement becomes visible atomically. Readers that briefly
    // deny deletion still get only the existing short Windows handoff budget.
    crate::lock_file::Retry::new(None).run(|| std::fs::rename(from, to))?;
    Ok(())
}

// Atomic replacement can leave the old Windows name briefly delete-pending.
// Keep that handoff bounded; malformed records and permanent IO errors propagate.
fn open_record(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        crate::lock_file::Retry::new(None).run(|| File::open(path))
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

/// Load a specific invocation, including a retained terminal result.
pub fn get(home: &Path, id: &str) -> Result<Job> {
    let path = job_dir(home, id)?.join("job.json");
    let file = match open_record(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let retained = root(home).join("results").join(id).join("job.json");
            open_record(&retained)
                .with_context(|| format!("open retained worker record {}", retained.display()))?
        }
        Err(error) => {
            return Err(error).with_context(|| format!("open worker record {}", path.display()));
        }
    };
    serde_json::from_reader(file).with_context(|| format!("decode worker record {id}"))
}

/// Identify this detached invocation. Helpers inherit its identity, while a
/// newly spawned worker receives its own id before exec; no global environment
/// mutation is required inside the asynchronous handler runtime.
pub fn current_job(home: &Path) -> Result<Option<Job>> {
    match std::env::var("MAGI_WORKER_ID") {
        Ok(id) => Ok(Some(get(home, &id)?)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn next_sequence(home: &Path) -> Result<u64> {
    let path = root(home).join("sequence.json");
    let previous: u64 = match open_record(&path) {
        Ok(file) => serde_json::from_reader(file)
            .with_context(|| format!("decode worker sequence {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error).with_context(|| format!("open worker sequence {}", path.display()));
        }
    };
    let sequence = previous
        .checked_add(1)
        .context("worker reservation sequence exhausted")?;
    save(&path, &sequence)?;
    Ok(sequence)
}

fn records(home: &Path) -> Result<Vec<Job>> {
    let directory = root(home).join("jobs");
    std::fs::create_dir_all(&directory)?;
    let mut jobs = Vec::new();
    let mut retired = false;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str() else {
            continue;
        };
        if id.starts_with(".new-") {
            // Reservations are staged only while holding the store lock. Any
            // staging directory visible to the next owner was left by a crash.
            std::fs::remove_dir_all(entry.path())?;
            continue;
        }
        if id.starts_with('.') {
            continue;
        }
        for pending in std::fs::read_dir(entry.path())? {
            let pending = pending?;
            if pending.file_type()?.is_file()
                && pending
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pending-")
            {
                std::fs::remove_file(pending.path())?;
            }
        }
        let job = get(home, id).with_context(|| format!("read worker {id}"))?;
        if matches!(job.status, Status::Starting | Status::Running) {
            jobs.push(job);
        } else {
            retired |= retire(home, &job);
        }
    }
    if retired {
        if let Err(error) = prune_results(home) {
            tracing::warn!(%error, "worker result retention deferred");
        }
    }
    Ok(jobs)
}

fn terminal(home: &Path, job: &mut Job, status: Status, result: String) -> Result<()> {
    job.status = status;
    job.result = Some(result);
    job.finished_at = Some(now());
    save(&job_dir(home, &job.id)?.join("job.json"), job)
        .with_context(|| format!("publish worker {} terminal status {status:?}", job.id))?;
    // The result is authoritative now. Keep the directory in place while this
    // process owns live.lock and its stdout/stderr handles. Another store owner
    // retires it opportunistically after execution ownership is released.
    clear_control_locked(home, &job.key)
}

fn retire(home: &Path, job: &Job) -> bool {
    let attempt = || -> Result<bool> {
        let dir = job_dir(home, &job.id)?;
        let Some(lease) = try_lock(&dir.join("live.lock"))? else {
            return Ok(false);
        };
        drop(lease);
        archive(home, job)?;
        Ok(true)
    };
    // Windows can still deny the directory rename in the short interval after
    // lease release but before process stdout/stderr are closed. Retry on the
    // next store read; archival failure must never turn completion into failure
    // or terminate an otherwise healthy scheduler.
    match attempt() {
        Ok(retired) => retired,
        Err(error) => {
            tracing::warn!(
                worker = %job.id,
                error = %format_args!("{error:#}"),
                "worker result retirement deferred"
            );
            false
        }
    }
}

fn archive(home: &Path, job: &Job) -> Result<()> {
    let results = root(home).join("results");
    std::fs::create_dir_all(&results)
        .with_context(|| format!("create worker results directory {}", results.display()))?;
    let from = job_dir(home, &job.id)?;
    let to = results.join(&job.id);
    std::fs::rename(&from, &to).with_context(|| {
        format!(
            "archive worker result {} -> {}",
            from.display(),
            to.display()
        )
    })?;
    sync_dir(&root(home).join("jobs"))?;
    sync_dir(&results)
}

// Caller owns the store lock. Acquiring the execution lease and then marking a
// dead startup terminal fences a late child: it must recheck state under store.
fn reconcile(home: &Path, jobs: &mut [Job]) -> Result<()> {
    let mut retired = false;
    for job in jobs {
        if !matches!(job.status, Status::Starting | Status::Running) {
            continue;
        }
        let lease = try_lock(&job_dir(home, &job.id)?.join("live.lock"))?;
        if lease.is_none() {
            continue;
        }
        if job.status == Status::Starting && now().saturating_sub(job.created_at) < STARTUP_SECONDS
        {
            continue;
        }
        let reason = if job.status == Status::Starting {
            "worker startup reservation expired before acknowledgement"
        } else {
            "worker exited without recording a result"
        };
        terminal(home, job, Status::Failed, reason.to_owned())?;
        drop(lease);
        retired |= retire(home, job);
    }
    if retired {
        if let Err(error) = prune_results(home) {
            tracing::warn!(%error, "worker result retention deferred");
        }
    }
    Ok(())
}

/// List live operations and unresolved startups. A missing lease only frees a
/// startup after its deadline and an atomic cancellation fence; PID is irrelevant.
pub fn active(home: &Path) -> Result<Vec<Job>> {
    let _store = store_lock(home)?;
    let mut jobs = records(home)?;
    reconcile(home, &mut jobs)?;
    Ok(jobs
        .into_iter()
        .filter(|job| matches!(job.status, Status::Starting | Status::Running))
        .collect())
}

/// Observe execution without creating, repairing, archiving, or locking the
/// writable store. Sandboxed diagnostics need only file reads and a read-only
/// handle for the OS lease probe. Starting reservations remain protected until
/// an authorized writer fences them; a missing running lease is unknown.
pub fn observe_active(home: &Path) -> Result<Vec<Job>> {
    let entries = match std::fs::read_dir(root(home).join("jobs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut active = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let id = name.to_str().context("invalid worker reservation name")?;
        if id.starts_with('.') {
            continue;
        }
        let job = get(home, id)?;
        match job.status {
            Status::Starting => active.push(job),
            Status::Running => {
                let file = match File::open(entry.path().join("live.lock")) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        active.push(job);
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                match crate::pr_head::try_lock_file(&file) {
                    Ok(()) => drop(Lease { _file: file }),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || cfg!(windows) && error.raw_os_error() == Some(33) =>
                    {
                        active.push(job);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Status::Finished | Status::Failed => {}
        }
    }
    Ok(active)
}

fn control_path(home: &Path, key: &str) -> PathBuf {
    root(home)
        .join("controls")
        .join(format!("{}.json", digest(key.as_bytes())))
}

/// Persist a stop or park request; the operation's handler consumes it safely.
pub fn request(home: &Path, key: &str, control: Control) -> Result<()> {
    let _store = store_lock(home)?;
    save(&control_path(home, key), &control)
}

/// Read the latest durable owner request for a logical operation.
pub fn control(home: &Path, key: &str) -> Result<Option<Control>> {
    let path = control_path(home, key);
    match open_record(&path) {
        Ok(file) => serde_json::from_reader(file)
            .map(Some)
            .with_context(|| format!("decode worker control {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("open worker control {}", path.display())),
    }
}

fn clear_control_locked(home: &Path, key: &str) -> Result<()> {
    let path = control_path(home, key);
    match std::fs::remove_file(&path) {
        Ok(()) => sync_dir(&root(home).join("controls")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove worker control {}", path.display())),
    }
}

/// Clear controls at an explicitly owned new singleton invocation boundary.
pub fn clear_control(home: &Path, key: &str) -> Result<()> {
    let _store = store_lock(home)?;
    clear_control_locked(home, key)
}

/// Resolve a retained executable strictly by its content identity.
pub fn executable(home: &Path, generation: &str) -> Result<PathBuf> {
    if generation.len() != 64 || !generation.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid worker generation");
    }
    let path = root(home)
        .join("generations")
        .join(generation)
        .join(if cfg!(windows) { "magi.exe" } else { "magi" });
    if !path.is_file() {
        bail!("worker generation {generation} is unavailable");
    }
    Ok(path)
}

/// Resolve helper invocations to this worker's started executable, not a newly
/// installed binary. Outside a worker, pin the caller's current executable.
pub fn helper_executable(home: &Path) -> Result<PathBuf> {
    if let Some(generation) = std::env::var_os("MAGI_WORKER_GENERATION") {
        return executable(
            home,
            generation
                .to_str()
                .context("invalid generation environment")?,
        );
    }
    Ok(pin_current(home)?.executable)
}

/// Remember the installed executable pathname before an updater replaces it.
/// A web process calls this at startup so Linux's deleted-executable marker or
/// Windows's backup rename cannot redirect successor workers to the old binary.
pub fn remember_current_executable() -> Result<()> {
    if INSTALLED_EXECUTABLE.get().is_some() {
        return Ok(());
    }
    let path = std::env::current_exe()?.canonicalize()?;
    let _ = INSTALLED_EXECUTABLE.set(path);
    Ok(())
}

static INSTALLED_EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

/// Copy and SHA-256 address the executable. MAGI_WORKER_EXE is an explicit
/// embedding/test override; it is never an in-process execution fallback.
pub fn pin_current(home: &Path) -> Result<Generation> {
    let source = match std::env::var_os("MAGI_WORKER_EXE") {
        Some(path) => PathBuf::from(path),
        None => match INSTALLED_EXECUTABLE.get() {
            Some(path) => path.clone(),
            None => std::env::current_exe()?,
        },
    };
    let source = source.canonicalize().context("resolve worker executable")?;
    // A worker already runs the immutable copy. Do not copy/hash a large binary
    // again for every finite maintenance invocation in the same generation.
    if let Some(directory) = source.parent() {
        if directory.parent() == Some(root(home).join("generations").as_path()) {
            if let Some(id) = directory.file_name().and_then(|name| name.to_str()) {
                if id.len() == 64
                    && id.bytes().all(|b| b.is_ascii_hexdigit())
                    && executable(home, id)?.canonicalize()? == source
                {
                    return Ok(Generation {
                        id: id.to_owned(),
                        executable: source,
                    });
                }
            }
        }
    }
    let stamp = source_stamp(&source)?;
    let mut cache = PIN_CACHE
        .lock()
        .map_err(|_| anyhow::anyhow!("worker generation cache poisoned"))?;
    if let Some(entry) = cache.iter().find(|entry| {
        entry.home == home
            && entry.source == source
            && entry.stamp == stamp
            && entry.generation.executable.is_file()
    }) {
        return Ok(entry.generation.clone());
    }
    let generation = pin_source(home, &source)?;
    if source_stamp(&source)? != stamp {
        bail!("worker executable changed while being pinned; retry");
    }
    if cache.len() >= 8 {
        cache.remove(0);
    }
    cache.push(CachedGeneration {
        home: home.to_path_buf(),
        source,
        stamp,
        generation: generation.clone(),
    });
    Ok(generation)
}

type SourceStamp = (u64, Option<SystemTime>, Option<SystemTime>, u64, u64);
struct CachedGeneration {
    home: PathBuf,
    source: PathBuf,
    stamp: SourceStamp,
    generation: Generation,
}
static PIN_CACHE: LazyLock<Mutex<Vec<CachedGeneration>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn source_stamp(path: &Path) -> Result<SourceStamp> {
    let metadata = std::fs::metadata(path)?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let identity = (0, 0);
    Ok((
        metadata.len(),
        metadata.modified().ok(),
        metadata.created().ok(),
        identity.0,
        identity.1,
    ))
}

fn pin_source(home: &Path, source: &Path) -> Result<Generation> {
    let generations = root(home).join("generations");
    std::fs::create_dir_all(&generations)?;
    let staging = generations.join(format!(".new-{}", nonce()));
    let mut staging_lease = {
        let _store = store_lock(home)?;
        // Only unpublished copies are reclaimed, never executable generations.
        for entry in std::fs::read_dir(&generations)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry.file_name().to_string_lossy().starts_with(".new-")
            {
                if let Some(lease) = try_lock(&entry.path().join("pin.lock"))? {
                    drop(lease);
                    remove_staging(&entry.path())?;
                }
            }
        }
        std::fs::create_dir(&staging)?;
        Some(try_lock(&staging.join("pin.lock"))?.context("generation staging lease unavailable")?)
    };
    let temp = staging.join(if cfg!(windows) { "magi.exe" } else { "magi" });
    let result = (|| -> Result<Generation> {
        let mut input = File::open(source)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?;
        }
        let id = hex_digest(hash);
        let mut permissions = input.metadata()?.permissions();
        permissions.set_readonly(true);
        output.set_permissions(permissions)?;
        output.sync_all()?;
        drop(output);
        sync_dir(&staging)?;
        let target = generations.join(&id);
        // The store lock serializes publication with retention and other pins.
        let _store = store_lock(home)?;
        drop(staging_lease.take());
        std::fs::remove_file(staging.join("pin.lock"))?;
        if target.exists() {
            let existing = target.join(temp.file_name().context("binary filename")?);
            if hash_file(&existing)? != id {
                bail!("immutable worker generation is corrupt: {id}");
            }
        } else {
            std::fs::rename(&staging, &target)?;
            sync_dir(&generations)?;
        }
        Ok(Generation {
            executable: target.join(temp.file_name().context("binary filename")?),
            id,
        })
    })();
    let _store = store_lock(home)?;
    drop(staging_lease);
    if staging.exists() {
        remove_staging(&staging)?;
    }
    result
}

fn remove_staging(staging: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
        }
        const READONLY: u32 = 0x1;
        for entry in std::fs::read_dir(staging)? {
            let entry = entry?;
            let attributes = entry.metadata()?.file_attributes();
            if attributes & READONLY != 0 {
                let path: Vec<u16> = entry.path().as_os_str().encode_wide().chain([0]).collect();
                // SAFETY: path is a live, NUL-terminated UTF-16 buffer. Clear
                // only the Windows readonly attribute, preserving other bits.
                if unsafe { SetFileAttributesW(path.as_ptr(), attributes & !READONLY) } == 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("clear staging readonly attribute");
                }
            }
        }
    }
    Ok(std::fs::remove_dir_all(staging)?)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex_digest(hash))
}

fn reserve(
    home: &Path,
    generation: &Generation,
    kind: &str,
    key: &str,
    payload: serde_json::Value,
) -> Result<(Job, bool)> {
    if kind.is_empty() || key.is_empty() {
        bail!("worker kind and operation key must be nonempty");
    }
    let _store = store_lock(home)?;
    let mut jobs = records(home)?;
    reconcile(home, &mut jobs)?;
    if let Some(job) = jobs
        .iter()
        .find(|job| job.key == key && matches!(job.status, Status::Starting | Status::Running))
    {
        if job.kind != kind || job.payload != payload {
            bail!("worker operation key {key} is already reserved for a different request");
        }
        return Ok((job.clone(), false));
    }
    let id = nonce();
    let job = Job {
        id: id.clone(),
        sequence: next_sequence(home)?,
        kind: kind.to_owned(),
        key: key.to_owned(),
        payload,
        generation: generation.id.clone(),
        version: None,
        status: Status::Starting,
        result: None,
        pid: None,
        created_at: now(),
        finished_at: None,
    };
    let jobs_dir = root(home).join("jobs");
    let staging = jobs_dir.join(format!(".new-{id}"));
    std::fs::create_dir(&staging)
        .with_context(|| format!("create worker reservation staging {}", staging.display()))?;
    save(&staging.join("job.json"), &job)?;
    clear_control_locked(home, key)?;
    std::fs::rename(&staging, job_dir(home, &id)?)
        .with_context(|| format!("publish worker {id} reservation from {}", staging.display()))?;
    sync_dir(&jobs_dir)?;
    Ok((job, true))
}

fn prune_results(home: &Path) -> Result<()> {
    let results = root(home).join("results");
    std::fs::create_dir_all(&results)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&results)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            // Directory mtime changes when the terminal job.json is published.
            // Retention needs no deserialization of old handler/config payloads.
            entries.push((entry.metadata()?.modified()?, entry.path()));
        }
    }
    entries.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, dir) in entries.into_iter().skip(RETAIN_FINISHED) {
        if let Some(lease) = try_lock(&dir.join("live.lock"))? {
            drop(lease);
            std::fs::remove_dir_all(dir)?;
        }
    }
    sync_dir(&results)
}

fn detached_command(home: &Path, job: &Job, generation: &Generation) -> Result<Command> {
    // Open before exec so argument parsing, startup panics, and task progress
    // remain diagnosable even before the child can publish its acknowledgement.
    // Archiving/pruning the reservation also archives/prunes its log.
    let log_path = job_dir(home, &job.id)?.join("worker.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open worker startup log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("clone worker startup log {}", log_path.display()))?;
    let mut command = Command::new(&generation.executable);
    command
        .arg("__worker")
        .arg("--home")
        .arg(home)
        .arg(&job.id)
        .env("MAGI_HOME", home)
        .env("MAGI_WORKER_HOME", home)
        .env("MAGI_WORKER_ID", &job.id)
        .env("MAGI_WORKER_GENERATION", &generation.id)
        .env("MAGI_WORKER_EXE", &generation.executable)
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .env_remove("MAGI_SEAT")
        .env_remove("MAGI_TURN_SCOPE")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    let mut paths = vec![
        generation
            .executable
            .parent()
            .context("generation directory")?
            .to_path_buf(),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    command.env("PATH", std::env::join_paths(paths)?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // A scheduler's Ctrl-C handling must not be inherited as an
                // ignored shutdown signal by its independent worker session.
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(
            crate::chat_watch::windows::detached_flags()
                .context("prepare independent Windows worker")?,
        );
    }
    Ok(command)
}

/// Reserve an exclusive operation and detach its immutable executable. Returns
/// after acknowledgement; a timeout preserves the reservation and never kills
/// the child or silently executes in the caller. Repeated identical requests
/// attach to the existing reservation rather than dispatching twice.
pub async fn spawn(home: &Path, kind: &str, key: &str, payload: serde_json::Value) -> Result<Job> {
    std::fs::create_dir_all(home).map_err(not_started_error)?;
    let home = home
        .canonicalize()
        .context("resolve worker home")
        .map_err(not_started_error)?;
    let pin_home = home.clone();
    let generation = tokio::task::spawn_blocking(move || pin_current(&pin_home))
        .await
        .map_err(not_started_error)?
        .map_err(not_started_error)?;
    let (job, fresh) = reserve(&home, &generation, kind, key, payload)?;
    if fresh {
        match detached_command(&home, &job, &generation)
            .and_then(|mut command| Ok(command.spawn()?))
        {
            Ok(mut child) => {
                // std::process::Child has no kill-on-drop. The reaper is only
                // bookkeeping; runtime or parent exit cannot terminate a worker.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(error) => {
                let _store = store_lock(&home)?;
                let mut job = get(&home, &job.id)?;
                if job.status == Status::Starting {
                    if let Some(_lease) = try_lock(&job_dir(&home, &job.id)?.join("live.lock"))? {
                        terminal(
                            &home,
                            &mut job,
                            Status::Failed,
                            format!("worker spawn failed: {error}"),
                        )?;
                        return Err(not_started_error(error));
                    }
                }
                return Err(error);
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let current = get(&home, &job.id)?;
        match current.status {
            Status::Running | Status::Finished => return Ok(current),
            Status::Failed => bail!(
                "worker {} failed: {}",
                current.id,
                current.result.as_deref().unwrap_or("unknown failure")
            ),
            Status::Starting => {}
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "worker {} startup is unacknowledged; its reservation remains occupied; inspect or retry the same operation key",
                job.id
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Hidden detached entrypoint. The lease is acquired before acknowledgement and
/// is retained through handler result persistence. Only the recorded generation
/// can acknowledge a reservation, even if a newer CLI is accidentally invoked.
pub async fn run(home: &Path, id: &str) -> Result<()> {
    let dir = job_dir(home, id)?;
    let (job, _lease) = {
        let _store = store_lock(home)?;
        let mut job = get(home, id)?;
        if job.status != Status::Starting {
            bail!("worker reservation is no longer startable");
        }
        let lease =
            try_lock(&dir.join("live.lock"))?.context("worker execution lease is already held")?;
        let current = std::env::current_exe()
            .context("locate running worker executable")?
            .canonicalize()
            .context("resolve running worker executable")?;
        let expected = executable(home, &job.generation)?
            .canonicalize()
            .with_context(|| {
                format!("resolve worker {id} reserved generation {}", job.generation)
            })?;
        if current != expected {
            bail!("worker must execute its reserved immutable generation");
        }
        job.pid = Some(std::process::id());
        job.version = Some(env!("CARGO_PKG_VERSION").to_owned());
        job.status = Status::Running;
        save(&dir.join("job.json"), &job)
            .with_context(|| format!("acknowledge worker {id} startup"))?;
        (job, lease)
    };
    let result = match job.kind.as_str() {
        "task" => crate::daemon::run_task_worker(home, job.payload.clone()).await,
        "scheduler" => crate::daemon::run_scheduler_worker(home, job.payload.clone()).await,
        "chat" => crate::consult::run_turn_worker(home, job.payload.clone()).await,
        "service" => crate::consult::run_service_worker(home, job.payload.clone()).await,
        "daemon-service" => crate::daemon::run_service_worker(home, job.payload.clone()).await,
        "deputy" => crate::deputy::run_worker(home, job.payload.clone()).await,
        kind => Err(anyhow::anyhow!("unknown worker kind {kind}")),
    };
    let _store = store_lock(home)?;
    let mut current = get(home, id)?;
    match &result {
        Ok(()) => terminal(home, &mut current, Status::Finished, "completed".to_owned())?,
        Err(error) => terminal(home, &mut current, Status::Failed, format!("{error:#}"))?,
    }
    result
}

fn hex_digest(hash: Sha256) -> String {
    format!("{:x}", hash.finalize())
}
fn digest(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hex_digest(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observing_an_unused_store_requires_no_writes() {
        let home = tempfile::tempdir().unwrap();
        assert!(observe_active(home.path()).unwrap().is_empty());
        assert!(!home.path().join("workers").exists());
    }

    #[test]
    fn observing_execution_probes_the_lease_without_rewriting_history() {
        let home = tempfile::tempdir().unwrap();
        let mut job = reservation(home.path(), "task:observer");
        let directory = job_dir(home.path(), &job.id).unwrap();
        let live_path = directory.join("live.lock");
        let lease = try_lock(&live_path).unwrap().unwrap();
        job.status = Status::Running;
        save(&directory.join("job.json"), &job).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&live_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        let before = std::fs::read(directory.join("job.json")).unwrap();
        let observed = observe_active(home.path()).unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].id, job.id);
        drop(lease);
        assert!(observe_active(home.path()).unwrap().is_empty());
        assert_eq!(std::fs::read(directory.join("job.json")).unwrap(), before);
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Running);
    }
    fn generation() -> Generation {
        Generation {
            id: "a".repeat(64),
            executable: PathBuf::from("unused"),
        }
    }
    fn reservation(home: &Path, key: &str) -> Job {
        reserve(
            home,
            &generation(),
            "task",
            key,
            serde_json::json!({"task":"example"}),
        )
        .unwrap()
        .0
    }

    #[test]
    fn reservation_deduplicates_identical_keys_and_rejects_collision() {
        let home = tempfile::tempdir().unwrap();
        let first = reservation(home.path(), "task:one");
        assert_eq!(first.id, reservation(home.path(), "task:one").id);
        assert!(
            reserve(
                home.path(),
                &generation(),
                "chat",
                "task:one",
                serde_json::Value::Null
            )
            .is_err()
        );
        assert_ne!(first.id, reservation(home.path(), "task:two").id);
    }

    #[test]
    fn reservation_order_is_durable_and_corruption_never_resets_it() {
        let home = tempfile::tempdir().unwrap();
        let first = reservation(home.path(), "chat:first");
        assert_eq!(
            first.sequence,
            reservation(home.path(), "chat:first").sequence
        );
        let second = reservation(home.path(), "chat:second");
        assert!(second.sequence > first.sequence);
        let encoded = serde_json::to_vec(&second).unwrap();
        assert_eq!(
            serde_json::from_slice::<Job>(&encoded).unwrap().sequence,
            second.sequence
        );
        let mut legacy = serde_json::to_value(&first).unwrap();
        legacy.as_object_mut().unwrap().remove("sequence");
        assert_eq!(serde_json::from_value::<Job>(legacy).unwrap().sequence, 0);
        std::fs::write(root(home.path()).join("sequence.json"), b"broken").unwrap();
        assert!(
            reserve(
                home.path(),
                &generation(),
                "chat",
                "chat:third",
                serde_json::Value::Null
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn pre_reservation_failure_is_proven_without_history_absence_guesses() {
        let home = tempfile::tempdir().unwrap();
        let blocked = home.path().join("not-a-directory");
        std::fs::write(&blocked, b"file").unwrap();
        let error = spawn(&blocked, "chat", "fresh:chat", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(not_started(&error));
        assert!(not_started(&error.context("publish recorded turn")));
        assert!(!not_started(&anyhow::anyhow!(
            "startup acknowledgement timed out"
        )));
        assert!(!blocked.join("workers").exists());
    }

    #[test]
    fn failed_os_spawn_is_fenced_and_proven_not_started() {
        let home = tempfile::tempdir().unwrap();
        let executable = home.path().join("invalid-executable");
        std::fs::write(&executable, b"not an executable binary").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::tests::unstarted_spawn_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("MAGI_WORKER_EXE", &executable)
            .env("MAGI_SPAWN_FAILURE_HOME", home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    #[ignore = "isolated environment for the failed OS spawn test"]
    async fn unstarted_spawn_helper() {
        let home = PathBuf::from(std::env::var_os("MAGI_SPAWN_FAILURE_HOME").unwrap());
        let error = spawn(&home, "chat", "fresh:failed-exec", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(not_started(&error));
        let entry = std::fs::read_dir(root(&home).join("jobs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let id = entry.file_name().to_str().unwrap().to_owned();
        let job = get(&home, &id).unwrap();
        assert_eq!(job.status, Status::Failed);
        assert_eq!(job.pid, None);
        assert!(active(&home).unwrap().is_empty());
        assert_eq!(get(&home, &id).unwrap().status, Status::Failed);
    }

    #[test]
    fn unresolved_startup_remains_reserved_and_live_lease_never_expires() {
        let home = tempfile::tempdir().unwrap();
        let mut job = reservation(home.path(), "task:one");
        assert_eq!(active(home.path()).unwrap().len(), 1);
        job.created_at = 0;
        save(
            &job_dir(home.path(), &job.id).unwrap().join("job.json"),
            &job,
        )
        .unwrap();
        let lease = try_lock(&job_dir(home.path(), &job.id).unwrap().join("live.lock"))
            .unwrap()
            .unwrap();
        assert_eq!(active(home.path()).unwrap().len(), 1);
        drop(lease);
        assert!(active(home.path()).unwrap().is_empty());
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Failed);
    }

    #[tokio::test]
    async fn cancelled_startup_cannot_execute_late() {
        let home = tempfile::tempdir().unwrap();
        let mut job = reservation(home.path(), "task:one");
        job.created_at = 0;
        save(
            &job_dir(home.path(), &job.id).unwrap().join("job.json"),
            &job,
        )
        .unwrap();
        assert!(active(home.path()).unwrap().is_empty());
        assert!(
            run(home.path(), &job.id)
                .await
                .unwrap_err()
                .to_string()
                .contains("no longer startable")
        );
    }

    #[test]
    fn running_without_lease_fails_regardless_of_pid() {
        let home = tempfile::tempdir().unwrap();
        let mut job = reservation(home.path(), "task:one");
        job.status = Status::Running;
        job.pid = Some(std::process::id());
        save(
            &job_dir(home.path(), &job.id).unwrap().join("job.json"),
            &job,
        )
        .unwrap();
        assert!(active(home.path()).unwrap().is_empty());
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Failed);
    }

    #[test]
    fn controls_survive_reopen_and_reset_at_new_invocation() {
        let home = tempfile::tempdir().unwrap();
        request(home.path(), "task:one", Control::Park).unwrap();
        assert_eq!(
            control(home.path(), "task:one").unwrap(),
            Some(Control::Park)
        );
        reservation(home.path(), "task:one");
        assert_eq!(control(home.path(), "task:one").unwrap(), None);
        request(home.path(), "task:one", Control::Stop).unwrap();
        assert_eq!(
            control(home.path(), "task:one").unwrap(),
            Some(Control::Stop)
        );
        assert_eq!(control(home.path(), "task:other").unwrap(), None);
    }

    #[test]
    fn singleton_locks_release_without_pid_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let lease = try_lease(home.path(), "scheduler").unwrap().unwrap();
        assert!(try_lease(home.path(), "scheduler").unwrap().is_none());
        assert!(try_lease(home.path(), "other").unwrap().is_some());
        drop(lease);
        assert!(try_lease(home.path(), "scheduler").unwrap().is_some());
    }

    #[test]
    fn immutable_generation_keeps_old_content_after_source_replacement() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        std::fs::write(&source, b"old executable").unwrap();
        let old = pin_source(home.path(), &source).unwrap();
        std::fs::write(&source, b"new executable").unwrap();
        let new = pin_source(home.path(), &source).unwrap();
        assert_ne!(old.id, new.id);
        assert_eq!(
            std::fs::read(executable(home.path(), &old.id).unwrap()).unwrap(),
            b"old executable"
        );
        assert_eq!(std::fs::read(new.executable).unwrap(), b"new executable");
    }

    #[test]
    fn acknowledgement_records_own_version_and_handler_failure() {
        let home = tempfile::tempdir().unwrap();
        let generation = pin_source(home.path(), &std::env::current_exe().unwrap()).unwrap();
        let (job, _) = reserve(
            home.path(),
            &generation,
            "unknown-kind",
            "unknown:one",
            serde_json::Value::Null,
        )
        .unwrap();
        let invoke = || {
            let output = Command::new(&generation.executable)
                .args([
                    "--exact",
                    "worker::tests::pinned_entrypoint_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("MAGI_WORKER_TEST_HOME", home.path())
                .env("MAGI_WORKER_TEST_ID", &job.id)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        invoke();
        let failed = get(home.path(), &job.id).unwrap();
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(failed.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert!(failed.result.unwrap().contains("unknown worker kind"));
        assert!(active(home.path()).unwrap().is_empty());
        assert!(!job_dir(home.path(), &job.id).unwrap().exists());
        invoke();
        assert!(!job_dir(home.path(), &job.id).unwrap().exists());
    }

    #[tokio::test]
    #[ignore = "subprocess entrypoint for the immutable-generation startup test"]
    async fn pinned_entrypoint_helper() {
        let home = PathBuf::from(std::env::var_os("MAGI_WORKER_TEST_HOME").unwrap());
        let id = std::env::var("MAGI_WORKER_TEST_ID").unwrap();
        assert!(run(&home, &id).await.is_err());
    }

    #[test]
    fn unfinished_staging_is_reclaimed_without_removing_generations() {
        let home = tempfile::tempdir().unwrap();
        let old = root(home.path()).join("generations").join(".new-abandoned");
        std::fs::create_dir_all(&old).unwrap();
        let partial = old.join("partial");
        std::fs::write(&partial, b"interrupted copy").unwrap();
        #[cfg(windows)]
        {
            let mut permissions = std::fs::metadata(&partial).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&partial, permissions).unwrap();
        }
        let source = home.path().join("source");
        std::fs::write(&source, b"complete executable").unwrap();
        let generation = pin_source(home.path(), &source).unwrap();
        assert!(!old.exists());
        assert!(generation.executable.is_file());
    }

    #[tokio::test]
    async fn unrelated_executable_cannot_acknowledge_or_consume_reservation() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("other-binary");
        std::fs::write(&source, b"different executable of the same Cargo version").unwrap();
        let generation = pin_source(home.path(), &source).unwrap();
        let (job, _) = reserve(
            home.path(),
            &generation,
            "task",
            "task:one",
            serde_json::Value::Null,
        )
        .unwrap();
        assert!(
            run(home.path(), &job.id)
                .await
                .unwrap_err()
                .to_string()
                .contains("immutable generation")
        );
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Starting);
        assert_eq!(active(home.path()).unwrap().len(), 1);
    }

    #[test]
    fn abandoned_record_staging_is_bounded_and_never_executes() {
        let home = tempfile::tempdir().unwrap();
        let staging = root(home.path()).join("jobs").join(".new-interrupted");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("job.json"), b"partial json").unwrap();
        assert!(active(home.path()).unwrap().is_empty());
        assert!(!staging.exists());
    }

    #[cfg(unix)]
    #[test]
    fn detached_startup_preserves_stdout_and_stderr_in_record_log() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("startup-diagnostic");
        std::fs::write(
            &source,
            b"#!/bin/sh\nprintf 'startup stdout\\n'\nprintf 'startup stderr\\n' >&2\nexit 7\n",
        )
        .unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        let generation = pin_source(home.path(), &source).unwrap();
        let (mut job, _) = reserve(
            home.path(),
            &generation,
            "task",
            "task:log",
            serde_json::Value::Null,
        )
        .unwrap();
        let mut command = detached_command(home.path(), &job, &generation).unwrap();
        for key in ["MAGI_RUN", "MAGI_NODE", "MAGI_SEAT", "MAGI_TURN_SCOPE"] {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == key && value.is_none())
            );
        }
        assert_eq!(command.spawn().unwrap().wait().unwrap().code(), Some(7));
        let log = job_dir(home.path(), &job.id).unwrap().join("worker.log");
        let output = std::fs::read_to_string(log).unwrap();
        assert!(output.contains("startup stdout"));
        assert!(output.contains("startup stderr"));
        terminal(
            home.path(),
            &mut job,
            Status::Failed,
            "startup failed".into(),
        )
        .unwrap();
        assert!(active(home.path()).unwrap().is_empty());
        assert!(
            root(home.path())
                .join("results")
                .join(&job.id)
                .join("worker.log")
                .is_file()
        );
    }

    #[cfg(windows)]
    #[test]
    fn record_publication_survives_readers_and_short_delete_sharing_handoffs() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let home = tempfile::tempdir().unwrap();
        let mut job = reservation(home.path(), "task:metadata-handoff");
        let directory = job_dir(home.path(), &job.id).unwrap();
        let path = directory.join("job.json");
        let lease = try_lock(&directory.join("live.lock")).unwrap().unwrap();
        let snapshot = File::open(&path).unwrap();
        // This reader allows ordinary IO but temporarily denies replacement.
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(1 | 2) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .open(&path)
            .unwrap();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(15));
                drop(reader);
            });
            job.status = Status::Running;
            save(&path, &job).unwrap();
        });
        let original: Job = serde_json::from_reader(snapshot).unwrap();
        assert_eq!(original.status, Status::Starting);
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Running);
        let running_snapshot = File::open(&path).unwrap();
        terminal(home.path(), &mut job, Status::Finished, "completed".into()).unwrap();
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Finished);
        let previous: Job = serde_json::from_reader(running_snapshot).unwrap();
        assert_eq!(previous.status, Status::Running);
        drop(lease);
        assert!(active(home.path()).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn denied_record_publication_preserves_reservation_and_original_os_error() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let home = tempfile::tempdir().unwrap();
        let job = reservation(home.path(), "task:metadata-denied");
        let directory = job_dir(home.path(), &job.id).unwrap();
        let path = directory.join("job.json");
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(1 | 2) // Deny deletion for the entire handoff budget.
            .open(&path)
            .unwrap();
        let mut acknowledged = job.clone();
        acknowledged.status = Status::Running;
        let error = save(&path, &acknowledged).unwrap_err();
        assert!(format!("{error:#}").contains("publish worker record"));
        let io = error.downcast_ref::<std::io::Error>().unwrap();
        assert!(matches!(io.raw_os_error(), Some(5 | 32)));
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Starting);
        assert_eq!(reservation(home.path(), &job.key).id, job.id);
        assert!(std::fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pending-")
        }));
        drop(reader);
        save(&path, &acknowledged).unwrap();
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Running);
    }

    #[test]
    fn terminal_result_survives_open_execution_and_output_handles() {
        let home = tempfile::tempdir().unwrap();
        let mut job = reservation(home.path(), "task:open-handles");
        let directory = job_dir(home.path(), &job.id).unwrap();
        let lease = try_lock(&directory.join("live.lock")).unwrap().unwrap();
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("worker.log"))
            .unwrap();
        writeln!(log, "before completion").unwrap();
        terminal(home.path(), &mut job, Status::Finished, "completed".into()).unwrap();
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Finished);
        assert!(active(home.path()).unwrap().is_empty());
        assert!(
            directory.exists(),
            "a held execution lease must prevent retirement"
        );
        drop(lease);
        // Retirement may still be denied by the output handle on Windows.
        // Either way, querying active work and the durable result must succeed.
        assert!(active(home.path()).unwrap().is_empty());
        assert_eq!(get(home.path(), &job.id).unwrap().status, Status::Finished);
        writeln!(log, "after completion").unwrap();
        drop(log);
        assert!(active(home.path()).unwrap().is_empty());
        assert!(
            root(home.path())
                .join("results")
                .join(&job.id)
                .join("worker.log")
                .is_file()
        );
        assert!(!directory.exists());
    }

    #[test]
    fn terminal_records_are_bounded_without_pruning_live_reservations() {
        let home = tempfile::tempdir().unwrap();
        for i in 0..RETAIN_FINISHED + 3 {
            let mut job = reservation(home.path(), &format!("task:{i}"));
            terminal(home.path(), &mut job, Status::Finished, "done".into()).unwrap();
        }
        let live = reservation(home.path(), "task:live");
        let jobs = records(home.path()).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            std::fs::read_dir(root(home.path()).join("results"))
                .unwrap()
                .count(),
            RETAIN_FINISHED
        );
        assert!(jobs.iter().any(|job| job.id == live.id));
    }
}
