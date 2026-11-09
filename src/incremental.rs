//! Disposable incremental generations with creation-time, per-file provenance.
//!
//! Legacy incremental directories are never adopted. A generation is fresh for
//! one compiler invocation; incomplete or changed generations consume their slot
//! until an operator investigates. This is a bound, not authority to wipe them.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Stable resource-admission marker emitted by the private compiler wrapper.
pub const BLOCKED: &str = "incremental cache admission blocked";

const SLOTS: usize = 4;
const GENERATION_BYTES: u64 = 128 * 1024 * 1024;
const STORE: &str = "magi-incremental";

/// Forked subprocesses can retain a copy until exec closes it. Explicitly
/// release our flock at the ownership boundary rather than waiting for every
/// inherited descriptor to close. Session acquisition queries POSIX locks and
/// then holds flock, so all these guards use the same release protocol.
#[derive(Debug)]
struct FileLease(File);

impl std::ops::Deref for FileLease {
    type Target = File;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for FileLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the guard retains its live descriptor through unlock.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(crate) fn default_environment(command: &mut tokio::process::Command) {
    let explicit = command
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == "CARGO_INCREMENTAL");
    let supplied = match explicit {
        Some((_, value)) => value.is_some(),
        None => std::env::var_os("CARGO_INCREMENTAL").is_some(),
    };
    if !supplied {
        command.env("CARGO_INCREMENTAL", "0");
    }
}

/// Configure a managed command, preserving explicit environment overrides and
/// Cargo's workspace wrapper chain. The wrapper sees shell-level opt-ins too.
pub(crate) fn configure(command: &mut tokio::process::Command, worktree: &Path) -> Result<()> {
    default_environment(command);
    #[cfg(not(unix))]
    let _ = worktree;
    #[cfg(unix)]
    let env: BTreeMap<_, _> = command
        .as_std()
        .get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(ToOwned::to_owned)))
        .collect();
    #[cfg(unix)]
    let value = |key: &str| {
        env.get(std::ffi::OsStr::new(key))
            .cloned()
            .unwrap_or_else(|| std::env::var_os(key))
    };
    #[cfg(unix)]
    if worktree.join(".git").is_file() {
        let git = git_dir(worktree)?;
        let (name, body) = wrapper_spec()?;
        let script = git.join(name);
        publish_wrapper(&script, &body, || {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            git.join(format!(
                "magi-rustc-{}-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4()
            ))
        })?;
        let previous = value("RUSTC_WRAPPER");
        let previous = if previous.is_some() && previous == value("MAGI_INCREMENTAL_WRAPPER") {
            value("MAGI_PREVIOUS_RUSTC_WRAPPER")
        } else {
            previous
        };
        command.env_remove("MAGI_PREVIOUS_RUSTC_WRAPPER");
        if let Some(previous) = previous.filter(|v| !v.is_empty()) {
            // Re-entering a managed command must not wrap our own wrapper.
            if previous != script.as_os_str() {
                command.env("MAGI_PREVIOUS_RUSTC_WRAPPER", previous);
            }
        }
        command
            .env("MAGI_INCREMENTAL_WORKTREE", worktree.canonicalize()?)
            .env("MAGI_INCREMENTAL_WRAPPER", &script)
            .env("RUSTC_WRAPPER", script);
    }
    Ok(())
}

/// Only a successfully reserved path belongs to this invocation. In particular,
/// a failed create_new must never remove another publisher's temporary file.
#[cfg(unix)]
struct WrapperTemporary {
    path: PathBuf,
    file: File,
}

#[cfg(unix)]
impl WrapperTemporary {
    fn create(mut candidate: impl FnMut() -> PathBuf) -> std::io::Result<Self> {
        for _ in 0..128 {
            let path = candidate();
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok(Self { path, file }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve an incremental wrapper temporary file",
        ))
    }
}

#[cfg(unix)]
impl Drop for WrapperTemporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn wrapper_spec() -> Result<(String, String)> {
    let executable = std::env::current_exe()?;
    let hash: String = Sha256::digest(executable.as_os_str().as_encoded_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let name = format!("magi-rustc-{hash}.sh");
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
    let body = format!(
        "#!/bin/sh\nexec {} __magi_incremental \"$@\"\n",
        quote(&executable)
    );
    Ok((name, body))
}

/// Execution may encounter the exact compiler wrapper its own setup created.
/// This is not an exemption for arbitrary administration during eviction.
pub(crate) fn current_wrapper(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        let (name, body) = wrapper_spec()?;
        if path.file_name() != Some(std::ffi::OsStr::new(&name)) {
            return Ok(false);
        }
        Ok(plain(path)?.is_file() && fs::read(path)? == body.as_bytes())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(unix)]
fn publish_wrapper(script: &Path, body: &str, candidate: impl FnMut() -> PathBuf) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut temporary = WrapperTemporary::create(candidate)?;
    temporary.file.write_all(body.as_bytes())?;
    temporary
        .file
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    // Link only a complete script, without replacing an existing path.
    // Concurrent command preparation must never observe a partial body.
    match fs::hard_link(&temporary.path, script) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                plain(script)?.is_file() && fs::read(script)? == body.as_bytes(),
                "incremental wrapper changed"
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn git_dir(worktree: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .current_dir(worktree)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()?;
    ensure!(
        output.status.success(),
        "incremental provenance requires a Git worktree"
    );
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

fn plain(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink() && path.canonicalize()? == path,
        "incremental path is not canonical: {}",
        path.display()
    );
    Ok(metadata)
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    bytes: u64,
    digest: Vec<u8>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn identity(path: &Path) -> Result<Identity> {
    let metadata = plain(path)?;
    ensure!(
        metadata.is_file(),
        "incremental entry is not a regular file"
    );
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Identity {
        bytes: metadata.len(),
        digest: hash.finalize().to_vec(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    version: u32,
    worktree: PathBuf,
    generation: PathBuf,
    files: BTreeMap<PathBuf, Identity>,
    directories: std::collections::BTreeSet<PathBuf>,
}

pub(crate) struct Generation {
    path: PathBuf,
    record: PathBuf,
    // The session lock is outside the cache, and lives through compiler exit.
    _lock: FileLease,
}

impl Generation {
    pub(crate) fn create(worktree: &Path, parent: &Path) -> Result<Self> {
        let target = worktree.join("target");
        ensure!(
            [
                target.join("debug/incremental"),
                target.join("release/incremental")
            ]
            .contains(&parent.to_owned()),
            "explicit incremental requires a local managed target; shared/custom targets remain non-incremental"
        );
        // Cargo creates these parents before invoking rustc. Never follow an alias.
        ensure!(
            plain(&target)?.is_dir() && plain(parent)?.is_dir(),
            "incremental parent is not a directory"
        );
        let store = git_dir(worktree)?.join(STORE);
        fs::create_dir_all(&store)?;
        ensure!(
            plain(&store)?.is_dir(),
            "invalid incremental provenance store"
        );
        // Hash immutable completed generations before taking the namespace
        // lock. Shared owner guards keep these snapshots stable while other
        // compiler admissions independently inspect the same completions.
        let mut completed = BTreeMap::new();
        let mut inspected = 0usize;
        for entry in fs::read_dir(&store)? {
            let owner = entry?.path();
            if owner.extension().is_some_and(|x| x == "owner") {
                inspected += 1;
                if inspected > 1024 {
                    break;
                }
                if let Ok(snapshot) = completed_charge(worktree, &owner) {
                    completed.insert(owner, snapshot);
                }
            }
        }
        let mut options = OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let admission = FileLease(
            options
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(store.join("admission.lock"))?,
        );
        wait_for_admission(&admission, std::time::Duration::from_secs(30))?;
        // Reset/clean can remove sealed cache data without its Git-side proof.
        // Forget only completed, absent generations, never unsealed attempts.
        for entry in fs::read_dir(&store)? {
            let record = entry?.path();
            if record.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let cleanup = || -> Result<()> {
                ensure!(
                    plain(&record)?.is_file() && fs::metadata(&record)?.len() < 4 * 1024 * 1024,
                    "invalid incremental proof"
                );
                let proof: Proof = serde_json::from_slice(&fs::read(&record)?)?;
                validate_proof(worktree, &record, &proof)?;
                ensure!(
                    matches!(fs::symlink_metadata(&proof.generation), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                    "generation still exists or is uninspectable"
                );
                let owner = record.with_extension("owner");
                ensure!(
                    plain(&owner)?.is_file() && fs::metadata(&owner)?.len() == 0,
                    "invalid incremental owner"
                );
                let lock = FileLease(OpenOptions::new().read(true).write(true).open(&owner)?);
                crate::pr_head::try_lock_file(&lock)?;
                fs::remove_file(&record)?;
                fs::remove_file(owner)?;
                Ok(())
            };
            let _ = cleanup(); // Refusal retains both proof and its reservation.
        }
        // Live and unproven attempts reserve a full generation. Completed,
        // unchanged generations consume their actual bytes, not a lifetime slot.
        let mut charged = 0u64;
        let mut records = 0usize;
        for entry in fs::read_dir(&store)? {
            let owner = entry?.path();
            if owner.extension().is_some_and(|x| x == "owner") {
                records += 1;
                let bytes = completed
                    .get(&owner)
                    .and_then(|(bytes, guard)| snapshot_charge(&owner, *bytes, guard))
                    .unwrap_or(GENERATION_BYTES);
                charged = charged.saturating_add(bytes);
            }
        }
        ensure!(
            records < 1024,
            "managed incremental provenance record limit exhausted; inspect retention report"
        );
        ensure!(
            charged.saturating_add(GENERATION_BYTES) <= SLOTS as u64 * GENERATION_BYTES,
            "managed incremental budget exhausted (512 MiB including active/unproven reservations); retain unproven caches and inspect retention report"
        );
        let token = crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4();
        let record = store.join(format!("{token}.json"));
        let lock = FileLease(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(record.with_extension("owner"))?,
        );
        crate::pr_head::try_lock_file(&lock)?;
        let path = parent.join(format!("magi-{token}"));
        fs::create_dir(&path)?;
        Ok(Self {
            path,
            record,
            _lock: lock,
        })
    }

    pub(crate) fn seal(&self, worktree: &Path) -> Result<()> {
        finalized_sessions(&self.path)?;
        let files = inventory(&self.path, true)?;
        let bytes = files
            .values()
            .fold(0u64, |sum, f| sum.saturating_add(f.bytes));
        ensure!(
            bytes <= GENERATION_BYTES,
            "managed incremental generation exceeds its 128 MiB budget"
        );
        let proof = Proof {
            version: 1,
            worktree: worktree.to_owned(),
            generation: self.path.clone(),
            files,
            directories: directory_inventory(&self.path)?,
        };
        let temp = self.record.with_extension("tmp");
        fs::write(&temp, serde_json::to_vec(&proof)?)?;
        fs::rename(temp, &self.record)?;
        Ok(())
    }
}

// A successful compiler may leave an abandoned working session. Creation-time
// ownership alone does not make arbitrary directories or lock names disposable.
fn finalized_sessions(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let crate_dir = entry?.path();
        ensure!(
            plain(&crate_dir)?.is_dir(),
            "unknown incremental root entry"
        );
        let mut sessions = Vec::new();
        let mut locks = Vec::new();
        for entry in fs::read_dir(&crate_dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .context("missing incremental name")?
                .to_str()
                .context("invalid incremental name")?
                .to_owned();
            if plain(&path)?.is_dir() {
                ensure!(
                    name.starts_with("s-")
                        && !name.ends_with("-working")
                        && name.split('-').count() == 4,
                    "incremental session is unfinished or unknown"
                );
                for output in fs::read_dir(&path)? {
                    ensure!(
                        plain(&output?.path())?.is_file(),
                        "unknown incremental session directory"
                    );
                }
                sessions.push(name);
            } else {
                ensure!(
                    name.starts_with("s-")
                        && name.ends_with(".lock")
                        && name.split('-').count() == 3,
                    "unknown incremental session lock"
                );
                locks.push(name);
            }
        }
        ensure!(
            !sessions.is_empty() && sessions.len() == locks.len(),
            "incremental session/lock inventory is incomplete"
        );
        for lock in locks {
            let prefix = format!("{}-", lock.strip_suffix(".lock").unwrap());
            ensure!(
                sessions.iter().any(|name| name.starts_with(&prefix)),
                "incremental session lock has no completed session"
            );
        }
    }
    Ok(())
}

// Contention is a short serialization wait, distinct from insufficient budget.
fn wait_for_admission(file: &File, timeout: std::time::Duration) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        match crate::pr_head::try_lock_file(file) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error).context("incremental generation admission lock"),
        }
        ensure!(
            started.elapsed() < timeout,
            "incremental generation admission wait timed out after {} ms",
            timeout.as_millis()
        );
        std::thread::sleep(
            std::time::Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())),
        );
    }
}

fn snapshot_charge(owner: &Path, bytes: u64, guard: &File) -> Option<u64> {
    let current = plain(owner).ok()?;
    let held = guard.metadata().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (current.is_file()
            && current.len() == 0
            && current.dev() == held.dev()
            && current.ino() == held.ino())
        .then_some(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = (current, held, bytes);
        None
    }
}

fn lock_for_accounting(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: this live descriptor is retained with the completed snapshot.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        crate::pr_head::try_lock_file(file)
    }
}

// Accounting grants no deletion authority. A shared owner guard and an exact
// completion proof may reduce the conservative full-generation reservation.
fn completed_charge(worktree: &Path, owner: &Path) -> Result<(u64, FileLease)> {
    ensure!(
        plain(owner)?.is_file() && fs::metadata(owner)?.len() == 0,
        "invalid incremental owner"
    );
    let record = owner.with_extension("json");
    // An unpublished owner may still be acquiring its initial exclusive lock.
    // Do not briefly share-lock it before a completion proof even exists.
    ensure!(
        plain(&record)?.is_file() && fs::metadata(&record)?.len() < 4 * 1024 * 1024,
        "invalid incremental proof"
    );
    let lock = FileLease(OpenOptions::new().read(true).write(true).open(owner)?);
    lock_for_accounting(&lock)?;
    let proof: Proof = serde_json::from_slice(&fs::read(record)?)?;
    validate_proof(worktree, &owner.with_extension("json"), &proof)?;
    let actual = inventory(&proof.generation, false)?;
    ensure!(
        directory_inventory(&proof.generation)?.is_subset(&proof.directories)
            && actual
                .iter()
                .all(|(p, identity)| proof.files.get(p) == Some(identity)),
        "incremental proof changed"
    );
    let bytes = actual
        .values()
        .fold(0u64, |bytes, identity| bytes.saturating_add(identity.bytes));
    Ok((bytes, lock))
}

fn inventory(root: &Path, recognize: bool) -> Result<BTreeMap<PathBuf, Identity>> {
    let mut files = BTreeMap::new();
    let mut bytes = 0u64;
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        ensure!(plain(&dir)?.is_dir(), "invalid incremental directory");
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let metadata = plain(&path)?;
            if metadata.is_dir() {
                dirs.push(path);
            } else {
                ensure!(
                    metadata.is_file(),
                    "incremental entry is not a regular file"
                );
                bytes = bytes.saturating_add(metadata.len());
                ensure!(
                    bytes <= GENERATION_BYTES,
                    "incremental generation exceeds its budget"
                );
                ensure!(files.len() < 10000, "incremental inventory too large");
                if recognize {
                    ensure!(
                        recognized(&path)?,
                        "unknown incremental output: {}",
                        path.display()
                    );
                }
                files.insert(path.strip_prefix(root)?.to_owned(), identity(&path)?);
            }
        }
    }
    Ok(files)
}

fn directory_inventory(root: &Path) -> Result<std::collections::BTreeSet<PathBuf>> {
    let mut found = std::collections::BTreeSet::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        ensure!(plain(&dir)?.is_dir(), "invalid incremental directory");
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if plain(&path)?.is_dir() {
                ensure!(
                    found.len() < 10000,
                    "incremental directory inventory too large"
                );
                found.insert(path.strip_prefix(root)?.to_owned());
                dirs.push(path);
            }
        }
    }
    Ok(found)
}

fn recognized(path: &Path) -> Result<bool> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if name.ends_with(".lock") {
        return Ok(fs::metadata(path)?.len() == 0);
    }
    let mut header = [0; 8];
    let n = File::open(path)?.read(&mut header)?;
    if matches!(
        name.as_ref(),
        "dep-graph.bin" | "query-cache.bin" | "work-products.bin"
    ) {
        return Ok(header[..n].starts_with(b"RSIC"));
    }
    if path.extension().is_some_and(|e| e == "o") || name == "metadata.rmeta" {
        return crate::retention::artifact_signature(path);
    }
    Ok(false)
}

/// Check both Unix lock protocols used by compilers. Query the POSIX lock
/// without acquiring it: holding both protocols can conflict on BSD. Retain
/// the flock guard through unlink, alongside the caller's output/Cargo locks.
/// See https://doc.rust-lang.org/stable/nightly-rustc/rustc_data_structures/flock/.
#[cfg(unix)]
fn lock_session(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is live and the initialized record covers the
    // whole file. F_GETLK only inspects locks; it does not change them.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 0;
    lock.l_len = 0;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let unlocked: libc::c_short = libc::F_UNLCK as _;
    if lock.l_type != unlocked {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "incremental session has a POSIX record lock",
        ));
    }
    crate::pr_head::try_lock_file(file)
}

#[cfg(all(unix, test))]
fn lock_posix_session(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: zero initializes reserved fields; fcntl borrows the lock record
    // only during this call and the test helper retains the live descriptor.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 0;
    lock.l_len = 0;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn lock_session(file: &File) -> std::io::Result<()> {
    crate::pr_head::try_lock_file(file)
}

/// Handle the private wrapper entrypoint before normal CLI startup. Existing
/// wrappers (including Cargo's workspace wrapper argument) stay in the chain.
pub fn wrapper(args: Vec<OsString>) -> Result<i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(wrapper_inner(args))
}

async fn wrapper_inner(args: Vec<OsString>) -> Result<i32> {
    let worktree = PathBuf::from(
        std::env::var_os("MAGI_INCREMENTAL_WORKTREE").context("missing incremental worktree")?,
    );
    let mut args = args;
    let mut generation = None;
    for index in 0..args.len() {
        let text = args[index].to_string_lossy();
        let value = if index > 0 && args[index - 1] == "-C" {
            text.strip_prefix("incremental=").map(str::to_owned)
        } else {
            text.strip_prefix("-Cincremental=").map(str::to_owned)
        };
        if let Some(value) = value {
            ensure!(
                generation.is_none(),
                "multiple incremental compiler paths are unsupported"
            );
            let parent = PathBuf::from(value);
            let parent = if parent.is_absolute() {
                parent
            } else {
                std::env::current_dir()?.join(parent)
            };
            // Cargo passes the profile incremental root; rustc creates its own
            // crate/session subdirectories beneath our fresh generation.
            let made = Generation::create(&worktree, &parent)?;
            let prefix = if text.starts_with("-C") {
                "-Cincremental="
            } else {
                "incremental="
            };
            args[index] = format!("{prefix}{}", made.path.display()).into();
            generation = Some(made);
        }
    }
    let command = if let Some(previous) =
        std::env::var_os("MAGI_PREVIOUS_RUSTC_WRAPPER").filter(|s| !s.is_empty())
    {
        let mut command = std::process::Command::new(previous);
        command.args(&args);
        command
    } else {
        let mut command = std::process::Command::new(args.first().context("missing compiler")?);
        command.args(&args[1..]);
        command
    };
    let mut running =
        crate::seat_process::Running::spawn_operation(command.into(), None, Vec::new())?;
    let result: Result<_> = async {
        loop {
            if let Some(status) = running.child().try_wait()? {
                break Ok(status);
            }
            if let Some(generation) = &generation
                && crate::disk::dir_size(&generation.path) > GENERATION_BYTES
            {
                anyhow::bail!(
                    "managed incremental compiler exceeded 128 MiB; retained unproven output"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    .await;
    // Keep the generation owner through confirmation of the entire invocation's
    // descendants, including a non-exec wrapper's compiler and escaped tools.
    running.finish().await;
    let status = result?;
    if status.success()
        && let Some(generation) = generation
    {
        generation.seal(&worktree)?;
    }
    Ok(status.code().unwrap_or(1))
}

/// Validate independently of dep-info, with the caller holding owner, driver,
/// output and Cargo locks. Keep all generation/session locks through unlink.
#[derive(Default)]
pub(crate) struct Reclaim {
    pub(crate) files: Vec<PathBuf>,
    _locks: Vec<FileLease>,
    finished: Vec<(PathBuf, PathBuf, std::collections::BTreeSet<PathBuf>)>,
}

impl Reclaim {
    pub(crate) fn finish(&self) -> Result<()> {
        for (record, generation, allowed_dirs) in &self.finished {
            if generation.exists() {
                ensure!(
                    inventory(generation, false)?.is_empty(),
                    "incremental generation still contains data"
                );
                ensure!(
                    directory_inventory(generation)?.is_subset(allowed_dirs),
                    "unknown incremental directory appeared before removal"
                );
                let mut dirs = vec![generation.clone()];
                let mut index = 0;
                while index < dirs.len() {
                    for entry in fs::read_dir(&dirs[index])? {
                        let path = entry?.path();
                        ensure!(plain(&path)?.is_dir(), "incremental generation changed");
                        dirs.push(path);
                    }
                    index += 1;
                }
                for dir in dirs.into_iter().rev() {
                    fs::remove_dir(dir)?;
                }
            }
            let owner = record.with_extension("owner");
            ensure!(
                plain(&owner)?.len() == 0,
                "incremental owner changed before removal"
            );
            fs::remove_file(record)?;
            fs::remove_file(owner)?;
        }
        Ok(())
    }
}

fn validate_proof(worktree: &Path, record: &Path, proof: &Proof) -> Result<()> {
    ensure!(
        proof.version == 1 && proof.worktree == worktree,
        "incremental ownership changed"
    );
    let parent = proof
        .generation
        .parent()
        .context("missing generation parent")?;
    ensure!(
        [
            worktree.join("target/debug/incremental"),
            worktree.join("target/release/incremental")
        ]
        .contains(&parent.to_owned()),
        "incremental generation outside managed target"
    );
    ensure!(
        proof
            .generation
            .file_name()
            .is_some_and(|n| n.to_string_lossy()
                == format!("magi-{}", record.file_stem().unwrap().to_string_lossy())),
        "incremental generation identity changed"
    );
    Ok(())
}

pub(crate) fn candidates(worktree: &Path) -> Result<Reclaim> {
    let store = git_dir(worktree)?.join(STORE);
    if !store.exists() {
        return Ok(Reclaim::default());
    }
    ensure!(
        plain(&store)?.is_dir(),
        "invalid incremental provenance store"
    );
    let mut outputs = Vec::new();
    let mut locks = Vec::new();
    let mut finished = Vec::new();
    for entry in fs::read_dir(store)? {
        let record = entry?.path();
        if record.extension().is_none_or(|x| x != "json") {
            continue;
        }
        ensure!(
            plain(&record)?.is_file() && fs::metadata(&record)?.len() < 4 * 1024 * 1024,
            "invalid incremental provenance"
        );
        let proof: Proof = serde_json::from_slice(&fs::read(&record)?)?;
        validate_proof(worktree, &record, &proof)?;
        let lock_path = record.with_extension("owner");
        let owner_metadata = plain(&lock_path)?;
        ensure!(
            owner_metadata.is_file() && owner_metadata.len() == 0,
            "incremental owner is missing or changed"
        );
        let owner = FileLease(OpenOptions::new().read(true).write(true).open(lock_path)?);
        crate::pr_head::try_lock_file(&owner).context("incremental compiler is live")?;
        locks.push(owner);
        finished.push((
            record.clone(),
            proof.generation.clone(),
            proof.directories.clone(),
        ));
        if !proof.generation.exists() {
            continue;
        }
        let actual = inventory(&proof.generation, false)?;
        ensure!(
            directory_inventory(&proof.generation)?.is_subset(&proof.directories),
            "unknown incremental directory"
        );
        // Partial deletion can be retried. Added, replaced or modified files
        // refuse the entire generation, even when their signatures look valid.
        ensure!(
            actual.iter().all(|(p, id)| proof.files.get(p) == Some(id)),
            "incremental provenance mismatch or unknown data"
        );
        for relative in actual.keys() {
            let path = proof.generation.join(relative);
            if path.extension().is_some_and(|x| x == "lock") {
                let file = OpenOptions::new().read(true).write(true).open(&path)?;
                lock_session(&file).context("incremental session is live")?;
                locks.push(FileLease(file));
            }
            outputs.push(path);
        }
    }
    Ok(Reclaim {
        files: outputs,
        _locks: locks,
        finished,
    })
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    // A real, dependency-free Cargo crate keeps this regression below a few MiB.
    // No production paths or inherited Cargo configuration participate.
    fn cargo_generation() -> (tempfile::TempDir, PathBuf, Generation) {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().canonicalize().unwrap();
        let git = std::process::Command::new("git")
            .arg("init")
            .arg(&worktree)
            .output()
            .unwrap();
        assert!(git.status.success());
        let generation = generate(&worktree);
        (temp, worktree, generation)
    }

    pub(crate) fn generate(worktree: &Path) -> Generation {
        fs::create_dir(worktree.join("src")).unwrap();
        fs::write(
            worktree.join("Cargo.toml"),
            "[package]\nname='cache-fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
        )
        .unwrap();
        fs::write(
            worktree.join("src/lib.rs"),
            "pub fn answer() -> usize { (0..100).sum() }\n",
        )
        .unwrap();
        let parent = worktree.join("target/debug/incremental");
        fs::create_dir_all(&parent).unwrap();
        let generation = Generation::create(worktree, &parent).unwrap();
        let mut command = std::process::Command::new("cargo");
        command.env_clear();
        for key in ["PATH", "HOME", "RUSTUP_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = command
            .current_dir(worktree)
            .env("RUSTUP_TOOLCHAIN", "stable")
            .env("CARGO_HOME", worktree.join("cargo-home"))
            .env("CARGO_INCREMENTAL", "0")
            .env(
                "RUSTFLAGS",
                format!("-C incremental={}", generation.path.display()),
            )
            .args(["build", "--offline", "--jobs", "1", "--target-dir"])
            .arg(worktree.join("target"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(crate::disk::dir_size(&generation.path) > 0);
        generation.seal(worktree).unwrap();
        generation
    }

    #[test]
    fn admission_contention_waits_and_timeout_is_distinct_from_budget_exhaustion() {
        use std::time::Duration;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("admission.lock");
        let held = FileLease(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        );
        crate::pr_head::try_lock_file(&held).unwrap();
        std::thread::scope(|scope| {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let path = path.clone();
            scope.spawn(move || {
                let waiting = FileLease(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .unwrap(),
                );
                ready_tx.send(()).unwrap();
                let result = wait_for_admission(&waiting, Duration::from_secs(5));
                done_tx.send(result).unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(done_rx.recv_timeout(Duration::from_millis(40)).is_err());
            drop(held);
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
        });
        let held = FileLease(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        );
        crate::pr_head::try_lock_file(&held).unwrap();
        let waiting = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let error = wait_for_admission(&waiting, Duration::from_millis(20))
            .unwrap_err()
            .to_string();
        assert!(error.contains("wait timed out"));
        assert!(!error.contains("budget exhausted"));
    }

    #[test]
    fn completed_snapshots_share_owner_locks_without_blocking_parallel_admission() {
        let (_temp, worktree, generation) = cargo_generation();
        let owner = generation.record.with_extension("owner");
        drop(generation);
        let first = completed_charge(&worktree, &owner).unwrap();
        let second = completed_charge(&worktree, &owner).unwrap();
        assert!(first.0 > 0);
        assert_eq!(first.0, second.0);
        let exclusive = FileLease(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&owner)
                .unwrap(),
        );
        assert!(crate::pr_head::try_lock_file(&exclusive).is_err());
        let parent = worktree.join("target/debug/incremental");
        let fresh = Generation::create(&worktree, &parent).unwrap();
        assert!(fresh.path.is_dir());
        drop(first);
        drop(second);
        assert!(crate::pr_head::try_lock_file(&exclusive).is_ok());
    }

    #[test]
    fn ownership_release_is_not_delayed_by_descriptor_copies() {
        let (_temp, worktree, generation) = cargo_generation();
        let store = git_dir(&worktree).unwrap().join(STORE);
        let open = |path: &Path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap()
        };
        let admission_path = store.join("admission.lock");
        let admission = FileLease(open(&admission_path));
        crate::pr_head::try_lock_file(&admission).unwrap();
        // dup and fork share the same open-file description. Keep a copy open
        // deterministically, without depending on another test's fork timing.
        let inherited_admission = admission.try_clone().unwrap();
        let next = FileLease(open(&admission_path));
        assert!(crate::pr_head::try_lock_file(&next).is_err());
        drop(admission);
        crate::pr_head::try_lock_file(&next).unwrap();
        drop(next);
        drop(inherited_admission);

        let owner = generation.record.with_extension("owner");
        let inherited_generation = generation._lock.try_clone().unwrap();
        assert!(completed_charge(&worktree, &owner).is_err());
        drop(generation);
        let first = completed_charge(&worktree, &owner).unwrap();
        let second = completed_charge(&worktree, &owner).unwrap();
        let inherited_first = first.1.try_clone().unwrap();
        let inherited_second = second.1.try_clone().unwrap();
        let exclusive = FileLease(open(&owner));
        drop(first);
        assert!(
            crate::pr_head::try_lock_file(&exclusive).is_err(),
            "another actual accounting owner still excludes reclamation"
        );
        drop(second);
        crate::pr_head::try_lock_file(&exclusive).unwrap();
        drop(exclusive);
        drop((inherited_generation, inherited_first, inherited_second));

        let reclaim = candidates(&worktree).unwrap();
        let inherited_reclaim: Vec<_> = reclaim
            ._locks
            .iter()
            .map(|lock| lock.try_clone().unwrap())
            .collect();
        assert!(candidates(&worktree).is_err());
        drop(reclaim);
        let retry = candidates(&worktree).unwrap();
        assert!(!retry.files.is_empty());
        drop(retry);
        drop(inherited_reclaim);
    }

    #[test]
    fn default_disables_incremental_and_explicit_environment_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let mut command = tokio::process::Command::new("cargo");
        command.env_remove("CARGO_INCREMENTAL");
        configure(&mut command, temp.path()).unwrap();
        let env: BTreeMap<_, _> = command.as_std().get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("CARGO_INCREMENTAL")],
            Some(std::ffi::OsStr::new("0"))
        );
        command.env("CARGO_INCREMENTAL", "1");
        configure(&mut command, temp.path()).unwrap();
        let env: BTreeMap<_, _> = command.as_std().get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("CARGO_INCREMENTAL")],
            Some(std::ffi::OsStr::new("1"))
        );
    }

    #[test]
    fn wrapper_temporary_collision_preserves_the_other_publisher() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let occupied = root.join("occupied.tmp");
        let fresh = root.join("fresh.tmp");
        let script = root.join("wrapper.sh");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut owner = WrapperTemporary::create(|| occupied.clone()).unwrap();
                owner.file.write_all(b"other publisher").unwrap();
                barrier.wait();
                barrier.wait();
                // The competing publisher must not unlink our reserved path.
                assert_eq!(fs::read(&occupied).unwrap(), b"other publisher");
            });
            barrier.wait();
            let mut attempts = 0;
            let result = publish_wrapper(&script, "complete wrapper", || {
                attempts += 1;
                if attempts == 1 {
                    occupied.clone()
                } else {
                    fresh.clone()
                }
            });
            // Always release the owner, even if publication regresses.
            barrier.wait();
            result.unwrap();
            assert_eq!(attempts, 2);
            assert_eq!(fs::read(&script).unwrap(), b"complete wrapper");
            assert!(!fresh.exists());
        });
        assert!(!occupied.exists());
    }

    #[test]
    fn wrapper_temporary_retries_only_collisions_and_leaves_them_intact() {
        let temp = tempfile::tempdir().unwrap();
        let occupied = temp.path().join("occupied.tmp");
        fs::write(&occupied, b"reserved").unwrap();
        let mut attempts = 0;
        let error = WrapperTemporary::create(|| {
            attempts += 1;
            occupied.clone()
        })
        .err()
        .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(attempts, 128);
        assert_eq!(fs::read(&occupied).unwrap(), b"reserved");
        attempts = 0;
        let error = WrapperTemporary::create(|| {
            attempts += 1;
            temp.path().join("missing/temporary")
        })
        .err()
        .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn execution_wrapper_identity_rejects_changed_unknown_and_symlink_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (name, body) = wrapper_spec().unwrap();
        let script = root.join(name);
        fs::write(&script, &body).unwrap();
        assert!(current_wrapper(&script).unwrap());
        let unknown = root.join("operator-checkpoint");
        fs::write(&unknown, &body).unwrap();
        assert!(!current_wrapper(&unknown).unwrap());
        fs::write(&script, "changed").unwrap();
        assert!(!current_wrapper(&script).unwrap());
        fs::remove_file(&script).unwrap();
        std::os::unix::fs::symlink(&unknown, &script).unwrap();
        assert!(current_wrapper(&script).is_err());
    }

    #[test]
    fn wrapper_publication_preserves_existing_destinations() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let script = root.join("wrapper.sh");
        let temporary = root.join("temporary");
        let body = "#!/bin/sh\nexit 0\n";
        fs::write(&script, body).unwrap();
        let inode = fs::metadata(&script).unwrap().ino();
        publish_wrapper(&script, body, || temporary.clone()).unwrap();
        assert_eq!(fs::metadata(&script).unwrap().ino(), inode);
        assert!(!temporary.exists());
        fs::write(&script, "different").unwrap();
        assert!(publish_wrapper(&script, body, || temporary.clone()).is_err());
        assert_eq!(fs::read_to_string(&script).unwrap(), "different");
        assert!(!temporary.exists());
        fs::remove_file(&script).unwrap();
        fs::create_dir(&script).unwrap();
        assert!(publish_wrapper(&script, body, || temporary.clone()).is_err());
        assert!(script.is_dir());
        assert!(!temporary.exists());
        fs::remove_dir(&script).unwrap();
        let target = root.join("target");
        fs::write(&target, body).unwrap();
        symlink(&target, &script).unwrap();
        assert!(publish_wrapper(&script, body, || temporary.clone()).is_err());
        assert_eq!(fs::read_link(&script).unwrap(), target);
        assert_eq!(fs::read_to_string(&target).unwrap(), body);
        assert!(!temporary.exists());
    }

    #[test]
    fn concurrent_configuration_publishes_complete_wrappers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let worktree = root.join("worktree");
        assert!(
            std::process::Command::new("git")
                .args(["init", "--separate-git-dir"])
                .arg(root.join("metadata"))
                .arg(&worktree)
                .output()
                .unwrap()
                .status
                .success()
        );
        use std::os::unix::fs::PermissionsExt;
        let executable = std::env::current_exe().unwrap();
        let quoted = executable.to_string_lossy().replace('\'', "'\\''");
        let body = format!("#!/bin/sh\nexec '{quoted}' __magi_incremental \"$@\"\n");
        let barrier = std::sync::Barrier::new(8);
        let wrappers = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let mut last = None;
                        barrier.wait();
                        for _ in 0..16 {
                            let mut command = tokio::process::Command::new("cargo");
                            configure(&mut command, &worktree).unwrap();
                            let env: BTreeMap<_, _> = command.as_std().get_envs().collect();
                            let wrapper =
                                PathBuf::from(env[std::ffi::OsStr::new("RUSTC_WRAPPER")].unwrap());
                            assert_eq!(fs::read_to_string(&wrapper).unwrap(), body);
                            assert_eq!(
                                fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
                                0o700
                            );
                            if let Some(previous) = last.replace(wrapper.clone()) {
                                assert_eq!(previous, wrapper);
                            }
                        }
                        last.unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(wrappers.iter().all(|path| path == &wrappers[0]));
        assert!(
            fs::read_dir(git_dir(&worktree).unwrap())
                .unwrap()
                .all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".tmp")
                })
        );
    }

    #[test]
    fn sealing_refuses_unfinished_sessions_and_unknown_directories() {
        let (_temp, worktree, generation) = cargo_generation();
        let unknown = generation.path.join("operator-empty");
        fs::create_dir(&unknown).unwrap();
        assert!(generation.seal(&worktree).is_err());
        fs::remove_dir(unknown).unwrap();
        let crate_dir = fs::read_dir(&generation.path)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let working = crate_dir.join("s-unfinished-cache-working");
        fs::create_dir(&working).unwrap();
        assert!(generation.seal(&worktree).is_err());
        assert!(working.exists());
        drop(generation);
        assert!(candidates(&worktree).is_err());
    }

    #[test]
    fn unknown_empty_directories_added_after_inventory_are_retained() {
        let (_temp, worktree, generation) = cargo_generation();
        let path = generation.path.clone();
        drop(generation);
        let reclaim = candidates(&worktree).unwrap();
        for file in &reclaim.files {
            fs::remove_file(file).unwrap();
        }
        let unknown = path.join("operator-empty");
        fs::create_dir(&unknown).unwrap();
        assert!(reclaim.finish().is_err());
        assert!(unknown.is_dir());
        fs::remove_dir(unknown).unwrap();
        reclaim.finish().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn fcntl_session_lock_is_respected_across_processes() {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};
        if let Some(path) = std::env::var_os("MAGI_TEST_SESSION_FILE") {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            // Deliberately do not take flock: both compiler protocols must
            // exclude reclamation, even on hosts where they are independent.
            lock_posix_session(&file).unwrap();
            fs::write(
                std::env::var_os("MAGI_TEST_SESSION_READY").unwrap(),
                "ready",
            )
            .unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
            return;
        }
        let (_temp, worktree, generation) = cargo_generation();
        drop(generation);
        let candidates = candidates(&worktree).unwrap();
        let session = candidates
            .files
            .iter()
            .find(|p| p.extension().is_some_and(|x| x == "lock"))
            .unwrap()
            .clone();
        drop(candidates);
        let ready = worktree.join("session-ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "incremental::tests::fcntl_session_lock_is_respected_across_processes",
            ])
            .env("MAGI_TEST_SESSION_FILE", &session)
            .env("MAGI_TEST_SESSION_READY", &ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        while !ready.exists() && started.elapsed() < std::time::Duration::from_secs(10) {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let observed = ready.exists();
        let blocked = observed
            && super::candidates(&worktree)
                .err()
                .is_some_and(|e| e.to_string().contains("session is live"));
        if observed {
            child.stdin.take().unwrap().write_all(b"x").unwrap();
        } else {
            child.kill().ok();
        }
        let status = child.wait().unwrap();
        assert!(
            observed && status.success(),
            "session helper did not acquire its lock"
        );
        assert!(blocked, "POSIX session lock must exclude reclamation");
        assert!(!super::candidates(&worktree).unwrap().files.is_empty());
        assert!(session.exists());
    }

    #[test]
    fn real_cargo_generations_require_exact_provenance_and_live_owner_exclusion() {
        let (_temp, worktree, generation) = cargo_generation();
        assert!(
            candidates(&worktree).is_err(),
            "compiler owner still holds the generation"
        );
        let path = generation.path.clone();
        drop(generation);
        let reclaim = candidates(&worktree).unwrap();
        let session = reclaim
            .files
            .iter()
            .find(|p| p.extension().is_some_and(|x| x == "lock"))
            .unwrap()
            .clone();
        drop(reclaim);
        let live_session = FileLease(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(session)
                .unwrap(),
        );
        crate::pr_head::try_lock_file(&live_session).unwrap();
        assert!(
            candidates(&worktree)
                .err()
                .unwrap()
                .to_string()
                .contains("session is live")
        );
        drop(live_session);
        let unknown = path.join("operator-note");
        fs::write(&unknown, "keep unknown data").unwrap();
        assert!(candidates(&worktree).is_err());
        assert_eq!(fs::read_to_string(&unknown).unwrap(), "keep unknown data");
        fs::remove_file(&unknown).unwrap();
        let unknown_dir = path.join("operator-empty");
        fs::create_dir(&unknown_dir).unwrap();
        assert!(candidates(&worktree).is_err());
        fs::remove_dir(unknown_dir).unwrap();
        let reclaim = candidates(&worktree).unwrap();
        assert!(!reclaim.files.is_empty());
        assert!(
            candidates(&worktree).is_err(),
            "maintenance retains session/owner locks"
        );
        // Partial deletion can resume, but changed bytes cannot be adopted.
        let removed = reclaim.files[0].clone();
        fs::remove_file(removed).unwrap();
        drop(reclaim);
        let reclaim = candidates(&worktree).unwrap();
        let changed = reclaim
            .files
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "bin"))
            .unwrap()
            .clone();
        let original = fs::read(&changed).unwrap();
        fs::write(&changed, b"modified proof input").unwrap();
        drop(reclaim);
        assert!(candidates(&worktree).is_err());
        fs::write(&changed, original).unwrap();
        let reclaim = candidates(&worktree).unwrap();
        for file in &reclaim.files {
            fs::remove_file(file).unwrap();
        }
        reclaim.finish().unwrap();
        assert!(!path.exists());
        assert!(candidates(&worktree).unwrap().files.is_empty());
        assert!(worktree.join("src/lib.rs").is_file());
    }

    #[test]
    fn externally_removed_sealed_generation_does_not_strand_admission() {
        let (_temp, worktree, generation) = cargo_generation();
        let path = generation.path.clone();
        drop(generation);
        let reclaim = candidates(&worktree).unwrap();
        for file in &reclaim.files {
            fs::remove_file(file).unwrap();
        }
        // Simulate a clean/reset removing only empty cache directories, while
        // leaving the completion proof in the worktree Git directory.
        let mut dirs = directory_inventory(&path)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>();
        dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for relative in dirs {
            fs::remove_dir(path.join(relative)).unwrap();
        }
        fs::remove_dir(&path).unwrap();
        drop(reclaim);
        let parent = worktree.join("target/debug/incremental");
        let fresh = Generation::create(&worktree, &parent).unwrap();
        assert_eq!(
            fs::read_dir(git_dir(&worktree).unwrap().join(STORE))
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "owner"))
                .count(),
            1
        );
        assert!(fresh.path.is_dir());
    }

    #[test]
    fn unsealed_attempts_consume_the_bounded_generation_budget() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().canonicalize().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg(&worktree)
                .output()
                .unwrap()
                .status
                .success()
        );
        let parent = worktree.join("target/debug/incremental");
        fs::create_dir_all(&parent).unwrap();
        for _ in 0..SLOTS {
            let generation = Generation::create(&worktree, &parent).unwrap();
            fs::write(generation.path.join("unknown"), "preserve").unwrap();
        }
        assert!(Generation::create(&worktree, &parent).is_err());
        assert!(
            candidates(&worktree).unwrap().files.is_empty(),
            "unsealed output never grants deletion authority"
        );
    }

    #[cfg(unix)]
    #[test]
    fn generation_symlinks_and_replaced_inodes_refuse_reclamation() {
        let (_temp, worktree, generation) = cargo_generation();
        let path = generation.path.clone();
        drop(generation);
        let files = candidates(&worktree).unwrap().files;
        let file = files
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "bin"))
            .unwrap();
        let saved = worktree.join("retained-input");
        fs::rename(file, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, file).unwrap();
        assert!(candidates(&worktree).is_err());
        fs::remove_file(file).unwrap();
        fs::copy(&saved, file).unwrap();
        assert!(
            candidates(&worktree).is_err(),
            "same bytes at a replaced inode are insufficient"
        );
        assert!(path.exists());
        assert!(saved.is_file());
    }
}
