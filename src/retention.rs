//! Bounded reclamation of retained attempts and Cargo outputs. Source recovery
//! and build eviction have separate authority: evicting a target never retires a run.
mod copies;
pub mod reconcile;

pub(crate) fn has_archived_seats(home: &Path, state: &RunState) -> Result<bool> {
    let artifacts = home.join("runs").join(&state.id).join("artifacts");
    if !artifacts.try_exists()? {
        return Ok(false);
    }
    ensure!(
        plain_dir(&artifacts) && artifacts.canonicalize()? == artifacts,
        "noncanonical artifacts"
    );
    for (i, entry) in std::fs::read_dir(artifacts)?.enumerate() {
        ensure!(i < 4096, "eviction record inspection limit exceeded");
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("eviction-") && name.ends_with(".json") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Re-materialize evicted read seats before execution can consume their paths.
/// Caller owns the run's driver/owner leases and repository coordination.
pub(crate) async fn restore_execution_seats(home: &Path, state: &RunState) -> Result<()> {
    copies::restore_execution_seats(home, state).await
}

/// Read the binary diff after verifying its independent source archive.
pub async fn source_diff(evidence: &Path) -> Result<Vec<u8>> {
    crate::fold_evidence::validate_snapshot(evidence).await?;
    Ok(std::fs::read(evidence.join("change.patch"))?)
}

/// Restore immutable checkout evidence into a new inspection repository.
pub async fn restore_source(evidence: &Path, destination: &Path) -> Result<()> {
    crate::fold_evidence::restore_snapshot(evidence, destination).await
}

/// Historical scan observations, independent of archive readability or deletion authority.
pub fn seat_inspections(home: &Path, id: &str) -> Result<Vec<String>> {
    ensure!(crate::run::is_run_id(id), "invalid run id");
    copies::seat_inspections(home, id)
}

/// List archived checkout tips for a run, validating their durable evidence.
pub async fn seat_evidence(home: &Path, id: &str) -> Result<Vec<String>> {
    copies::seat_evidence(home, id).await
}

/// Validated archived heads and seat labels for the run's evidence browser.
pub async fn seat_heads(
    home: &Path,
    id: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    copies::seat_heads(home, id).await
}

/// Read a recorded seat's diff without relying on its checkout or Git objects.
pub async fn seat_diff(home: &Path, id: &str, head: &str) -> Result<Vec<u8>> {
    copies::seat_diff(home, id, head).await
}

/// Restore a recorded checkout tip without changing run or recovery authority.
pub async fn restore_seat(home: &Path, id: &str, head: &str, destination: &Path) -> Result<()> {
    copies::restore_seat(home, id, head, destination).await
}

use crate::{
    git,
    proc::Quiet as _,
    run::{Liveness, RunState},
};
use anyhow::{Context, Result, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

/// An OS lease on a worktree's outputs, independent of repository coordination.
/// Kept by the process cleanup owner until descendants terminate, including
/// reparented tools retaining the inherited liveness witness.
#[derive(Debug)]
pub(crate) struct OutputLease {
    _file: std::sync::Arc<OutputFile>,
    _activity: bool,
}

#[derive(Debug)]
struct OutputFile(File);

impl std::ops::Deref for OutputFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for OutputFile {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the final shared owner releases this live descriptor's lock.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl OutputLease {
    #[cfg(unix)]
    pub(crate) fn descriptor(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self._file.as_raw_fd()
    }

    /// A separate exclusive lock prevents nested/concurrent pre-clean operations.
    pub(crate) fn pre_clean(dir: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("magi-pre-clean.lock"))?;
        crate::pr_head::try_lock_file(&file)
            .context("pre-clean already running; retry after descendants terminate")?;
        Ok(Self {
            _file: std::sync::Arc::new(OutputFile(file)),
            _activity: false,
        })
    }

    pub(crate) fn duplicate(&self) -> Self {
        Self {
            _file: std::sync::Arc::clone(&self._file),
            _activity: false,
        }
    }

    pub(crate) fn borrow(worktree: &Path) -> Result<Option<Self>> {
        if !worktree.join(".git").is_file() {
            return Ok(None);
        }
        Self::take(worktree, false).map(Some)
    }
    pub(crate) fn cache(home: &Path, cache: &Path, exclusive: bool) -> Result<Self> {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let mut normalized = cache.to_owned();
        let mut missing = Vec::new();
        while !normalized.exists() {
            let Some(name) = normalized.file_name() else {
                break;
            };
            missing.push(name.to_owned());
            if !normalized.pop() {
                break;
            }
        }
        normalized = normalized.canonicalize().unwrap_or(normalized);
        for part in missing.into_iter().rev() {
            normalized.push(part);
        }
        normalized.hash(&mut hash);
        let dir = home.join("cache-leases");
        std::fs::create_dir_all(&dir)?;
        Self::lock(
            &dir.join(format!("outputs-{:x}.lock", hash.finish())),
            exclusive,
        )
    }
    /// Reserve headroom once per volume while admitting a managed writer.
    /// These are estimates, not filesystem quotas. The ordinary emergency
    /// monitor remains necessary for writes exceeding the reservation.
    #[cfg(unix)]
    pub(crate) fn admit(home: &Path, paths: &[PathBuf], min_free: u64) -> Result<Vec<Self>> {
        Self::admit_with(home, paths, min_free, crate::disk::free_bytes)
    }

    #[cfg(unix)]
    pub(crate) fn admit_with(
        home: &Path,
        paths: &[PathBuf],
        min_free: u64,
        free_bytes: impl Fn(&Path) -> Result<u64>,
    ) -> Result<Vec<Self>> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if min_free == 0 {
            return Ok(Vec::new());
        }
        const RESERVE: u64 = 1024 * 1024 * 1024;
        std::fs::create_dir_all(home)?;
        let dir = home.canonicalize()?.join("disk-admission");
        std::fs::create_dir_all(&dir)?;
        ensure!(
            plain_dir(&dir) && dir.canonicalize()? == dir,
            "disk admission directory is not canonical"
        );
        let mut devices = std::collections::BTreeSet::new();
        let mut reservations = Vec::new();
        for path in paths {
            let probe = path
                .ancestors()
                .find(|p| p.exists())
                .context("disk admission path missing")?;
            let device = std::fs::metadata(probe)?.dev();
            if !devices.insert(device) {
                continue;
            }
            let open = |path: &Path| -> Result<File> {
                Ok(OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)?)
            };
            // Explicitly unlock on return. A concurrent fork can briefly
            // inherit this descriptor before exec closes it; closing only the
            // parent's copy would leave the namespace spuriously busy.
            let admission = Self {
                _file: std::sync::Arc::new(OutputFile(open(&dir.join(format!("{device}.lock")))?)),
                _activity: false,
            };
            let started = std::time::Instant::now();
            loop {
                match crate::pr_head::try_lock_file(&admission._file) {
                    Ok(()) => break,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) && started.elapsed() < std::time::Duration::from_secs(1) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => {
                        return Err(error)
                            .context("disk admission busy or unavailable; retry without dispatch");
                    }
                }
            }
            let mut active = 0u64;
            let mut available = None;
            for slot in 0..1024 {
                let file = dir.join(format!("{device}-{slot}.reserve"));
                if !file.exists() {
                    if available.is_none() {
                        available = Some(file);
                    }
                    continue;
                }
                let file = open(&file)?;
                match crate::pr_head::try_lock_file(&file) {
                    Ok(()) if available.is_none() => {
                        available = Some(dir.join(format!("{device}-{slot}.reserve")))
                    }
                    Ok(()) => {}
                    Err(_) => active = active.saturating_add(RESERVE),
                }
            }
            let free = free_bytes(probe)?;
            let required = min_free.saturating_add(active).saturating_add(RESERVE);
            ensure!(
                free >= required,
                "disk admission blocked: {free} bytes free; floor {min_free}, active writer reservations {active}, next writer reservation {RESERVE} bytes (estimates, not quotas)"
            );
            let file = open(&available.context("disk admission reservation slots exhausted")?)?;
            crate::pr_head::try_lock_file(&file)?;
            reservations.push(Self {
                _file: std::sync::Arc::new(OutputFile(file)),
                _activity: false,
            });
        }
        Ok(reservations)
    }

    #[cfg(not(unix))]
    pub(crate) fn admit(_home: &Path, _paths: &[PathBuf], _min_free: u64) -> Result<Vec<Self>> {
        // Registered seats already fail closed without Unix containment.
        Ok(Vec::new())
    }

    pub(crate) fn take(worktree: &Path, exclusive: bool) -> Result<Self> {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(worktree)
            .quiet()
            .output()?;
        ensure!(
            out.status.success(),
            "cannot identify output lease directory"
        );
        let dir = PathBuf::from(String::from_utf8(out.stdout)?.trim());
        Self::lock(&dir.join("magi-output.lock"), exclusive)
    }
    fn lock(path: &Path, exclusive: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("output lease: open {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let mode = if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            };
            // SAFETY: live file descriptor, no retained pointers.
            ensure!(
                unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0,
                "output lease: outputs are in use or being reclaimed: {}",
                path.display()
            );
        }
        #[cfg(not(unix))]
        let _ = exclusive;
        if !exclusive {
            // Empty legacy/maintenance-only leases carry no activity witness.
            file.set_len(1)?;
            file.set_modified(std::time::SystemTime::now())
                .with_context(|| format!("record output activity {}", path.display()))?;
        }
        Ok(Self {
            _file: std::sync::Arc::new(OutputFile(file)),
            _activity: !exclusive,
        })
    }
}
#[cfg(unix)]
impl Drop for OutputLease {
    fn drop(&mut self) {
        if self._activity
            && let Err(error) = self._file.set_modified(std::time::SystemTime::now())
        {
            // If release time cannot be recorded, retain a nonempty invalid
            // witness rather than trusting the old acquisition timestamp.
            let _ = self._file.set_len(2);
            tracing::warn!("could not record output lease release activity: {error}");
        }
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Report {
    pub checked_at: Option<Timestamp>,
    pub limit_bytes: u64,
    pub reclaim_budget_bytes: u64,
    /// Logical inventory, including protected and unproven data.
    pub measured_bytes: u64,
    /// Logical bytes unlinked, not a measurement of physical disk recovery.
    pub freed_bytes: u64,
    pub measured_allocated_file_bytes: Option<u64>,
    /// Candidates proven under locks in this pass; uninspected data is excluded.
    pub proven_reclaimable_bytes: u64,
    /// Estimated allocated blocks recoverable if every proven link is removed.
    /// Inodes with links outside the candidate set contribute zero.
    pub proven_reclaimable_allocated_bytes: Option<u64>,
    pub protected_or_unproven_bytes: u64,
    /// Deduplicated allocated file blocks whose final link was unlinked. Open
    /// descriptors and shared extents can prevent an equivalent physical gain.
    pub estimated_released_allocated_bytes: Option<u64>,
    /// Observed free-space change on the home volume; concurrent writes, clones
    /// and deferred reclamation mean this is not attributable to this sweep.
    pub observed_home_free_change_bytes: Option<i64>,
    pub retired: usize,
    pub evicted_copies: usize,
    pub source_scan: Option<serde_json::Value>,
    pub removed_empty_roots: usize,
    pub source_unlinked_logical_bytes: u64,
    pub source_unlinked_allocated_bytes: Option<u64>,
    pub source_evidence_added_logical_bytes: u64,
    pub source_evidence_added_allocated_bytes: Option<u64>,
    pub retained: Vec<String>,
    pub failures: Vec<String>,
}

#[cfg(unix)]
fn allocated_inventory(
    root: &Path,
    seen: &mut std::collections::BTreeSet<(u64, u64)>,
) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        plain_dir(root) && root.canonicalize()? == root,
        "allocated inventory root is not canonical"
    );
    let mut dirs = vec![root.to_owned()];
    let mut bytes = 0u64;
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                dirs.push(path);
            } else if metadata.is_file() && seen.insert((metadata.dev(), metadata.ino())) {
                bytes = bytes.saturating_add(metadata.blocks().saturating_mul(512));
            }
        }
    }
    Ok(bytes)
}

#[derive(Default)]
struct ReleasedAllocation {
    #[cfg(unix)]
    files: std::collections::BTreeMap<PathBuf, (u64, u64, u64, u64)>,
    #[cfg(unix)]
    removed: std::collections::BTreeMap<(u64, u64), u64>,
}

impl ReleasedAllocation {
    fn of(paths: &[PathBuf]) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mut files = std::collections::BTreeMap::new();
            for path in paths {
                let m = std::fs::symlink_metadata(path)?;
                ensure!(m.is_file(), "allocation candidate is not regular");
                files.insert(
                    path.clone(),
                    (m.dev(), m.ino(), m.nlink(), m.blocks().saturating_mul(512)),
                );
            }
            Ok(Self {
                files,
                removed: Default::default(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = paths;
            Ok(Self::default())
        }
    }
    fn reclaimable_allocated_bytes(&self) -> Option<u64> {
        #[cfg(unix)]
        {
            let mut inodes = std::collections::BTreeMap::new();
            for &(device, inode, links, blocks) in self.files.values() {
                let entry = inodes
                    .entry((device, inode))
                    .or_insert((0u64, links, blocks));
                entry.0 += 1;
            }
            Some(
                inodes
                    .values()
                    .filter(|(count, links, _)| count == links)
                    .fold(0u64, |sum, (_, _, blocks)| sum.saturating_add(*blocks)),
            )
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    fn unlinked(&mut self, path: &Path) -> Option<u64> {
        #[cfg(unix)]
        {
            let (device, inode, links, blocks) = self.files[path];
            let removed = self.removed.entry((device, inode)).or_default();
            *removed += 1;
            Some(if *removed == links { blocks } else { 0 })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }
}

fn plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

/// Old run records outlive their checkout and output directories. Only an
/// absent path means nothing to reclaim; inaccessible paths remain errors.
fn run_paths_missing(state: &RunState) -> Result<bool> {
    for path in [state.worktree_root(), state.repo.clone()] {
        match std::fs::metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
        }
    }
    Ok(false)
}

/// Enumerate actual registered worktrees, not just candidates in run.json.
pub(crate) async fn worktrees(state: &RunState) -> Result<Vec<PathBuf>> {
    let root = state.worktree_root();
    if !plain_dir(&root) {
        return Ok(Vec::new());
    }
    let list = git::git(&state.repo, &["worktree", "list", "--porcelain"]).await?;
    registered_worktrees(state, &list)
}

fn registered_worktrees(state: &RunState, list: &str) -> Result<Vec<PathBuf>> {
    let root = state.worktree_root();
    if !plain_dir(&root) {
        return Ok(Vec::new());
    }
    let root = root.canonicalize()?;
    let mut paths = Vec::new();
    for path in list.lines().filter_map(|l| l.strip_prefix("worktree ")) {
        let path = PathBuf::from(path);
        #[cfg(windows)]
        let path = if plain_dir(&path) {
            git::canonical_output_path(path.to_str().context("non UTF-8 registration")?)?
        } else {
            continue;
        };
        if plain_dir(&path)
            && path.parent() == Some(root.as_path())
            && path.canonicalize()? == path
            && path.join(".git").is_file()
        {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// Recognize Cargo's generated directory, never merely an ignored directory.
/// Unknown root entries are retained; cache identity/profile paths fail closed.
async fn cargo_target(worktree: &Path) -> Result<PathBuf> {
    validate_cargo_target(worktree, false).await
}

async fn validate_cargo_target(worktree: &Path, allow_missing_tag: bool) -> Result<PathBuf> {
    let target = worktree.join("target");
    ensure!(plain_dir(&target), "target absent or not a plain directory");
    ensure!(target.canonicalize()? == target, "target path changed");
    ensure!(
        git::git(worktree, &["ls-files", "--", "target"])
            .await?
            .is_empty(),
        "tracked target contents"
    );
    ensure!(
        git::git_raw(worktree, &["check-ignore", "--quiet", "target/"])
            .await?
            .ok(),
        "target is not ignored"
    );
    cache_tag(&target, allow_missing_tag)?;
    let path = target.join(".rustc_info.json");
    plain_file(&path, "rustc identity")?;
    let info: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("read rustc identity {}", path.display()))?,
    )
    .with_context(|| format!("parse rustc identity {}", path.display()))?;
    ensure!(
        info.get("rustc_fingerprint").is_some(),
        "unknown Cargo identity {}",
        path.display()
    );
    if allow_missing_tag && !target.join("CACHEDIR.TAG").exists() {
        ensure!(
            info.get("rustc_fingerprint")
                .is_some_and(serde_json::Value::is_u64)
                && info
                    .get("outputs")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|outputs| outputs.values().any(|output| {
                        output.get("success").and_then(serde_json::Value::as_bool) == Some(true)
                            && output
                                .get("stdout")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|stdout| stdout.starts_with("rustc "))
                            && output
                                .get("stderr")
                                .is_some_and(serde_json::Value::is_string)
                    }))
                && info
                    .get("successes")
                    .is_some_and(serde_json::Value::is_object),
            "missing-tag rustc identity structure invalid: {}",
            path.display()
        );
        let mut profiles = 0;
        for profile in ["debug", "release"] {
            let dir = target.join(profile);
            if !dir.exists() {
                continue;
            }
            profile_output_dirs(&dir, true)?;
            plain_file(&dir.join(".cargo-lock"), "missing-tag Cargo lock")?;
            profiles += 1;
        }
        ensure!(
            profiles > 0,
            "missing-tag proof: no Cargo profile in {}",
            target.display()
        );
    }
    for name in ["debug", "release"] {
        let path = target.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && path.canonicalize()? == path,
                "profile validation: not a plain directory: {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("profile validation: inspect {}", path.display()));
            }
        }
    }
    Ok(target)
}

fn plain_file(path: &Path, stage: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("{stage}: inspect {}", path.display()))?;
    ensure!(
        meta.is_file() && path.canonicalize()? == path,
        "{stage}: not a plain file: {}",
        path.display()
    );
    Ok(())
}

fn cache_tag(target: &Path, allow_missing: bool) -> Result<()> {
    let path = target.join("CACHEDIR.TAG");
    match std::fs::symlink_metadata(&path) {
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        result => {
            result.with_context(|| format!("cache tag: inspect {}", path.display()))?;
        }
    }
    plain_file(&path, "cache tag")?;
    let tag = std::fs::read_to_string(&path)
        .with_context(|| format!("cache tag: read {}", path.display()))?;
    ensure!(
        tag.starts_with("Signature: 8a477f597d28d172789f06886806bc55"),
        "unknown Cargo cache tag: {}",
        path.display()
    );
    Ok(())
}

/// Observe compiler metadata and artifacts under the output and Cargo locks.
/// Unknown contents never become deletion authority. Unreadable timestamps or
/// a future timestamp retain outputs, even when the configured grace is zero.
fn output_age(target: &Path, now: Timestamp, grace: u64) -> Result<()> {
    let mut dirs = vec![target.to_owned()];
    let mut entries = 0;
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("output activity: list {}", dir.display()))?
        {
            let path = entry?.path();
            entries += 1;
            ensure!(
                entries <= 100_000,
                "output activity inventory too large: {}",
                target.display()
            );
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("output activity: inspect {}", path.display()))?;
            if metadata.is_dir() {
                // Only supported Cargo profiles supply execution evidence.
                if dir != target
                    || matches!(
                        path.file_name().and_then(|s| s.to_str()),
                        Some("debug" | "release")
                    )
                {
                    dirs.push(path);
                }
            } else if metadata.is_file()
                && (dir != target || path.file_name().is_some_and(|n| n == ".rustc_info.json"))
            {
                let at =
                    Timestamp::try_from(metadata.modified().with_context(|| {
                        format!("output activity: modified {}", path.display())
                    })?)?;
                ensure!(
                    crate::clean::due(now, at, grace),
                    "output activity within grace period: {} modified {at} (now {now}, grace {grace}s)",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn artifact_signature(path: &Path) -> Result<bool> {
    use std::io::Read as _;
    let mut header = [0; 8];
    let n = File::open(path)?.read(&mut header)?;
    let header = &header[..n];
    Ok(header.starts_with(b"\x7fELF")
        || header.starts_with(b"!<arch>\n")
        || header.starts_with(b"rust\0\0\0")
        || header.starts_with(b"MZ\x90\0")
        || [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xca, 0xfe, 0xba, 0xbf],
        ]
        .iter()
        .any(|magic| header.starts_with(magic)))
}

/// Enumerate only known Cargo output locations. A v2 unit's `out` is not
/// wholesale compiler authority: callers must still prove each file by dep-info
/// and signature. In particular, build-script output is never traversed.
fn profile_output_dirs(profile: &Path, require_proof: bool) -> Result<Vec<PathBuf>> {
    let mut present = std::collections::BTreeSet::new();
    for path in [
        profile.to_owned(),
        profile.join("deps"),
        profile.join(".fingerprint"),
        profile.join("build"),
    ] {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                ensure!(
                    plain_dir(&path) && path.canonicalize()? == path,
                    "Cargo profile proof: {} is not a plain directory",
                    path.display()
                );
                present.insert(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if !present.contains(profile) {
        return Ok(Vec::new());
    }
    let deps = present.contains(&profile.join("deps"));
    let fingerprint = present.contains(&profile.join(".fingerprint"));
    let mut dirs = vec![profile.to_owned()];
    if deps || fingerprint {
        ensure!(
            !require_proof || (deps && fingerprint),
            "incomplete Cargo v1 profile proof"
        );
        if deps {
            dirs.push(profile.join("deps"));
        }
        // v1 build-script directories have flat <package>-<hash> names;
        // a nested v2 package directory must not silently mix layouts.
        if present.contains(&profile.join("build")) {
            for entry in std::fs::read_dir(profile.join("build"))? {
                let path = entry?.path();
                if plain_dir(&path) {
                    for entry in std::fs::read_dir(&path)? {
                        ensure!(
                            !entry?.file_name().to_str().is_some_and(unit_hash),
                            "mixed Cargo profile layouts"
                        );
                    }
                }
            }
        }
    } else if present.contains(&profile.join("build")) {
        let mut units = 0;
        for package in std::fs::read_dir(profile.join("build"))? {
            let package = package?.path();
            let meta = std::fs::symlink_metadata(&package)?;
            ensure!(
                !meta.file_type().is_symlink(),
                "Cargo v2 package is a symlink: {}",
                package.display()
            );
            if !meta.is_dir() {
                ensure!(!require_proof, "Cargo v2 package is not a directory");
                continue;
            }
            ensure!(
                package.canonicalize()? == package,
                "Cargo v2 package is not a plain directory"
            );
            let mut package_units = 0;
            for unit in std::fs::read_dir(&package)? {
                let unit = unit?.path();
                let meta = std::fs::symlink_metadata(&unit)?;
                ensure!(
                    !meta.file_type().is_symlink(),
                    "Cargo v2 unit is a symlink: {}",
                    unit.display()
                );
                // Reclamation only: a non-hash or incomplete unit is not a
                // rustc unit, so it is retained while complete ones still
                // count. The missing-tag proof demands every unit be valid.
                let structured = meta.is_dir()
                    && unit
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(unit_hash)
                    && unit.canonicalize()? == unit
                    && ["fingerprint", "out"].iter().all(|name| {
                        let path = unit.join(name);
                        plain_dir(&path) && path.canonicalize().is_ok_and(|p| p == path)
                    });
                // A completed build-script run unit has no invoked.timestamp;
                // it is a known structure but its output never gets authority.
                if structured && v2_run_script_unit(&unit) {
                    continue;
                }
                let complete = structured
                    && plain_file(
                        &unit.join("fingerprint/invoked.timestamp"),
                        "Cargo unit fingerprint",
                    )
                    .is_ok();
                if !complete {
                    ensure!(!require_proof, "invalid or incomplete Cargo v2 unit");
                    continue;
                }
                dirs.push(unit.join("out"));
                units += 1;
                package_units += 1;
                ensure!(units <= 4096, "Cargo unit inventory too large");
            }
            ensure!(
                package_units > 0 || !require_proof,
                "empty Cargo v2 package"
            );
        }
        ensure!(units > 0 || !require_proof, "missing Cargo v2 units");
    } else {
        ensure!(!require_proof, "missing Cargo profile proof");
    }
    Ok(dirs)
}

// A build-script run unit: fingerprint holds exactly a plain
// `run-build-script-*` file with its json sibling, and no timestamp.
fn v2_run_script_unit(unit: &Path) -> bool {
    let fp = unit.join("fingerprint");
    if std::fs::symlink_metadata(fp.join("invoked.timestamp")).is_ok() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&fp) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with("run-build-script-")
            && path.extension().is_none()
            && plain_file(&path, "Cargo run fingerprint").is_ok()
            && plain_file(&path.with_extension("json"), "Cargo run fingerprint json").is_ok()
        {
            return true;
        }
    }
    false
}

// A unit directory may also contain arbitrary build-script products. Only
// lib/bin rustc fingerprints matching this dep-info stem confer output authority.
fn v2_compiler_dep_info(dir: &Path, dep_info: &Path) -> Result<bool> {
    let unit = dir.parent().context("Cargo unit parent")?;
    let hash = unit.file_name().unwrap_or_default().to_string_lossy();
    let stem = dep_info.file_stem().unwrap_or_default().to_string_lossy();
    for entry in std::fs::read_dir(unit.join("fingerprint"))? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // Cargo hashes library filenames but v2 binaries use the crate name
        // directly; the containing unit directory supplies their disambiguation.
        let library = name
            .strip_prefix("lib-")
            .is_some_and(|name| stem == format!("{}-{hash}", name.replace('-', "_")));
        let binary = name
            .strip_prefix("bin-")
            .is_some_and(|name| stem == name.replace('-', "_"));
        if library || binary {
            plain_file(&path, "Cargo compiler fingerprint")?;
            plain_file(
                &path.with_extension("json"),
                "Cargo compiler fingerprint json",
            )?;
            plain_file(
                &path.with_file_name(format!(
                    "dep-{}",
                    path.file_name().unwrap().to_string_lossy()
                )),
                "Cargo compiler fingerprint dep-info",
            )?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn unit_hash(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Legacy tags identify a cache, not ownership of every file inside it.
/// Only direct rustc outputs named by dep-info may be reclaimed. Build-script
/// output, incremental caches, unknown files and the manifests themselves stay.
fn declared_outputs(target: &Path) -> Result<Vec<PathBuf>> {
    let mut outputs = std::collections::BTreeSet::new();
    for profile in ["debug", "release"] {
        for dir in profile_output_dirs(&target.join(profile), false)? {
            if !plain_dir(&dir) || dir.canonicalize()? != dir {
                continue;
            }
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().is_none_or(|ext| ext != "d")
                    || !std::fs::symlink_metadata(&path)?.is_file()
                    || std::fs::metadata(&path)?.len() > 1024 * 1024
                    || path.canonicalize()? != path
                {
                    continue;
                }
                if dir.file_name().is_some_and(|name| name == "out")
                    && !v2_compiler_dep_info(&dir, &path)?
                {
                    continue;
                }
                let body = std::fs::read_to_string(&path)?;
                for line in body.lines() {
                    let Some((names, dependencies)) = line.split_once(": ") else {
                        continue;
                    };
                    if dependencies.trim().is_empty() {
                        continue;
                    }
                    // Rustc escapes spaces and backslashes in Make paths.
                    // Unsupported Make syntax fails closed for that record.
                    let mut words = Vec::new();
                    let mut word = String::new();
                    let mut escaped = false;
                    for c in names.chars() {
                        if escaped {
                            word.push(c);
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c.is_whitespace() {
                            if !word.is_empty() {
                                words.push(std::mem::take(&mut word));
                            }
                        } else {
                            word.push(c);
                        }
                    }
                    if escaped || word.contains(['$', '%']) {
                        continue;
                    }
                    if !word.is_empty() {
                        words.push(word);
                    }
                    for name in words {
                        let output = PathBuf::from(name);
                        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                        let output_stem = output.file_stem().unwrap_or_default().to_string_lossy();
                        let artifact_name =
                            output_stem == stem || output_stem == format!("lib{stem}");
                        let artifact_kind = output.extension().is_none_or(|ext| {
                            matches!(
                                ext.to_str(),
                                Some(
                                    "rlib"
                                        | "rmeta"
                                        | "so"
                                        | "dylib"
                                        | "a"
                                        | "lib"
                                        | "dll"
                                        | "exe"
                                        | "pdb"
                                )
                            )
                        });
                        if artifact_name
                            && artifact_kind
                            && output.parent() == Some(dir.as_path())
                            && output.extension().is_none_or(|ext| ext != "d")
                            && std::fs::symlink_metadata(&output).is_ok_and(|m| m.is_file())
                            && output.canonicalize().is_ok_and(|p| p == output)
                            && artifact_signature(&output)?
                        {
                            outputs.insert(output);
                        }
                    }
                }
            }
        }
    }
    Ok(outputs.into_iter().collect())
}

#[derive(Serialize, Deserialize)]
struct CargoResidue {
    target: PathBuf,
    files: std::collections::BTreeMap<PathBuf, Vec<u8>>,
}

// Keep the exact metadata as historical evidence. An old proof cannot adopt
// newly added files, changed metadata, symlinks or a different worktree.
fn residue_files(
    target: &Path,
    outputs: &[PathBuf],
) -> Result<std::collections::BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = std::collections::BTreeMap::new();
    let mut dirs = vec![target.to_owned()];
    let mut bytes = 0usize;
    let mut entries = 0usize;
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            entries += 1;
            ensure!(entries <= 4096, "residue inventory too large");
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "symlink in Cargo residue"
            );
            if metadata.is_dir() {
                dirs.push(path);
            } else {
                ensure!(metadata.is_file(), "non-file in Cargo residue");
                if outputs.contains(&path) {
                    continue;
                }
                ensure!(metadata.len() <= 1024 * 1024, "residue file too large");
                let body = std::fs::read(&path)?;
                bytes = bytes.saturating_add(body.len());
                ensure!(bytes <= 8 * 1024 * 1024, "residue inventory too large");
                files.insert(path.strip_prefix(target)?.to_owned(), body);
            }
        }
    }
    Ok(files)
}

fn residue_proof(home: &Path, id: &str, worktree: &Path) -> Result<PathBuf> {
    Ok(home
        .join("runs")
        .join(id)
        .join("build-residue")
        .join(worktree.file_name().context("missing worktree name")?))
}

// Artifact eviction tolerates unrelated root data. Whole-target removal must
// still reject it, including empty directories absent from the file inventory.
fn residue_root(target: &Path) -> Result<()> {
    for entry in std::fs::read_dir(target)? {
        let name = entry?.file_name();
        ensure!(
            matches!(
                name.to_str(),
                Some("debug" | "release" | "CACHEDIR.TAG" | ".rustc_info.json")
            ),
            "unrecognized Cargo residue root entry"
        );
    }
    Ok(())
}

fn record_residue(home: &Path, id: &str, target: &Path, outputs: &[PathBuf]) -> Result<()> {
    cache_tag(target, false)?;
    residue_root(target)?;
    let files = residue_files(target, outputs)?;
    for (relative, body) in &files {
        let path = target.join(relative);
        let identity = matches!(relative.to_str(), Some("CACHEDIR.TAG" | ".rustc_info.json"));
        let lock = matches!(
            relative.to_str(),
            Some("debug/.cargo-lock" | "release/.cargo-lock")
        ) && body.is_empty();
        let dep_info = path.extension().is_some_and(|ext| ext == "d")
            && outputs.iter().any(|output| {
                output.parent() == path.parent()
                    && (output.file_stem() == path.file_stem()
                        || output.file_stem().is_some_and(|stem| {
                            stem.to_string_lossy()
                                == format!(
                                    "lib{}",
                                    path.file_stem().unwrap_or_default().to_string_lossy()
                                )
                        }))
            });
        ensure!(identity || lock || dep_info, "unproven Cargo residue");
    }
    let proof = residue_proof(home, id, target.parent().context("missing worktree")?)?;
    std::fs::create_dir_all(proof.parent().context("missing proof directory")?)?;
    let tmp = proof.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&CargoResidue {
            target: target.to_owned(),
            files,
        })?,
    )?;
    std::fs::rename(tmp, proof)?;
    Ok(())
}

pub(crate) async fn verify_residue(home: &Path, id: &str, worktree: &Path) -> Result<Vec<File>> {
    let target = cargo_target(worktree).await?;
    residue_root(&target)?;
    let locks = cargo_locks(&target)?;
    let proof: CargoResidue =
        serde_json::from_slice(&std::fs::read(residue_proof(home, id, worktree)?)?)?;
    ensure!(
        proof.target == target && proof.files == residue_files(&target, &[])?,
        "Cargo residue changed or contains unproven files"
    );
    Ok(locks)
}

/// Supplemental exclusion for Cargo builds launched outside the managed seats.
pub(crate) fn cargo_locks(target: &Path) -> Result<Vec<File>> {
    let mut held = Vec::new();
    for (profile, name) in ["debug", "release"].into_iter().flat_map(|profile| {
        [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"].map(|name| (profile, name))
    }) {
        let lock = target.join(profile).join(name);
        match std::fs::symlink_metadata(&lock) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            result => {
                result.with_context(|| format!("Cargo lock: inspect {}", lock.display()))?;
            }
        }
        {
            plain_file(&lock, "Cargo lock")?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock)
                .with_context(|| format!("Cargo lock: open {}", lock.display()))?;
            crate::pr_head::try_lock_file(&file)
                .with_context(|| format!("Cargo build is active: {}", lock.display()))?;
            held.push(file);
        }
    }
    Ok(held)
}

/// All repositories share one cap and one per-pass deletion budget. The report
/// survives a restart and explains every skipped run/target or failed operation.
pub(crate) async fn sweep(home: &Path, disk: &crate::config::Disk, now: Timestamp) -> Report {
    let home = home.to_owned();
    let disk = disk.clone();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || runtime.block_on(sweep_inner(&home, &disk, now)))
        .await
        .unwrap_or_else(|e| Report {
            failures: vec![format!("retention worker failed: {e}")],
            ..Report::default()
        })
}

#[derive(Default, Serialize, Deserialize)]
struct Measurement {
    at: Option<Timestamp>,
    bytes: u64,
}

async fn sweep_inner(home: &Path, disk: &crate::config::Disk, now: Timestamp) -> Report {
    let mut report = Report {
        checked_at: Some(now),
        limit_bytes: disk.worktree_limit_bytes,
        reclaim_budget_bytes: disk.worktree_reclaim_bytes,
        ..Report::default()
    };
    if !cfg!(unix) {
        report
            .failures
            .push("Automatic output reclamation requires Unix process containment".into());
        return report;
    }
    let Ok(_janitor) = OutputLease::lock(&home.join("retention.lock"), true) else {
        return report;
    };
    let mut targets = Vec::new();
    let mut under_pressure = false;
    let mut registrations = std::collections::HashMap::new();
    let measurements_path = home.join("output-measurements.json");
    let mut measurements: std::collections::BTreeMap<PathBuf, Measurement> =
        std::fs::read(&measurements_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
    let mut observed = std::collections::BTreeSet::new();
    let mut ids: Vec<_> = std::fs::read_dir(home.join("runs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    for id in ids {
        if !crate::run::is_run_id(&id) {
            continue;
        }
        let state = match RunState::load_under(&id, home) {
            Ok(state) => state,
            Err(e) => {
                report
                    .retained
                    .push(format!("{id}: unreadable run ownership: {e:#}"));
                continue;
            }
        };
        if disk.worktree_limit_bytes == 0 {
            continue;
        }
        match run_paths_missing(&state) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                report.failures.push(format!("{id}: {e:#}"));
                continue;
            }
        }
        let list = match registrations.entry(state.repo.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                match git::git(&state.repo, &["worktree", "list", "--porcelain"]).await {
                    Ok(list) => entry.insert(list),
                    Err(e) => {
                        // The checkout may disappear after the inventory check.
                        // Do not confuse a missing Git executable with that case.
                        if matches!(run_paths_missing(&state), Ok(true)) {
                            continue;
                        }
                        report.failures.push(format!("{id}: {e:#}"));
                        continue;
                    }
                }
            }
        };
        let paths = match registered_worktrees(&state, list) {
            Ok(paths) => paths,
            Err(e) => {
                report.failures.push(format!("{id}: {e:#}"));
                continue;
            }
        };
        for path in paths {
            if !path.join("target").exists() {
                continue;
            }
            let target = path.join("target");
            observed.insert(target.clone());
            let pressure =
                crate::disk::free_bytes(&path).map_or(true, |free| free < disk.min_free_bytes);
            under_pressure |= pressure;
            let measurement = measurements.entry(target).or_default();
            if pressure
                || measurement
                    .at
                    .is_none_or(|at| crate::clean::due(now, at, 300))
            {
                measurement.bytes = crate::disk::dir_size(&path.join("target"));
                measurement.at = Some(now);
            }
            let size = measurement.bytes;
            report.measured_bytes = report.measured_bytes.saturating_add(size);
            targets.push((state.output_activity(), id.clone(), path, size));
        }
    }
    if under_pressure {
        // Inventory sizes prioritize scarce-space recovery only; deletion authority
        // and budget accounting come from declared outputs measured under locks.
        targets.sort_by_key(|t| (std::cmp::Reverse(t.3), t.0));
    } else {
        targets.sort_by_key(|t| t.0);
    }
    let free_before = crate::disk::free_bytes(home).ok();
    #[cfg(unix)]
    {
        let mut seen = std::collections::BTreeSet::new();
        report.measured_allocated_file_bytes = Some(0);
        report.estimated_released_allocated_bytes = Some(0);
        report.proven_reclaimable_allocated_bytes = Some(0);
        for (_, _, path, _) in &targets {
            report.measured_allocated_file_bytes = report
                .measured_allocated_file_bytes
                .zip(allocated_inventory(&path.join("target"), &mut seen).ok())
                .map(|(a, b)| a.saturating_add(b));
        }
    }
    let mut remaining = report.measured_bytes;
    let pressure_paths: Vec<_> = targets.iter().map(|t| t.2.clone()).collect();
    let mut deletion_started = false;
    let mut oversized_started = false;
    for (updated, id, path, _) in targets {
        if disk.worktree_limit_bytes == 0
            || remaining <= disk.worktree_limit_bytes
                && crate::disk::free_bytes(&path).is_ok_and(|free| free >= disk.min_free_bytes)
        {
            report.retained.push(format!("{id}: output reclamation disabled or aggregate usage and free space are within policy"));
            continue;
        }
        if oversized_started || report.freed_bytes >= disk.worktree_reclaim_bytes {
            report
                .retained
                .push(format!("{id}: per-pass byte budget spent"));
            continue;
        }
        let result = async {
            ensure!(
                crate::clean::due(now, updated, disk.fold_grace_secs),
                "execution activity within grace period: {updated} (now {now}, grace {}s)", disk.fold_grace_secs
            );
            let _owner = crate::pr_head::owner_lock(home, &id)?;
            let state = RunState::load_under(&id, home)?;
            ensure!(
                state.liveness_under(home, crate::daemon::is_working_on(home, &id, now))
                    == Liveness::Dead,
                "run live or ownership unknown"
            );
            ensure!(crate::clean::due(now, state.output_activity(), disk.fold_grace_secs), "execution activity after inventory: {}", state.output_activity());
            let _driver = crate::driver::maintenance(home, &id)?;
            let _outputs = OutputLease::take(&path, true)?;
            ensure!(
                worktrees(&state).await?.contains(&path),
                "worktree registration changed"
            );
            let target = validate_cargo_target(&path, true).await
                .with_context(|| format!("stage validate-target: {}", path.join("target").display()))?;
            let _cargo = cargo_locks(&target)
                .with_context(|| format!("stage cargo-locks: {}", target.display()))?;
            // Cargo may have finished between identity inspection and locking.
            // Revalidate the same authority with all writers excluded.
            validate_cargo_target(&path, true).await
                .with_context(|| format!("stage validate-target (locked): {}", target.display()))?;
            let lease_meta = _outputs._file.metadata().context("output lease activity: metadata")?;
            if lease_meta.len() > 0 {
                ensure!(lease_meta.len() == 1, "output lease activity witness is unreadable or incomplete for {}", path.display());
                let lease_at = Timestamp::try_from(lease_meta.modified().context("output lease activity: modified")?)?;
                ensure!(crate::clean::due(now, lease_at, disk.fold_grace_secs),
                    "output lease activity within grace period: {lease_at} (now {now}, grace {}s)", disk.fold_grace_secs);
            }
            output_age(&target, now, disk.fold_grace_secs)
                .with_context(|| format!("stage output-age: {}", target.display()))?;
            let mut outputs = declared_outputs(&target)
                .with_context(|| format!("stage declared-outputs: {}", target.display()))?;
            let incremental = match crate::incremental::candidates(&path) {
                Ok(candidates) => candidates,
                Err(error) => {
                    report.retained.push(format!("{id}: incremental cache retained: {error:#}"));
                    crate::incremental::Reclaim::default()
                }
            };
            let incremental_files = incremental.files.clone();
            outputs.extend(incremental_files.iter().cloned());
            outputs.sort();
            outputs.dedup();
            ensure!(!outputs.is_empty(), "no dep-info-proven outputs or provenance-proven incremental caches; legacy incremental/build and unknown files retained");
            let sizes = outputs.iter().map(|p| std::fs::metadata(p).map(|m| m.len())
                .with_context(|| format!("stage measure: {}", p.display())))
                .collect::<Result<Vec<_>>>()?;
            let current_size = sizes.iter().fold(0u64, |bytes, size| bytes.saturating_add(*size));
            report.proven_reclaimable_bytes = report.proven_reclaimable_bytes.saturating_add(current_size);
            let mut allocation = ReleasedAllocation::of(&outputs)?;
            report.proven_reclaimable_allocated_bytes = report.proven_reclaimable_allocated_bytes
                .zip(allocation.reclaimable_allocated_bytes()).map(|(a, b)| a.saturating_add(b));
            let exceeds_budget = current_size > disk.worktree_reclaim_bytes.saturating_sub(report.freed_bytes);
            // 予算は1回の削除量を抑えるもので、大きな出力を永久に保持するためではない。
            // 正の予算では最初の削除対象だけ超過を許し、その後は予算消費済みとする。
            // 途中で失敗しても再度許可しないよう、例外は削除開始時に消費する。
            ensure!(!deletion_started || !exceeds_budget,
                "declared outputs exceed remaining per-pass byte budget");
            // Record adoption before deletion; interrupted deletion is revalidated
            // on the next pass. No source file or branch is modified.
            let ledger = home.join("runs").join(&id).join("build-outputs.json");
            std::fs::write(
                &ledger,
                serde_json::to_vec(
                    &serde_json::json!({"target":target,"bytes":current_size,"at":now,"action":"reclaim-declared-outputs","identity":if target.join("CACHEDIR.TAG").exists() { "cache-tag" } else { "rustc-profile-dep-info" },"files":outputs}),
                )?,
            ).with_context(|| format!("stage ledger: {}", ledger.display()))?;
            // Unknown leftovers still permit artifact-only eviction, but never
            // authorize retirement. Only a complete proven residue is adopted.
            if !incremental_files.is_empty() {
                report.retained.push(format!("{id}: incremental proof does not authorize retirement"));
            } else if let Err(error) = record_residue(home, &id, &target, &outputs) {
                report.retained.push(format!("{id}: residue not adopted for retirement: {error:#}"));
            }
            ensure!(worktrees(&state).await?.contains(&path),
                "worktree registration changed before deletion");
            deletion_started = true;
            oversized_started = exceeds_budget;
            measurements.remove(&target);
            for (output, size) in outputs.iter().zip(sizes) {
                std::fs::remove_file(output).with_context(|| format!("stage remove: {}", output.display()))?;
                report.estimated_released_allocated_bytes = report.estimated_released_allocated_bytes
                    .zip(allocation.unlinked(output)).map(|(a, b)| a.saturating_add(b));
                // Preserve actual progress even if a later removal fails.
                report.freed_bytes = report.freed_bytes.saturating_add(size);
                remaining = remaining.saturating_sub(size);
            }
            incremental.finish().context("finish incremental reclamation")?;
            report.retained.push(format!("{id}: unmanifested files and Cargo metadata retained"));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            report.retained.push(format!(
                "{id}/{}: {e:#}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    measurements.retain(|path, _| observed.contains(path));
    if let Ok(body) = serde_json::to_vec(&measurements)
        && let Err(e) = std::fs::write(&measurements_path, body)
    {
        report.failures.push(format!("output measurements: {e}"));
    }
    copies::evict(home, disk, now, &mut report).await;
    retire_empty(home, disk, now, &mut report).await;
    report.protected_or_unproven_bytes = report
        .measured_bytes
        .saturating_sub(report.proven_reclaimable_bytes);
    let pressure_remaining = pressure_paths
        .iter()
        .any(|path| crate::disk::free_bytes(path).map_or(true, |free| free < disk.min_free_bytes));
    if !report.failures.is_empty()
        || disk.worktree_limit_bytes > 0
            && (remaining > disk.worktree_limit_bytes || pressure_remaining)
    {
        let details = report
            .retained
            .iter()
            .filter(|reason| {
                !reason.contains("within policy")
                    && !reason.ends_with("unmanifested files and Cargo metadata retained")
            })
            .take(4)
            .chain(report.failures.iter().take(2))
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        crate::notices::raise_in(
            home,
            crate::notices::Notice::warn(
                "disk:retention",
                format!(
                    "Retention remains blocked by the disk cap, free-space floor or inspection failures (logical inventory {}B, proven reclaimable {}B, protected or unproven {}B, unlinked {}B). {} Retained/held targets: {details}. Inspect the cleanup report for active owners, retained recovery sources and failures.",
                    report.measured_bytes,
                    report.proven_reclaimable_bytes,
                    report.protected_or_unproven_bytes,
                    report.freed_bytes,
                    if report.proven_reclaimable_bytes == 0 {
                        "No inspected contents have deletion authority."
                    } else {
                        "Authorized reclamation was insufficient; remaining protections and budgets still apply."
                    }
                ),
            ),
        );
    }
    report.observed_home_free_change_bytes = free_before
        .zip(crate::disk::free_bytes(home).ok())
        .map(|(before, after)| {
            (i128::from(after) - i128::from(before))
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
        });
    let store = home.join("retention.json");
    if let Ok(body) = serde_json::to_vec_pretty(&report) {
        let tmp = home.join("retention.json.tmp");
        if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(tmp, store)) {
            tracing::warn!("retention report could not be saved: {e}");
        }
    }
    tracing::info!(
        measured_bytes = report.measured_bytes,
        freed_bytes = report.freed_bytes,
        retired = report.retired,
        retained = report.retained.len(),
        failures = report.failures.len(),
        "retained worktree cleanup"
    );
    for reason in report.retained.iter().chain(&report.failures) {
        tracing::debug!(%reason, "worktree retained");
    }
    report
}

async fn retire_empty(
    home: &Path,
    disk: &crate::config::Disk,
    now: Timestamp,
    report: &mut Report,
) {
    if !disk.auto_fold {
        return;
    }
    let queue = crate::queue::Queue::at(home.join("queue"));
    let questions = crate::ask::Questions::at(home.join("questions"));
    let open = questions.list_for_effects();
    for listed in queue.list() {
        let Ok(_claim) = queue.claim(&listed.id) else {
            continue;
        };
        let Ok(task) = queue.get(&listed.id) else {
            continue;
        };
        if open
            .iter()
            .any(|q| (q.run == task.id || task.blocked_by.contains(&q.id)) && q.status.open())
        {
            report.retained.push(format!(
                "{}: task has an unanswered owner decision",
                task.id
            ));
            continue;
        }
        let sources: Vec<_> = task
            .runs
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .cloned()
            .collect();
        let mut useful = Vec::new();
        let mut selected = Vec::new();
        let mut public = Vec::new();
        let mut unreadable = false;
        for id in &sources {
            let Ok(state) = RunState::load_under(id, home) else {
                unreadable = true;
                continue;
            };
            if state.released() || state.retired_to.is_some() {
                continue;
            }
            if task.pr_recovery.is_some() && state.pr_recovery == task.pr_recovery {
                selected.push(state.clone());
            }
            if state.pr.is_some() && state.pr_taken_over_by.is_none() {
                public.push(state.clone());
            }
            if state.candidates.iter().any(|c| c.commits > 0 && !c.empty) {
                useful.push(state);
            }
        }
        // Prefer a durable owner-approved selection, then one existing PR
        // owner, then an unambiguous useful source. Distinct historical changes
        // without an owner selection cannot be resolved by creation time.
        let pool = if !selected.is_empty() {
            selected
        } else if !public.is_empty() {
            public
        } else {
            useful
        };
        if unreadable || pool.len() != 1 {
            report.retained.push(format!(
                "{}: recovery authority is ambiguous or history is unreadable; {} sources retained",
                task.id,
                sources.len()
            ));
            continue;
        }
        let authority = &pool[0];
        if authority.candidates.is_empty() {
            continue;
        }
        let latest = &authority.id;
        for id in &sources {
            if id == latest || report.retired >= 5 {
                continue;
            }
            if let Ok(state) = RunState::load_under(id, home)
                && state.retired_to.as_ref() == Some(latest)
                && worktrees(&state).await.is_ok_and(|paths| paths.is_empty())
            {
                continue;
            }
            let result = async {
                ensure!(
                    !task
                        .resume_override
                        .as_ref()
                        .is_some_and(|pin| pin.pinned_run.as_ref() == Some(id)),
                    "operator-pinned recovery run"
                );
                ensure!(
                    !task.replacement.as_ref().is_some_and(|pin| pin.run == *id),
                    "selected replacement recovery source"
                );
                let _owner = crate::pr_head::owner_lock(home, id)?;
                let mut state = RunState::load_under(id, home)?;
                ensure!(
                    state.repo == authority.repo,
                    "repository differs from recovery authority"
                );
                ensure!(
                    !crate::pr_recovery::explicit_sources_retained(home, id),
                    "explicitly retained recovery source"
                );
                ensure!(
                    state.liveness_under(home, crate::daemon::is_working_on(home, id, now))
                        == Liveness::Dead,
                    "run live or ownership unknown"
                );
                let _driver = crate::driver::maintenance(home, id)?;
                ensure!(
                    crate::clean::due(now, state.updated_at, disk.fold_grace_secs),
                    "within grace period"
                );
                ensure!(
                    !open.iter().any(|q| q.run == *id && q.status.open()),
                    "unanswered owner decision"
                );
                ensure!(
                    state.pr.is_none()
                        && state.pr_head.is_none()
                        && state.pr_recovery.is_none()
                        && state.land_approval.is_none()
                        && state.merge_decision.is_none()
                        && state.reviews.is_empty()
                        && state.recovery.is_none()
                        && state.candidate_preservation.is_empty()
                        && state.interrupted_implementations.is_empty()
                        && !state.released(),
                    "PR ownership or recovery checkpoint"
                );
                let proof = prove_empty(home, &state, latest).await?;
                commit_retirement(home, &queue, &task, &mut state, latest, &sources, now)?;
                // Durable retirement precedes removal. A crash retries removal
                // using the same proof; it cannot make this attempt resumable.
                for path in &proof.paths {
                    ensure!(
                        git::worktree_remove_clean(&state.repo, path).await?,
                        "Git refused clean worktree removal"
                    );
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            match result {
                Ok(()) => report.retired += 1,
                Err(e) => report.retained.push(format!("{id}: {e:#}")),
            }
        }
    }
}

struct EmptyProof {
    paths: Vec<PathBuf>,
    _outputs: Vec<OutputLease>,
    _cargo: Vec<File>,
}

// Compare actual source bytes with the original tree independently of index
// stat caches, assume-unchanged flags, sparse checkout and fsmonitor hints.
pub(crate) async fn tracked_base(worktree: &Path, base: &str) -> Result<()> {
    let tree = git::git(worktree, &["ls-tree", "-r", "--full-tree", "-z", base]).await?;
    let mut files = Vec::new();
    let mut blobs = Vec::new();
    for record in tree.split('\0').filter(|s| !s.is_empty()) {
        let (header, name) = record.split_once('\t').context("unknown tree record")?;
        let parts: Vec<_> = header.split_whitespace().collect();
        ensure!(
            parts.len() == 3 && matches!(parts[0], "100644" | "100755") && parts[1] == "blob",
            "unsupported tracked source kind"
        );
        let path = worktree.join(name);
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink() && path.canonicalize()? == path,
            "noncanonical tracked source"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            ensure!(
                (meta.permissions().mode() & 0o111 != 0) == (parts[0] == "100755"),
                "tracked executable mode differs from original base"
            );
        }
        path.to_str().context("non UTF-8 source")?;
        files.push(git::path_arg(&path));
        blobs.push(parts[2].to_owned());
    }
    for (paths, expected) in files.chunks(64).zip(blobs.chunks(64)) {
        let mut args = vec!["hash-object", "--no-filters", "--"];
        args.extend(paths.iter().map(String::as_str));
        let actual = git::git(worktree, &args).await?;
        ensure!(
            actual.lines().eq(expected.iter().map(String::as_str)),
            "tracked bytes differ from original base"
        );
    }
    Ok(())
}

async fn prove_empty(home: &Path, state: &RunState, authority: &str) -> Result<EmptyProof> {
    let id = &state.id;
    let paths = worktrees(state).await?;
    let mut guards = Vec::new();
    let mut cargo_guards = Vec::new();
    for path in &paths {
        guards.push(OutputLease::take(path, true)?);
        ensure!(
            git::rev_parse(path, "HEAD").await? == state.base_commit,
            "unique or unrecorded commits"
        );
        ensure!(
            git::git(
                path,
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain",
                    "--untracked-files=all"
                ]
            )
            .await?
            .is_empty(),
            "tracked or untracked source changes"
        );
        ensure!(
            git::git(
                path,
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--ignored",
                    "--",
                    ".",
                    ":(exclude)target"
                ]
            )
            .await?
            .is_empty(),
            "source changes, ignored files or build outputs"
        );
        tracked_base(path, &state.base_commit).await?;
        match std::fs::symlink_metadata(path.join("target")) {
            Ok(_) => cargo_guards.extend(verify_residue(home, id, path).await?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        for checkpoint in [
            "rebase-merge",
            "rebase-apply",
            "sequencer",
            "BISECT_LOG",
            "REVERT_HEAD",
            "AUTO_MERGE",
            "MERGE_MSG",
            "CHERRY_PICK_HEAD",
            "MERGE_HEAD",
            "REBASE_HEAD",
        ] {
            let location = git::git(
                path,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    checkpoint,
                ],
            )
            .await?;
            ensure!(
                matches!(std::fs::symlink_metadata(location.trim()), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "retained Git recovery metadata: {checkpoint}"
            );
        }
        // Git also initializes ORIG_HEAD when creating a worktree.
        // Only an origin outside the base is a recovery source.
        let origin_path = git::git(
            path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "ORIG_HEAD",
            ],
        )
        .await?;
        match std::fs::symlink_metadata(origin_path.trim()) {
            Ok(meta) => {
                ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "unknown ORIG_HEAD artifact"
                );
                git::rev_parse(path, "ORIG_HEAD")
                    .await
                    .context("unreadable ORIG_HEAD")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let Ok(original) = git::rev_parse(path, "ORIG_HEAD").await {
            ensure!(
                git::git_raw(
                    path,
                    &["merge-base", "--is-ancestor", &original, &state.base_commit]
                )
                .await?
                .ok(),
                "ORIG_HEAD retains commits outside the original base"
            );
        }
        let heads = git::git(path, &["reflog", "show", "--format=%H", "HEAD"]).await?;
        for head in heads.lines().collect::<std::collections::BTreeSet<_>>() {
            ensure!(
                git::git_raw(
                    path,
                    &["merge-base", "--is-ancestor", head, &state.base_commit]
                )
                .await?
                .ok(),
                "worktree reflog retains commits outside the original base"
            );
        }
        for checkpoint in ["CHERRY_PICK_HEAD", "MERGE_HEAD", "REBASE_HEAD"] {
            ensure!(
                !git::git_raw(path, &["rev-parse", "--verify", checkpoint])
                    .await?
                    .ok(),
                "unfinished Git operation"
            );
        }
    }
    // Missing worktrees cannot establish that unrecorded prep edits
    // were empty. Recorded candidates must also point at the base.
    if paths.is_empty() && state.retired_to.as_deref() == Some(authority) {
        return Ok(EmptyProof {
            paths,
            _outputs: guards,
            _cargo: cargo_guards,
        });
    }
    ensure!(!paths.is_empty(), "no worktrees to prove empty");
    for candidate in &state.candidates {
        ensure!(
            (paths.contains(&candidate.worktree)
                || state.retired_to.as_deref() == Some(authority) && !candidate.worktree.exists())
                && git::rev_parse(&state.repo, &candidate.branch).await? == state.base_commit,
            "candidate recovery source"
        );
    }
    let pattern = format!("refs/heads/magi/{}/", state.short());
    let branches = git::git(
        &state.repo,
        &["for-each-ref", "--format=%(objectname)", &pattern],
    )
    .await?;
    ensure!(
        branches.lines().all(|head| head == state.base_commit),
        "unrecorded candidate branch changes"
    );
    Ok(EmptyProof {
        paths,
        _outputs: guards,
        _cargo: cargo_guards,
    })
}

/// The only task-write critical section in retirement. All Git proof and
/// deletion happens outside it, under the run's owner/driver/output leases.
/// Atomic task writes make the earlier snapshot consistent; this final compare
/// rejects every intervening owner selection, hold, edit or history update.
fn commit_retirement(
    home: &Path,
    queue: &crate::queue::Queue,
    expected: &crate::queue::Task,
    state: &mut RunState,
    authority: &str,
    sources: &[String],
    now: Timestamp,
) -> Result<()> {
    let _write = queue.lock_task(&expected.id)?;
    let current = queue.get(&expected.id)?;
    ensure!(
        serde_json::to_value(&current)? == serde_json::to_value(expected)?,
        "task changed during retirement proof; retaining worktrees for the current owner decision"
    );
    ensure!(
        !current
            .resume_override
            .as_ref()
            .is_some_and(|pin| pin.pinned_run.as_ref() == Some(&state.id))
            && !current
                .replacement
                .as_ref()
                .is_some_and(|pin| pin.run == state.id),
        "current owner selected this recovery source"
    );
    ensure!(
        !crate::pr_recovery::explicit_sources_retained(home, &state.id),
        "explicitly retained recovery source"
    );
    ensure!(
        !crate::ask::Questions::at(home.join("questions"))
            .list_for_effects()
            .iter()
            .any(|q| q.status.open()
                && (q.run == state.id
                    || q.run == current.id
                    || current.blocked_by.contains(&q.id))),
        "owner decision arrived during retirement proof"
    );
    let dir = home.join("recovery-lineages");
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{}.tmp", current.id));
    let lineage = serde_json::json!({"task":current.id,"authority":authority,"sources":sources,
        "selection":current.pr_recovery,"task_updated_at":current.updated_at,"at":now});
    std::fs::write(&tmp, serde_json::to_vec_pretty(&lineage)?)?;
    std::fs::rename(tmp, dir.join(format!("{}.json", current.id)))?;
    state.retired_to = Some(authority.to_owned());
    state.event(
        "retention",
        format!(
            "empty attempt retired to {authority}; recovery sources: {}",
            sources.join(", ")
        ),
    );
    state.save_under(home)?;
    // No Git command or worktree removal may be added before this guard drops.
    Ok(())
}

/// Wait for the emergency reserve to be exhausted. The normal floor requests
/// a checkpoint first; half the floor stops owned writers before node completion.
/// A failed measurement also closes admission rather than allowing blind writes.
pub(crate) async fn emergency(paths: Vec<PathBuf>, min_free: u64) -> String {
    if min_free == 0 {
        return std::future::pending().await;
    }
    loop {
        for path in &paths {
            let existing = path.ancestors().find(|p| p.exists()).unwrap_or(path);
            match crate::disk::free_bytes(existing) {
                Ok(free) if free >= min_free / 2 => {}
                Ok(free) => {
                    return format!(
                        "disk pressure: {free} bytes free; stopping owned writers to preserve the emergency reserve"
                    );
                }
                Err(e) => return format!("disk pressure: free space measurement failed: {e:#}"),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Disk},
        queue::{Source, Task},
        run::{Candidate, RunStatus},
    };

    struct Fx {
        _temp: tempfile::TempDir,
        home: PathBuf,
        repo: PathBuf,
        bay: PathBuf,
        base: String,
    }
    impl Fx {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let home = root.join("home");
            let repo = root.join("repo");
            let bay = root.join("worktrees");
            std::fs::create_dir_all(&repo).unwrap();
            std::fs::create_dir_all(home.join("runs")).unwrap();
            git::git(&repo, &["init", "-b", "main"]).await.unwrap();
            git::git(&repo, &["config", "user.name", "Fixture"])
                .await
                .unwrap();
            git::git(&repo, &["config", "user.email", "fixture@example.invalid"])
                .await
                .unwrap();
            git::git(&repo, &["config", "commit.gpgsign", "false"])
                .await
                .unwrap();
            std::fs::write(repo.join(".gitignore"), "target/\nscratch/\n").unwrap();
            std::fs::write(repo.join("source"), "base\n").unwrap();
            git::git(&repo, &["add", "."]).await.unwrap();
            git::git(&repo, &["commit", "-m", "fixture"]).await.unwrap();
            let base = git::rev_parse(&repo, "HEAD").await.unwrap();
            Self {
                _temp: temp,
                home,
                repo,
                bay,
                base,
            }
        }
        async fn run(&self, tag: &str, useful: bool) -> RunState {
            let mut cfg = Config::default();
            cfg.graph.worktree_root = Some(self.bay.clone());
            let mut state = RunState::new(
                self.repo.clone(),
                "main".into(),
                self.base.clone(),
                "task".into(),
                cfg,
            );
            state.id = format!("20260101-000000-{tag}");
            state.status = RunStatus::Blocked;
            state.driver_exited = true;
            let wt = state.worktree_root().join("cand-A");
            let branch = format!("magi/{tag}/A");
            git::worktree_add_branch(&self.repo, &wt, &branch, &self.base)
                .await
                .unwrap();
            if useful {
                std::fs::write(wt.join("source"), tag).unwrap();
                git::git(&wt, &["add", "."]).await.unwrap();
                git::git(&wt, &["commit", "-m", "candidate change"])
                    .await
                    .unwrap();
            }
            state.candidates.push(Candidate {
                index: 0,
                label: 'A',
                agent: "fixture".into(),
                branch,
                worktree: wt,
                summary: String::new(),
                stat: String::new(),
                files: usize::from(useful),
                commits: usize::from(useful),
                empty: !useful,
                failed: None,
                verified_noop: None,
                duration_ms: 0,
                folded: false,
            });
            state.save_under(&self.home).unwrap();
            state
        }
        fn disk() -> Disk {
            Disk {
                worktree_limit_bytes: 1,
                worktree_reclaim_bytes: 1024 * 1024,
                fold_grace_secs: 0,
                min_free_bytes: 0,
                ..Disk::default()
            }
        }
        fn target(wt: &Path) {
            let target = wt.join("target");
            std::fs::create_dir_all(target.join("debug")).unwrap();
            let mut artifact = [1; 200];
            artifact[..4].copy_from_slice(b"\x7fELF");
            std::fs::write(target.join("debug/output"), artifact).unwrap();
            let output = target
                .join("debug/output")
                .display()
                .to_string()
                .replace('\\', "\\\\")
                .replace(' ', "\\ ");
            std::fs::write(target.join("debug/output.d"), format!("{output}: source\n")).unwrap();
            std::fs::write(target.join("debug/.cargo-lock"), "").unwrap();
            std::fs::write(
                target.join("CACHEDIR.TAG"),
                "Signature: 8a477f597d28d172789f06886806bc55\n",
            )
            .unwrap();
            std::fs::write(
                target.join(".rustc_info.json"),
                "{\"rustc_fingerprint\":123}",
            )
            .unwrap();
        }
        fn now() -> Timestamp {
            Timestamp::now() + jiff::SignedDuration::from_secs(2)
        }
        fn task(&self, runs: &[RunState]) -> Task {
            let mut task = Task::new(
                "fixture".into(),
                "task".into(),
                self.repo.clone(),
                Source::Human,
            );
            task.runs = runs.iter().map(|s| s.id.clone()).collect();
            crate::queue::Queue::at(self.home.join("queue"))
                .put(&mut task)
                .unwrap();
            task
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn incremental_only_reclamation_preserves_legacy_and_unknown_data() {
        let f = Fx::new().await;
        let state = f.run("ca01", true).await;
        let wt = &state.candidates[0].worktree;
        let generation = crate::incremental::tests::generate(wt);
        // Remove dep-info authority: incremental proof must be independent.
        assert!(!declared_outputs(&wt.join("target")).unwrap().is_empty());
        for dir in profile_output_dirs(&wt.join("target/debug"), true).unwrap() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|x| x == "d") {
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
        assert!(declared_outputs(&wt.join("target")).unwrap().is_empty());
        assert_eq!(
            sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes,
            0,
            "live generation is protected"
        );
        drop(generation);
        let candidates = crate::incremental::candidates(wt).unwrap();
        let logical: u64 = candidates
            .files
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .sum();
        let unknown = candidates.files[0].parent().unwrap().join("operator-note");
        std::fs::write(&unknown, "retain unknown data").unwrap();
        drop(candidates);
        let refused = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(refused.freed_bytes, 0, "{refused:?}");
        assert!(
            refused
                .retained
                .iter()
                .any(|s| s.contains("provenance mismatch"))
        );
        std::fs::remove_file(&unknown).unwrap();
        let legacy = wt.join("target/debug/incremental/legacy-session");
        std::fs::create_dir(&legacy).unwrap();
        // Even compiler-authored bytes do not retroactively confer ownership.
        let candidates = crate::incremental::candidates(wt).unwrap();
        let bin = candidates
            .files
            .iter()
            .find(|p| p.extension().is_some_and(|x| x == "bin"))
            .unwrap();
        std::fs::copy(bin, legacy.join("query-cache.bin")).unwrap();
        drop(candidates);
        let cargo = cargo_locks(&wt.join("target")).unwrap();
        assert_eq!(sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes, 0);
        drop(cargo);
        let outputs = OutputLease::borrow(wt).unwrap().unwrap();
        assert_eq!(sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes, 0);
        drop(outputs);
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.freed_bytes, logical, "{report:?}");
        assert_eq!(report.proven_reclaimable_bytes, logical);
        assert!(report.measured_bytes > logical);
        assert_eq!(
            report.protected_or_unproven_bytes,
            report.measured_bytes - logical
        );
        #[cfg(unix)]
        {
            assert!(report.estimated_released_allocated_bytes.unwrap() > 0);
            assert!(
                report.measured_allocated_file_bytes.unwrap()
                    >= report.estimated_released_allocated_bytes.unwrap()
            );
        }
        assert!(report.observed_home_free_change_bytes.is_some());
        assert!(legacy.join("query-cache.bin").is_file());
        assert!(wt.join("src/lib.rs").is_file());
        assert_eq!(report.retired, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn incremental_and_deps_links_account_for_retained_third_link() {
        use std::os::unix::fs::MetadataExt;
        let f = Fx::new().await;
        let state = f.run("ca02", true).await;
        let wt = &state.candidates[0].worktree;
        let generation = crate::incremental::tests::generate(wt);
        drop(generation);
        let candidates = crate::incremental::candidates(wt).unwrap();
        let objects = candidates
            .files
            .iter()
            .filter(|p| p.extension().is_some_and(|x| x == "o"))
            .take(2)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            objects.len(),
            2,
            "fixture must produce real incremental objects"
        );
        let mut outputs = candidates.files.clone();
        drop(candidates);
        let target = wt.join("target");
        assert!(!declared_outputs(&target).unwrap().is_empty());
        let generated_dirs = profile_output_dirs(&target.join("debug"), true).unwrap();
        // The directory Cargo itself wrote its dep-info into (v1 `deps`, v2 unit
        // `out`): the links below go there, so the real layout is exercised.
        let mut output_dir = None;
        for dir in &generated_dirs {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|x| x == "d") {
                    std::fs::remove_file(path).unwrap();
                    if dir != &generated_dirs[0] {
                        output_dir = Some(dir.clone());
                    }
                }
            }
        }
        let output_dir = output_dir.expect("Cargo generated no recognised output directory");
        // Cargo also leaves hard-linked object outputs beside the dep-info.
        // Remove those fixture outputs so the only external links are the
        // ones below.
        for entry in std::fs::read_dir(&output_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                std::fs::remove_file(path).unwrap();
            }
        }
        // v2 outputs need a dep-info stem matching a compiler fingerprint of
        // their unit; v1 has no such rule.
        let stem = if output_dir.file_name().is_some_and(|n| n == "out") {
            let unit = output_dir.parent().unwrap();
            let hash = unit.file_name().unwrap().to_string_lossy().into_owned();
            let name = std::fs::read_dir(unit.join("fingerprint"))
                .unwrap()
                .filter_map(|e| e.unwrap().file_name().into_string().ok())
                .find_map(|n| {
                    n.strip_prefix("lib-")
                        .filter(|n| !n.contains('.'))
                        .map(str::to_owned)
                })
                .expect("Cargo unit has a lib fingerprint");
            format!("{}-{hash}", name.replace('-', "_"))
        } else {
            "link".to_owned()
        };
        // One dep-info file may declare several outputs; the names differ only
        // by the optional `lib` prefix the artifact check accepts.
        let mut declared = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            assert_eq!(std::fs::metadata(object).unwrap().nlink(), 1);
            let dep = output_dir.join(if index == 0 {
                stem.clone()
            } else {
                format!("lib{stem}")
            });
            std::fs::hard_link(object, &dep).unwrap();
            declared.push(dep.display().to_string());
            outputs.push(dep);
        }
        std::fs::write(
            output_dir.join(format!("{stem}.d")),
            format!("{}: src/lib.rs\n", declared.join(" ")),
        )
        .unwrap();
        let retained = wt.join("retained-input");
        std::fs::hard_link(&objects[1], &retained).unwrap();
        let sentinel = std::fs::read(&retained).unwrap();
        let logical: u64 = outputs
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .sum();
        let mut seen = std::collections::BTreeSet::new();
        let retained_meta = std::fs::metadata(&retained).unwrap();
        let retained_inode = (retained_meta.dev(), retained_meta.ino());
        let mut expected = 0u64;
        for output in &outputs {
            let m = std::fs::metadata(output).unwrap();
            let inode = (m.dev(), m.ino());
            if inode != retained_inode && seen.insert(inode) {
                expected += m.blocks() * 512;
            }
        }
        assert!(expected > 0);
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.proven_reclaimable_bytes, logical, "{report:?}");
        assert_eq!(report.freed_bytes, logical, "{report:?}");
        assert_eq!(report.proven_reclaimable_allocated_bytes, Some(expected));
        assert_eq!(report.estimated_released_allocated_bytes, Some(expected));
        assert!(outputs.iter().all(|p| !p.exists()));
        assert_eq!(std::fs::read(retained).unwrap(), sentinel);
        assert!(wt.join("src/lib.rs").is_file());
    }

    #[tokio::test]
    async fn retention_notice_explains_unproven_inventory_and_live_owner() {
        let f = Fx::new().await;
        let state = f.run("ca03", true).await;
        let wt = &state.candidates[0].worktree;
        Fx::target(wt);
        std::fs::remove_file(wt.join("target/debug/output")).unwrap();
        let unknown = wt.join("target/debug/operator-note");
        std::fs::write(&unknown, vec![1; 4096]).unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.proven_reclaimable_bytes, 0);
        #[cfg(unix)]
        assert_eq!(report.proven_reclaimable_allocated_bytes, Some(0));
        assert!(report.protected_or_unproven_bytes >= 4096);
        let message = || {
            crate::notices::Notices::at(f.home.join("notifications"))
                .list()
                .into_iter()
                .find(|n| n.key == "disk:retention")
                .unwrap()
                .message
        };
        let notice = message();
        assert!(notice.contains("proven reclaimable 0B"), "{notice}");
        assert!(notice.contains("protected or unproven"), "{notice}");
        assert!(
            notice.contains("legacy incremental/build and unknown files retained"),
            "{notice}"
        );
        let owner = crate::pr_head::owner_lock(&f.home, &state.id).unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.freed_bytes, 0);
        let notice = message();
        assert!(notice.contains(&state.id), "{notice}");
        assert!(
            notice.contains("lock") || notice.contains("owner"),
            "{notice}"
        );
        drop(owner);
        assert_eq!(std::fs::read(unknown).unwrap(), vec![1; 4096]);
    }

    #[tokio::test]
    async fn real_cargo_build_holds_the_target_against_reclamation() {
        let f = Fx::new().await;
        let state = f.run("ca02", true).await;
        let wt = &state.candidates[0].worktree;
        Fx::target(wt);
        std::fs::create_dir(wt.join("src")).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname='live-cache-fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(wt.join("src/lib.rs"), "pub fn answer() -> usize { 42 }\n").unwrap();
        std::fs::write(
            wt.join("build.rs"),
            r#"fn main() {
            println!("cargo:rerun-if-env-changed=FIXTURE_READY");
            let Ok(ready) = std::env::var("FIXTURE_READY") else { return };
            let release = std::env::var("FIXTURE_RELEASE").unwrap();
            std::fs::write(ready, "ready").unwrap();
            let start = std::time::Instant::now();
            while !std::path::Path::new(&release).exists() {
                assert!(start.elapsed().as_secs() < 120, "fixture was not released");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }"#,
        )
        .unwrap();
        let ready = wt.join("ready");
        let release = wt.join("release");
        let mut cargo = tokio::process::Command::new("cargo");
        cargo.env_clear();
        for key in ["PATH", "HOME", "RUSTUP_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                cargo.env(key, value);
            }
        }
        cargo
            .current_dir(wt)
            .env("RUSTUP_TOOLCHAIN", "stable")
            .env("CARGO_HOME", f._temp.path().join("cargo-home"))
            .env("CARGO_INCREMENTAL", "1")
            .args(["build", "--offline", "--jobs", "1", "--target-dir"])
            .arg(wt.join("target"));
        // Compile before timing the active-build handshake. Under a parallel
        // gate, even this tiny fixture's first compiler startup can exceed the
        // readiness window; compilation speed is not what this test verifies.
        let warm = cargo.output().await.unwrap();
        assert!(
            warm.status.success(),
            "fixture compilation failed: {}",
            String::from_utf8_lossy(&warm.stderr)
        );
        let log_path = f._temp.path().join("live-cargo.stderr");
        let mut child = cargo
            .env("FIXTURE_READY", &ready)
            .env("FIXTURE_RELEASE", &release)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log_path).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let readiness = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !ready.exists() {
                if let Some(status) = child.try_wait().unwrap() {
                    return Err(format!("fixture exited before readiness: {status}"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok(())
        })
        .await;
        let arrived = matches!(readiness, Ok(Ok(())));
        let report = if arrived {
            Some(sweep(&f.home, &Fx::disk(), Fx::now()).await)
        } else {
            None
        };
        // Release and join our build before making assertions, including on failure.
        std::fs::write(release, "release").unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait()).await;
        if status.is_err() {
            child.kill().await.unwrap();
            child.wait().await.unwrap();
        }
        assert!(
            arrived,
            "fixture build never reached its active build script: {readiness:?}\n{}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        assert!(
            status.unwrap().unwrap().success(),
            "fixture build failed: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        let report = report.unwrap();
        assert_eq!(report.freed_bytes, 0, "{report:?}");
        assert!(
            report
                .retained
                .iter()
                .any(|s| s.contains("Cargo build is active")),
            "{report:?}"
        );
        assert!(wt.join("target/debug/output").is_file());
        assert!(crate::disk::dir_size(&wt.join("target/debug/incremental")) > 0);
    }

    #[cfg(unix)]
    #[test]
    fn allocated_accounting_deduplicates_links_and_requires_the_last_link() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let outside = temp.path().join("retained-input");
        std::fs::write(&a, vec![1; 65536]).unwrap();
        std::fs::hard_link(&a, &b).unwrap();
        std::fs::hard_link(&a, &outside).unwrap();
        let mut seen = Default::default();
        let blocks = allocated_inventory(&temp.path().canonicalize().unwrap(), &mut seen).unwrap();
        assert!(blocks > 0);
        let mut accounting = ReleasedAllocation::of(&[a.clone(), b.clone()]).unwrap();
        std::fs::remove_file(&a).unwrap();
        assert_eq!(accounting.unlinked(&a), Some(0));
        std::fs::remove_file(&b).unwrap();
        assert_eq!(accounting.unlinked(&b), Some(0));
        assert_eq!(std::fs::read(&outside).unwrap().len(), 65536);
        let mut accounting = ReleasedAllocation::of(std::slice::from_ref(&outside)).unwrap();
        std::fs::remove_file(&outside).unwrap();
        assert_eq!(accounting.unlinked(&outside), Some(blocks));
    }

    #[cfg(unix)]
    #[test]
    fn relative_admission_home_matches_absolute_home_and_retains_symlink_checks() {
        let temp = tempfile::tempdir_in(".").unwrap();
        let home = PathBuf::from(temp.path().file_name().unwrap()).join("state");
        assert!(home.is_relative());
        let paths = vec![std::env::current_dir().unwrap()];
        let gib = 1024 * 1024 * 1024;
        let reservation = OutputLease::admit_with(&home, &paths, 8 * gib, |_| Ok(9 * gib)).unwrap();
        assert!(
            OutputLease::admit_with(&home.canonicalize().unwrap(), &paths, 8 * gib, |_| Ok(
                9 * gib
            ))
            .is_err()
        );
        drop(reservation);
        assert!(OutputLease::admit_with(&home, &paths, 8 * gib, |_| Ok(9 * gib)).is_ok());
        let bad = temp.path().join("symlink-state");
        std::fs::create_dir(&bad).unwrap();
        std::os::unix::fs::symlink(
            home.canonicalize().unwrap().join("disk-admission"),
            bad.join("disk-admission"),
        )
        .unwrap();
        assert!(OutputLease::admit_with(&bad, &paths, 8 * gib, |_| Ok(9 * gib)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_disk_admission_reserves_once_per_volume_until_release() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let paths = vec![home.clone(), home.join("not-created-yet")];
        let gib = 1024 * 1024 * 1024;
        let floor = 8 * gib;
        let first = OutputLease::admit_with(&home, &paths, floor, |_| Ok(floor + gib)).unwrap();
        assert_eq!(first.len(), 1);
        let blocked =
            OutputLease::admit_with(&home, &paths, floor, |_| Ok(floor + gib)).unwrap_err();
        assert!(blocked.to_string().contains("active writer reservations"));
        let second =
            OutputLease::admit_with(&home, &paths, floor, |_| Ok(floor + 2 * gib)).unwrap();
        drop(first);
        assert!(OutputLease::admit_with(&home, &paths, floor, |_| Ok(floor + gib)).is_err());
        drop(second);
        assert!(OutputLease::admit_with(&home, &paths, floor, |_| Ok(floor + gib)).is_ok());
        assert!(
            OutputLease::admit_with(&home, &paths, floor, |_| anyhow::bail!("unmeasured")).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn simultaneous_writers_cannot_spend_the_same_volume_headroom() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let barrier = std::sync::Barrier::new(2);
        let gib = 1024 * 1024 * 1024;
        let results = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        OutputLease::admit_with(&home, std::slice::from_ref(&home), 100, |_| {
                            Ok(100 + gib)
                        })
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|e| e.to_string().contains("active writer reservations"))
        );
    }

    #[tokio::test]
    async fn sweep_reclaims_real_cargo_outputs_and_preserves_source_and_unknown_files() {
        real_cargo_fixture(false).await;
    }

    #[tokio::test]
    async fn missing_tag_from_precreated_target_reclaims_only_real_cargo_artifacts() {
        real_cargo_fixture(true).await;
    }

    async fn real_cargo_fixture(precreate: bool) {
        let f = Fx::new().await;
        let state = f.run("ca00", true).await;
        let wt = &state.candidates[0].worktree;
        let source = "fn main() { println!(\"retained fixture\"); }\n";
        std::fs::create_dir(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/main.rs"), source).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"retained-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .unwrap();
        if precreate {
            std::fs::create_dir_all(wt.join("target/magi-tools")).unwrap();
        }
        let mut cargo = std::process::Command::new("cargo");
        cargo.env_clear();
        for key in ["PATH", "HOME", "RUSTUP_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                cargo.env(key, value);
            }
        }
        let build = cargo
            .current_dir(wt)
            .env("RUSTUP_TOOLCHAIN", "stable")
            .env("CARGO_HOME", f._temp.path().join("cargo-home"))
            .env("CARGO_INCREMENTAL", "0")
            .arg("--config")
            .arg(format!(
                "build.build-dir={}",
                serde_json::to_string(&wt.join("target")).unwrap()
            ))
            .args([
                "build",
                "--offline",
                "--release",
                "--jobs",
                "1",
                "--target-dir",
            ])
            .arg(wt.join("target"))
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        assert_eq!(wt.join("target/CACHEDIR.TAG").exists(), !precreate);
        if precreate {
            // Cargo leaves pre-existing target roots untagged on subsequent builds too.
            let again = cargo.output().unwrap();
            assert!(
                again.status.success(),
                "{}",
                String::from_utf8_lossy(&again.stderr)
            );
            assert!(!wt.join("target/CACHEDIR.TAG").exists());
            assert!(cargo_target(wt).await.is_err());
        }
        git::git(wt, &["add", "Cargo.toml", "Cargo.lock", "src"])
            .await
            .unwrap();
        git::git(wt, &["commit", "-m", "real Cargo fixture"])
            .await
            .unwrap();
        let artifact = wt.join("target/release/retained-fixture");
        assert!(artifact.is_file());
        let unknown = wt.join("target/release/operator-note");
        std::fs::write(&unknown, "keep this note\n").unwrap();

        let target = wt.join("target");
        let mut preserved = std::collections::BTreeMap::new();
        for relative in [
            "tmp/sentinel",
            "magi-tools/sentinel",
            "lifecycle-client-check.log",
        ] {
            let path = target.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("retain {relative}\n")).unwrap();
            preserved.insert(path.clone(), std::fs::read(path).unwrap());
        }
        // Even genuine compiler bytes and valid dep-info outside supported
        // profiles cannot expand the deletion inventory.
        let decoy = target.join("magi-tools/retained-fixture");
        std::fs::copy(&artifact, &decoy).unwrap();
        let dep_info = decoy.with_extension("d");
        std::fs::write(&dep_info, format!("{}: src/main.rs\n", decoy.display())).unwrap();
        for path in [
            decoy,
            dep_info,
            wt.join("Cargo.toml"),
            wt.join("Cargo.lock"),
        ] {
            preserved.insert(path.clone(), std::fs::read(path).unwrap());
        }
        let supported_dirs = profile_output_dirs(&target.join("release"), true).unwrap();
        let proven = declared_outputs(&target).unwrap();
        assert!(
            proven
                .iter()
                .any(|path| path.parent() != Some(target.join("release").as_path())),
            "both uplifted and unit/deps binary references must be proven: {proven:?}"
        );

        let out = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert!(out.freed_bytes > 0, "{out:?}");
        for (path, bytes) in preserved {
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{}", path.display());
        }
        assert!(!artifact.exists(), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(wt.join("src/main.rs")).unwrap(),
            source
        );
        assert_eq!(
            std::fs::read_to_string(&unknown).unwrap(),
            "keep this note\n"
        );
        assert_eq!(std::fs::read_to_string(wt.join("source")).unwrap(), "ca00");
        let ledger: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                f.home
                    .join("runs")
                    .join(&state.id)
                    .join("build-outputs.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            ledger["identity"],
            if precreate {
                "rustc-profile-dep-info"
            } else {
                "cache-tag"
            }
        );
        if precreate {
            assert!(!target.join("CACHEDIR.TAG").exists());
            assert!(verify_residue(&f.home, &state.id, wt).await.is_err());
        }
        let deleted: Vec<PathBuf> = serde_json::from_value(ledger["files"].clone()).unwrap();
        assert!(deleted.contains(&artifact));
        assert_eq!(deleted, proven);
        for path in deleted {
            assert!(!path.exists());
            assert!(supported_dirs.contains(&path.parent().unwrap().to_owned()));
        }
        assert!(!residue_proof(&f.home, &state.id, wt).unwrap().exists());
    }

    fn missing_tag_target(wt: &Path) {
        Fx::target(wt);
        let target = wt.join("target");
        std::fs::remove_file(target.join("CACHEDIR.TAG")).unwrap();
        for subdir in ["debug/deps", "debug/.fingerprint"] {
            std::fs::create_dir_all(target.join(subdir)).unwrap();
        }
        std::fs::write(target.join(".rustc_info.json"),
            r#"{"rustc_fingerprint":123,"outputs":{"1":{"success":true,"stdout":"rustc fixture","stderr":""}},"successes":{}}"#).unwrap();
    }

    // Synthetic v2 coverage runs even on toolchains that still produce v1.
    fn v2_target(wt: &Path) -> PathBuf {
        missing_tag_target(wt);
        let target = wt.join("target");
        std::fs::remove_dir(target.join("debug/deps")).unwrap();
        std::fs::remove_dir(target.join("debug/.fingerprint")).unwrap();
        let unit = target.join("debug/build/fixture/0123456789abcdef");
        let out = unit.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir(unit.join("fingerprint")).unwrap();
        for name in [
            "lib-fixture",
            "lib-fixture.json",
            "dep-lib-fixture",
            "invoked.timestamp",
        ] {
            std::fs::write(unit.join("fingerprint").join(name), "fixture").unwrap();
        }
        let artifact = out.join("libfixture-0123456789abcdef.rlib");
        std::fs::copy(target.join("debug/output"), &artifact).unwrap();
        std::fs::write(
            out.join("fixture-0123456789abcdef.d"),
            format!("{}: source\n", artifact.display()),
        )
        .unwrap();
        artifact
    }

    #[tokio::test]
    async fn missing_tag_v2_proves_only_compiler_units_and_preserves_retained_links() {
        let f = Fx::new().await;
        let state = f.run("bc01", true).await;
        let wt = &state.candidates[0].worktree;
        let artifact = v2_target(wt);
        let unknown = artifact.parent().unwrap().join("operator-note");
        std::fs::write(&unknown, "retain").unwrap();
        // A valid binary and dep-info in arbitrary build-script output are not authority.
        let script_unit = wt.join("target/debug/build/fixture/fedcba9876543210");
        let script = script_unit.join("out");
        std::fs::create_dir_all(&script).unwrap();
        std::fs::create_dir(script_unit.join("fingerprint")).unwrap();
        std::fs::write(script_unit.join("fingerprint/invoked.timestamp"), "fixture").unwrap();
        let decoy = script.join("libfixture-fedcba9876543210.rlib");
        std::fs::copy(&artifact, &decoy).unwrap();
        std::fs::write(
            script.join("fixture-fedcba9876543210.d"),
            format!("{}: source\n", decoy.display()),
        )
        .unwrap();
        // Cargo uplifts the same inode from the unit into the profile root.
        let uplift = wt.join("target/debug/libfixture.rlib");
        std::fs::hard_link(&artifact, &uplift).unwrap();
        std::fs::write(
            wt.join("target/debug/fixture.d"),
            format!("{}: source\n", uplift.display()),
        )
        .unwrap();
        let bin_unit = wt.join("target/debug/build/fixture/1111111111111111");
        std::fs::create_dir_all(bin_unit.join("out")).unwrap();
        std::fs::create_dir(bin_unit.join("fingerprint")).unwrap();
        for name in [
            "bin-fixture",
            "bin-fixture.json",
            "dep-bin-fixture",
            "invoked.timestamp",
        ] {
            std::fs::write(bin_unit.join("fingerprint").join(name), "fixture").unwrap();
        }
        let bin = bin_unit.join("out/fixture");
        std::fs::hard_link(wt.join("target/debug/output"), &bin).unwrap();
        std::fs::write(
            bin.with_extension("d"),
            format!("{}: source\n", bin.display()),
        )
        .unwrap();
        let retained = wt.join("retained-link");
        std::fs::hard_link(&artifact, &retained).unwrap();
        use std::os::unix::fs::MetadataExt;
        let expected_allocation = std::fs::metadata(wt.join("target/debug/output"))
            .unwrap()
            .blocks()
            * 512;
        let sentinel = std::fs::read(&retained).unwrap();
        let first = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(first.freed_bytes, 800, "{first:?}");
        assert!(!bin.exists());
        assert_eq!(
            first.estimated_released_allocated_bytes,
            Some(expected_allocation)
        );
        assert_eq!(std::fs::read(&retained).unwrap(), sentinel);
        assert!(retained.is_file());
        assert!(unknown.is_file());
        assert!(decoy.is_file());
        assert!(!artifact.exists());
        // Unlinking proven names must never remove the retained reference's bytes.
        assert_eq!(std::fs::metadata(retained).unwrap().len(), 200);
    }

    #[tokio::test]
    async fn missing_tag_v2_accepts_completed_build_script_run_units() {
        let f = Fx::new().await;
        let state = f.run("bc02", true).await;
        let wt = &state.candidates[0].worktree;
        let artifact = v2_target(wt);
        let run = wt.join("target/debug/build/fixture/2222222222222222");
        std::fs::create_dir_all(run.join("out")).unwrap();
        std::fs::create_dir(run.join("fingerprint")).unwrap();
        for name in [
            "run-build-script-build-script-build",
            "run-build-script-build-script-build.json",
        ] {
            std::fs::write(run.join("fingerprint").join(name), "fixture").unwrap();
        }
        // Build-script output carries no deletion authority.
        let decoy = run.join("out/libdecoy-2222222222222222.rlib");
        std::fs::copy(&artifact, &decoy).unwrap();
        std::fs::write(
            run.join("out/decoy-2222222222222222.d"),
            format!("{}: source\n", decoy.display()),
        )
        .unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert!(report.freed_bytes > 0, "{report:?}");
        assert!(!artifact.exists());
        assert!(decoy.is_file());
    }

    #[tokio::test]
    async fn missing_tag_v2_rejects_incomplete_mixed_and_symlinked_layouts() {
        for invalid in [
            "build",
            "deps",
            ".fingerprint",
            "out",
            "fingerprint",
            "hash",
            "mixed",
            "unit-fingerprint",
            "compiler-fingerprint",
            "absent-out",
        ] {
            let f = Fx::new().await;
            let state = f.run("bc02", true).await;
            let wt = &state.candidates[0].worktree;
            let artifact = v2_target(wt);
            let unit = artifact.parent().unwrap().parent().unwrap();
            let profile = wt.join("target/debug");
            match invalid {
                "mixed" => {
                    std::fs::create_dir(profile.join("deps")).unwrap();
                    std::fs::create_dir(profile.join(".fingerprint")).unwrap();
                }
                "hash" => {
                    std::fs::rename(unit, unit.with_file_name("unknown")).unwrap();
                }
                "unit-fingerprint" => {
                    std::fs::remove_file(unit.join("fingerprint/invoked.timestamp")).unwrap();
                }
                "compiler-fingerprint" => {
                    std::fs::remove_file(unit.join("fingerprint/lib-fixture.json")).unwrap();
                }
                "absent-out" => {
                    std::fs::remove_dir_all(unit.join("out")).unwrap();
                }
                name => {
                    let path = if matches!(name, "out" | "fingerprint") {
                        unit.join(name)
                    } else {
                        profile.join(name)
                    };
                    if path.exists() {
                        std::fs::remove_dir_all(&path).unwrap();
                    }
                    std::os::unix::fs::symlink(&f.repo, &path).unwrap();
                }
            }
            let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
            assert_eq!(report.freed_bytes, 0, "{invalid}: {report:?}");
            assert!(!report.retained.is_empty(), "{invalid}: {report:?}");
        }
    }

    #[tokio::test]
    async fn bookkeeping_flushes_do_not_extend_artifact_grace_including_legacy_records() {
        for legacy in [false, true] {
            let f = Fx::new().await;
            let mut state = f.run("ac00", true).await;
            Fx::target(&state.candidates[0].worktree);
            let original = state.updated_at;
            let now = Fx::now() + jiff::SignedDuration::from_secs(7200);
            if legacy {
                let path = f.home.join("runs").join(&state.id).join("run.json");
                let mut value = serde_json::to_value(&state).unwrap();
                value.as_object_mut().unwrap().remove("output_activity_at");
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
                state = RunState::load_under(&state.id, &f.home).unwrap();
                assert_eq!(state.output_activity(), original);
            }
            let age = state.output_activity();
            for offset in [30, 20, 10] {
                state
                    .save_under_at(&f.home, now - jiff::SignedDuration::from_secs(offset))
                    .unwrap();
                state = RunState::load_under(&state.id, &f.home).unwrap();
                assert_eq!(state.output_activity(), age);
            }
            let mut disk = Fx::disk();
            disk.fold_grace_secs = 3600;
            let report = sweep(&f.home, &disk, now).await;
            assert_eq!(report.freed_bytes, 200, "legacy={legacy}: {report:?}");
        }
    }

    #[tokio::test]
    async fn recent_execution_outputs_and_released_output_leases_retain_artifacts() {
        for evidence in [
            "execution",
            "artifact",
            "dep-info",
            "fingerprint",
            "lease",
            "future",
        ] {
            let f = Fx::new().await;
            let mut state = f.run("ac01", true).await;
            let wt = state.candidates[0].worktree.clone();
            missing_tag_target(&wt);
            let now = Fx::now() + jiff::SignedDuration::from_secs(7200);
            let recent = now - jiff::SignedDuration::from_secs(10);
            match evidence {
                "execution" => {
                    state.note_output_activity(recent);
                    state.save_under_at(&f.home, recent).unwrap();
                }
                "lease" => {
                    let lease = OutputLease::borrow(&wt).unwrap().unwrap();
                    let file = lease._file.try_clone().unwrap();
                    drop(lease);
                    file.set_modified(std::time::SystemTime::from(recent))
                        .unwrap();
                }
                _ => {
                    let relative = match evidence {
                        "dep-info" => "debug/output.d",
                        "fingerprint" => "debug/.fingerprint/invoked.timestamp",
                        _ => "debug/output",
                    };
                    let path = wt.join("target").join(relative);
                    let file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(path)
                        .unwrap();
                    let at = if evidence == "future" {
                        now + jiff::SignedDuration::from_secs(10)
                    } else {
                        recent
                    };
                    file.set_modified(std::time::SystemTime::from(at)).unwrap();
                }
            }
            let mut disk = Fx::disk();
            disk.fold_grace_secs = 3600;
            let report = sweep(&f.home, &disk, now).await;
            assert_eq!(report.freed_bytes, 0, "{evidence}: {report:?}");
            assert!(
                report.retained.iter().any(|s| s.contains("grace period")
                    || s.contains("execution activity after inventory")),
                "{evidence}: {report:?}"
            );
            let aged = now + jiff::SignedDuration::from_secs(7200);
            let report = sweep(&f.home, &disk, aged).await;
            assert_eq!(report.freed_bytes, 200, "{evidence}: {report:?}");
        }
    }

    #[tokio::test]
    async fn missing_tag_proof_preserves_locks_unknown_files_and_retirement_boundary() {
        let f = Fx::new().await;
        let state = f.run("ac02", true).await;
        let wt = &state.candidates[0].worktree;
        missing_tag_target(wt);
        let target = wt.join("target");
        let unknown = target.join("debug/notes");
        std::fs::write(&unknown, "operator data").unwrap();
        for lock in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
            let path = target.join("debug").join(lock);
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            crate::pr_head::try_lock_file(&file).unwrap();
            let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
            assert_eq!(report.freed_bytes, 0, "{lock}: {report:?}");
            assert!(
                report
                    .retained
                    .iter()
                    .any(|s| s.contains(path.to_str().unwrap())),
                "{report:?}"
            );
        }
        let owner = crate::pr_head::owner_lock(&f.home, &state.id).unwrap();
        assert_eq!(sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes, 0);
        drop(owner);
        let driver = crate::driver::maintenance(&f.home, &state.id).unwrap();
        assert_eq!(sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes, 0);
        drop(driver);
        let output = OutputLease::borrow(wt).unwrap().unwrap();
        assert_eq!(sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes, 0);
        drop(output);
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.freed_bytes, 200, "{report:?}");
        assert_eq!(std::fs::read_to_string(unknown).unwrap(), "operator data");
        assert!(!target.join("CACHEDIR.TAG").exists());
        assert!(!residue_proof(&f.home, &state.id, wt).unwrap().exists());
        assert!(verify_residue(&f.home, &state.id, wt).await.is_err());
        assert_eq!(sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes, 0);
    }

    #[tokio::test]
    async fn missing_tag_partial_reclamation_can_resume_without_adopting_residue() {
        let f = Fx::new().await;
        let state = f.run("ac04", true).await;
        let wt = &state.candidates[0].worktree;
        missing_tag_target(wt);
        let target = wt.join("target");
        let first = target.join("debug/output");
        let second = target.join("debug/second");
        std::fs::copy(&first, &second).unwrap();
        std::fs::write(
            second.with_extension("d"),
            format!("{}: source\n", second.display()),
        )
        .unwrap();
        let outputs = declared_outputs(&target).unwrap();
        assert_eq!(outputs.len(), 2);
        // Simulate interruption after durable adoption and one successful unlink.
        std::fs::write(f.home.join("runs").join(&state.id).join("build-outputs.json"),
            serde_json::to_vec(&serde_json::json!({"target":target,"files":outputs,"action":"reclaim-declared-outputs"})).unwrap()).unwrap();
        std::fs::remove_file(first).unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.freed_bytes, 200, "{report:?}");
        assert!(!second.exists());
        assert!(second.with_extension("d").exists());
        assert!(!residue_proof(&f.home, &state.id, wt).unwrap().exists());
    }

    #[tokio::test]
    async fn missing_tag_proof_rejects_invalid_identity_structure_and_symlinks() {
        for invalid in [
            "identity",
            "deps",
            "fingerprint",
            "lock",
            "dep-info",
            "tag-symlink",
            "bad-tag",
        ] {
            let f = Fx::new().await;
            let state = f.run("ac03", true).await;
            let wt = &state.candidates[0].worktree;
            missing_tag_target(wt);
            let target = wt.join("target");
            match invalid {
                "identity" => std::fs::write(
                    target.join(".rustc_info.json"),
                    r#"{"rustc_fingerprint":123}"#,
                )
                .unwrap(),
                "deps" | "fingerprint" => {
                    let path = target.join("debug").join(if invalid == "deps" {
                        "deps"
                    } else {
                        ".fingerprint"
                    });
                    std::fs::remove_dir(&path).unwrap();
                    std::os::unix::fs::symlink(&f.repo, path).unwrap();
                }
                "lock" => {
                    std::fs::remove_file(target.join("debug/.cargo-lock")).unwrap();
                }
                "dep-info" => std::fs::write(target.join("debug/output.d"), "").unwrap(),
                "tag-symlink" => {
                    std::os::unix::fs::symlink(target.join("absent"), target.join("CACHEDIR.TAG"))
                        .unwrap()
                }
                "bad-tag" => std::fs::write(target.join("CACHEDIR.TAG"), "unknown").unwrap(),
                _ => unreachable!(),
            }
            let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
            assert_eq!(report.freed_bytes, 0, "{invalid}: {report:?}");
            assert!(target.join("debug/output").is_file());
        }
    }

    #[tokio::test]
    async fn sweep_budget_oversized_target_makes_progress_without_a_second_deletion() {
        let f = Fx::new().await;
        let first = f.run("ba00", true).await;
        let second = f.run("ba01", true).await;
        for state in [&first, &second] {
            Fx::target(&state.candidates[0].worktree);
        }
        let mut disk = Fx::disk();
        disk.worktree_reclaim_bytes = 100;
        let out = sweep(&f.home, &disk, Fx::now()).await;
        assert_eq!(out.freed_bytes, 200, "{out:?}");
        assert!(
            out.retained
                .contains(&format!("{}: per-pass byte budget spent", second.id)),
            "{out:?}"
        );
        assert!(
            !first.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
        assert!(
            second.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
        assert_eq!(sweep(&f.home, &disk, Fx::now()).await.freed_bytes, 200);
    }

    #[tokio::test]
    async fn sweep_budget_pressure_prioritizes_largest_over_oldest() {
        let f = Fx::new().await;
        let small = f.run("bb00", true).await;
        let large = f.run("bb01", true).await;
        for state in [&small, &large] {
            Fx::target(&state.candidates[0].worktree);
        }
        let output = large.candidates[0].worktree.join("target/debug/output");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&output)
            .unwrap()
            .set_len(1000)
            .unwrap();
        let mut disk = Fx::disk();
        disk.worktree_reclaim_bytes = 300;
        disk.min_free_bytes = u64::MAX;
        // Pressure must trigger reclamation even when aggregate usage is below cap.
        disk.worktree_limit_bytes = u64::MAX;
        let out = sweep(&f.home, &disk, Fx::now()).await;
        assert_eq!(out.freed_bytes, 1000, "{out:?}");
        assert!(!output.exists());
        assert!(
            small.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
    }

    #[tokio::test]
    async fn sweep_budget_zero_remaining_and_inventory_are_distinct() {
        let f = Fx::new().await;
        let first = f.run("bc00", true).await;
        let second = f.run("bc01", true).await;
        for state in [&first, &second] {
            Fx::target(&state.candidates[0].worktree);
        }
        // Non-declared Cargo data must not determine whether outputs fit.
        std::fs::create_dir_all(
            first.candidates[0]
                .worktree
                .join("target/debug/incremental"),
        )
        .unwrap();
        std::fs::write(
            first.candidates[0]
                .worktree
                .join("target/debug/incremental/cache"),
            [0; 1000],
        )
        .unwrap();
        let mut disk = Fx::disk();
        disk.worktree_reclaim_bytes = 0;
        assert_eq!(sweep(&f.home, &disk, Fx::now()).await.freed_bytes, 0);
        disk.worktree_reclaim_bytes = 300;
        let out = sweep(&f.home, &disk, Fx::now()).await;
        assert_eq!(out.freed_bytes, 200, "{out:?}");
        assert!(
            !first.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
        assert!(
            second.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
        assert!(
            out.retained
                .iter()
                .any(|r| r.contains("declared outputs exceed"))
        );
    }

    #[tokio::test]
    async fn sweep_budget_exception_preserves_grace_liveness_and_locks() {
        let f = Fx::new().await;
        let mut protected = f.run("bd00", true).await;
        let later = f.run("bd01", true).await;
        for state in [&protected, &later] {
            Fx::target(&state.candidates[0].worktree);
        }
        let mut disk = Fx::disk();
        disk.worktree_reclaim_bytes = 100;
        disk.fold_grace_secs = 3600;
        let out = sweep(&f.home, &disk, Fx::now()).await;
        assert_eq!(out.freed_bytes, 0);
        assert!(
            out.retained
                .iter()
                .any(|r| r.contains("within grace period"))
        );
        disk.fold_grace_secs = 0;
        protected.driver_exited = false;
        protected.driver_pid = Some(std::process::id());
        protected.driver_started_at = crate::proc::process_started_at(std::process::id());
        protected
            .save_under_at(&f.home, protected.updated_at)
            .unwrap();
        let out = sweep(&f.home, &disk, Fx::now()).await;
        assert!(
            out.retained
                .iter()
                .any(|r| r.contains("run live or ownership unknown"))
        );
        assert_eq!(
            out.freed_bytes, 200,
            "live target must not consume exception: {out:?}"
        );
        let output = protected.candidates[0].worktree.join("target/debug/output");
        assert!(output.exists());
        protected.driver_exited = true;
        protected.save_under(&f.home).unwrap();
        let owner = crate::pr_head::owner_lock(&f.home, &protected.id).unwrap();
        assert_eq!(sweep(&f.home, &disk, Fx::now()).await.freed_bytes, 0);
        assert!(output.exists());
        drop(owner);
        let cargo = cargo_locks(&protected.candidates[0].worktree.join("target")).unwrap();
        assert_eq!(sweep(&f.home, &disk, Fx::now()).await.freed_bytes, 0);
        assert!(output.exists());
        drop(cargo);
        let lease = OutputLease::take(&protected.candidates[0].worktree, true).unwrap();
        assert_eq!(sweep(&f.home, &disk, Fx::now()).await.freed_bytes, 0);
        assert!(output.exists());
        drop(lease);
        assert_eq!(sweep(&f.home, &disk, Fx::now()).await.freed_bytes, 200);
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn sweep_skips_missing_run_paths_and_reclaims_other_runs() {
        for remove_root in [true, false] {
            let f = Fx::new().await;
            let stale = Fx::new().await;
            let mut missing = stale.run("aa00", false).await;
            Fx::target(&missing.candidates[0].worktree);
            missing.save_under(&f.home).unwrap();
            if remove_root {
                std::fs::remove_dir_all(missing.worktree_root()).unwrap();
                // An existing non-repository proves the absent root is checked
                // before querying Git, even when no registrations are cached.
                std::fs::remove_dir_all(stale.repo.join(".git")).unwrap();
            } else {
                std::fs::remove_dir_all(&stale.repo).unwrap();
            }
            let healthy = f.run("aa01", false).await;
            Fx::target(&healthy.candidates[0].worktree);

            let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(report.freed_bytes, 200, "{report:?}");
            assert!(
                !healthy.candidates[0]
                    .worktree
                    .join("target/debug/output")
                    .exists()
            );
            if !remove_root {
                // Without the repository, ownership cannot be validated.
                assert!(
                    missing.candidates[0]
                        .worktree
                        .join("target/debug/output")
                        .exists()
                );
            }
        }
    }

    #[tokio::test]
    async fn sweep_skips_missing_candidate_checkout_and_reclaims_later_runs() {
        let f = Fx::new().await;
        let mut missing = f.run("aa00", false).await;
        Fx::target(&missing.candidates[0].worktree);
        missing.repo = missing.candidates[0].worktree.clone();
        missing.save_under(&f.home).unwrap();
        std::fs::remove_dir_all(&missing.repo).unwrap();
        assert!(missing.worktree_root().is_dir());
        assert!(f.repo.is_dir());

        let healthy = f.run("aa01", false).await;
        Fx::target(&healthy.candidates[0].worktree);

        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert!(report.failures.is_empty(), "{report:?}");
        assert_eq!(report.freed_bytes, 200, "{report:?}");
        assert!(
            !healthy.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
    }

    #[tokio::test]
    async fn sweep_reports_invalid_repository_and_still_reclaims_later_runs() {
        let f = Fx::new().await;
        let broken = Fx::new().await;
        let mut invalid = broken.run("aa00", false).await;
        Fx::target(&invalid.candidates[0].worktree);
        invalid.save_under(&f.home).unwrap();
        std::fs::remove_dir_all(broken.repo.join(".git")).unwrap();
        let healthy = f.run("aa01", false).await;
        Fx::target(&healthy.candidates[0].worktree);

        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.failures.len(), 1, "{report:?}");
        assert!(report.failures[0].starts_with(&format!("{}:", invalid.id)));
        assert_eq!(report.freed_bytes, 200, "{report:?}");
        assert!(
            !healthy.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
        assert!(
            invalid.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
    }

    async fn reconciliation_plan(f: &Fx, task: &Task, source: &RunState) -> serde_json::Value {
        let path = reconcile::run(&f.home, &task.id, &source.id, None)
            .await
            .unwrap();
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reconciliation_standing_chat_applies_the_owners_request_without_a_phrase() {
        use crate::{
            chat_control::{Operation, dispatch},
            talk::{self, Talks},
        };
        let f = Fx::new().await;
        let source = f.run("ae00", true).await;
        let clean = f.run("ae01", false).await;
        let queue = crate::queue::Queue::at(f.home.join("queue"));
        let talks = Talks::at(f.home.join("talks"));
        let open_talk = |id: &str| -> talk::Talk {
            let mut conversation = serde_json::from_value(serde_json::json!({
                "schema":1,"id":id,"repo":f.repo,"agent":"worker","status":"open","turns":[],
                "created_at":Timestamp::now(),"updated_at":Timestamp::now(),
                "seat":crate::agent::SeatState::new("talk","worker",1)
            }))
            .unwrap();
            talks.put(&mut conversation).unwrap();
            conversation
        };
        let conversation = open_talk("talk-owner");
        let other = open_talk("talk-other");
        // A task this chat did not file: the owner's request covers it too.
        let task = f.task(&[source.clone(), clean.clone()]);
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let operation = |apply: Option<&str>| Operation::ReconcileRetention {
            task: task.id.clone(),
            authority: source.id.clone(),
            apply: apply.map(str::to_owned),
        };
        // A plan saved outside the chat is not applied through it.
        let legacy_path = reconcile::run(&f.home, &task.id, &source.id, None)
            .await
            .unwrap();
        let legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(legacy_path).unwrap()).unwrap();
        let legacy_id = legacy["id"].as_str().unwrap();
        assert!(
            dispatch(
                &queue,
                &f.home,
                &talks,
                &conversation.id,
                operation(Some(legacy_id))
            )
            .await
            .is_err()
        );
        let summary = dispatch(&queue, &f.home, &talks, &conversation.id, operation(None))
            .await
            .unwrap();
        assert!(summary.contains("Complete review summary"));
        assert!(summary.contains(&source.id) && summary.contains(&clean.id));
        let root = f.home.join("retention-reconciliations");
        let displayed: serde_json::Value =
            serde_json::from_str(summary.split_once('\n').unwrap().1).unwrap();
        let id = displayed["plan"].as_str().unwrap();
        let dir = root.join(id);
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("plan.json")).unwrap()).unwrap();
        assert_eq!(plan["version"], 2);
        assert!(clean.candidates[0].worktree.exists());
        assert!(!dir.join("approved.json").exists());
        // The plan stays bound to the conversation it was saved for.
        assert!(
            dispatch(&queue, &f.home, &talks, &other.id, operation(Some(id)))
                .await
                .is_err()
        );
        assert!(!dir.join("approved.json").exists());
        // No owner phrase or quote: the request itself applies the plan. An
        // interruption after the durable fence keeps the audit for a retry.
        let error = reconcile::interrupt_chat_after_fence(
            &f.home,
            &task,
            id,
            reconcile::ChatRequest {
                talks: &talks,
                conversation: &conversation.id,
            },
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("simulated interruption"),
            "{error:#}"
        );
        assert!(clean.candidates[0].worktree.exists());
        assert!(dir.join("approved.json").exists());
        dispatch(
            &queue,
            &f.home,
            &talks,
            &conversation.id,
            operation(Some(id)),
        )
        .await
        .unwrap();
        assert!(!clean.candidates[0].worktree.exists());
        assert!(source.candidates[0].worktree.exists());
        let audit: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("approved.json")).unwrap()).unwrap();
        assert_eq!(audit["version"], 2);
        assert_eq!(
            audit["owner"],
            serde_json::json!({"conversation": conversation.id})
        );
        dispatch(
            &queue,
            &f.home,
            &talks,
            &conversation.id,
            operation(Some(id)),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reconciliation_proves_unmeasured_startup_candidates_empty() {
        let f = Fx::new().await;
        let source = f.run("af00", true).await;
        let mut clean = f.run("af01", false).await;
        let mut dirty = f.run("af02", false).await;
        for state in [&mut clean, &mut dirty] {
            state.status = RunStatus::Prep;
            state.prep_complete = Some(false);
            state.candidates[0].empty = false;
            state.save_under(&f.home).unwrap();
        }
        std::fs::write(dirty.candidates[0].worktree.join("source"), "unsaved work").unwrap();
        let task = f.task(&[source.clone(), clean.clone(), dirty.clone()]);
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let plan = reconciliation_plan(&f, &task, &source).await;
        let selected: Vec<_> = plan["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["selected"] == true)
            .collect();
        assert_eq!(selected.len(), 1, "{plan:#}");
        assert_eq!(selected[0]["run"], clean.id);
        reconcile::run(&f.home, &task.id, &source.id, plan["id"].as_str())
            .await
            .unwrap();
        assert!(!clean.candidates[0].worktree.exists());
        assert!(dirty.candidates[0].worktree.exists());
        assert!(
            RunState::load_under(&dirty.id, &f.home)
                .unwrap()
                .retired_to
                .is_none()
        );
    }

    #[tokio::test]
    async fn reconciliation_blanket_retention_selected_source_and_repeated_batches() {
        let f = Fx::new().await;
        let mut source = f.run("aa00", true).await;
        let alternative = f.run("aa01", true).await;
        let head = git::rev_parse(&f.repo, &source.candidates[0].branch)
            .await
            .unwrap();
        source.reviewed_commits = Some(vec![head.clone()]);
        source.save_under(&f.home).unwrap();
        let mut runs = vec![source.clone(), alternative.clone()];
        for i in 2..14 {
            runs.push(f.run(&format!("aa{i:02}"), false).await);
        }
        let mut task = f.task(&runs);
        task.pr_recovery = Some(crate::pr_recovery::Selection {
            url: "https://example.invalid/pull/1".into(),
            branch: "selected".into(),
            head: head.clone(),
            source_head: Some(head.clone()),
            source_run: Some(source.id.clone()),
            source_proof: None,
            disposition: Some(crate::pr_recovery::Disposition::Land),
            instruction: task.instruction.clone(),
            approved_at: Timestamp::now(),
        });
        source.pr_recovery = task.pr_recovery.clone();
        source.save_under(&f.home).unwrap();
        runs[0] = source.clone();
        crate::queue::Queue::at(f.home.join("queue"))
            .put(&mut task)
            .unwrap();
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let task_before = serde_json::to_value(&task).unwrap();
        let mut report = Report::default();
        retire_empty(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(
            report.retired, 0,
            "automatic cleanup must still refuse blanket markers"
        );
        assert!(
            report
                .retained
                .iter()
                .any(|reason| reason.contains("explicitly retained"))
        );
        let mut total = 0;
        for expected in [5, 5, 2, 0] {
            let plan = reconciliation_plan(&f, &task, &source).await;
            let selected: Vec<_> = plan["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["selected"] == true)
                .collect();
            assert_eq!(selected.len(), expected, "{plan:#}");
            for e in &selected {
                assert!(
                    RunState::load_under(e["run"].as_str().unwrap(), &f.home)
                        .unwrap()
                        .retired_to
                        .is_none()
                );
            }
            let operation = plan["id"].as_str().unwrap();
            for _ in 0..2 {
                reconcile::run(&f.home, &task.id, &source.id, Some(operation))
                    .await
                    .unwrap();
            }
            total += expected;
        }
        assert_eq!(total, 12);
        for run in &runs {
            let dir = f.home.join("runs").join(&run.id);
            assert!(dir.join("run.json").exists());
            assert!(dir.join("retain-recovery-sources").exists());
            assert!(
                git::branch_exists(&f.repo, &run.candidates[0].branch)
                    .await
                    .unwrap()
            );
        }
        assert!(source.candidates[0].worktree.exists());
        assert!(alternative.candidates[0].worktree.exists());
        assert_eq!(
            git::rev_parse(&f.repo, &source.candidates[0].branch)
                .await
                .unwrap(),
            head
        );
        assert_eq!(
            serde_json::to_value(
                crate::queue::Queue::at(f.home.join("queue"))
                    .get(&task.id)
                    .unwrap()
            )
            .unwrap(),
            task_before
        );
        assert_eq!(
            runs.iter()
                .filter(|r| RunState::load_under(&r.id, &f.home)
                    .unwrap()
                    .retired_to
                    .is_none())
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn reconciliation_refuses_live_dirty_ignored_unknown_and_git_checkpoints() {
        let f = Fx::new().await;
        let source = f.run("ab00", true).await;
        let mut runs = vec![source.clone()];
        for i in 1..9 {
            runs.push(f.run(&format!("ab{i:02}"), false).await);
        }
        std::fs::write(runs[1].candidates[0].worktree.join("source"), "dirty").unwrap();
        std::fs::write(runs[2].candidates[0].worktree.join("untracked"), "source").unwrap();
        std::fs::create_dir(runs[3].candidates[0].worktree.join("scratch")).unwrap();
        std::fs::write(
            runs[3].candidates[0].worktree.join("scratch/source"),
            "ignored source",
        )
        .unwrap();
        std::fs::write(
            runs[4].worktree_root().join("unknown"),
            "unregistered source",
        )
        .unwrap();
        let checkpoint = git::git(
            &runs[5].candidates[0].worktree,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "sequencer",
            ],
        )
        .await
        .unwrap();
        std::fs::create_dir(checkpoint.trim()).unwrap();
        runs[6].driver_exited = false;
        runs[6].driver_pid = Some(std::process::id());
        runs[6].driver_started_at = crate::proc::process_started_at(std::process::id());
        runs[6].save_under(&f.home).unwrap();
        runs[7].driver_exited = false;
        runs[7].driver_pid = None;
        runs[7].driver_claimed = None; // Legacy record without first-claim evidence.
        runs[7].save_under(&f.home).unwrap();
        assert_eq!(runs[7].liveness_under(&f.home, false), Liveness::Unknown);

        let hidden = &runs[8].candidates[0].worktree;
        git::git(hidden, &["update-index", "--assume-unchanged", "source"])
            .await
            .unwrap();
        std::fs::write(hidden.join("source"), "hidden dirty implementation").unwrap();
        assert!(
            git::git(hidden, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty()
        );
        let task = f.task(&runs);
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let plan = reconciliation_plan(&f, &task, &source).await;
        assert!(
            plan["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["selected"] == false),
            "{plan:#}"
        );
        for run in &runs {
            assert!(run.candidates[0].worktree.exists());
        }
    }

    #[tokio::test]
    async fn reconciliation_stale_plans_and_changed_markers_fail_closed() {
        let f = Fx::new().await;
        let source = f.run("ac00", true).await;
        let empty = f.run("ac01", false).await;
        let task = f.task(&[source.clone(), empty.clone()]);
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let plan = reconciliation_plan(&f, &task, &source).await;
        std::fs::write(
            f.home.join("runs").join(&source.id).join("new-report"),
            "new review checkpoint",
        )
        .unwrap();
        assert!(
            reconcile::run(&f.home, &task.id, &source.id, plan["id"].as_str())
                .await
                .is_err()
        );
        assert!(empty.candidates[0].worktree.exists());
        assert!(
            RunState::load_under(&empty.id, &f.home)
                .unwrap()
                .retired_to
                .is_none()
        );
        let marker = f
            .home
            .join("runs")
            .join(&empty.id)
            .join("retain-recovery-sources");
        std::fs::write(&marker, "different owner").unwrap();
        let plan = reconciliation_plan(&f, &task, &source).await;
        assert!(
            plan["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["selected"] == false)
        );
    }

    #[tokio::test]
    async fn reconciliation_recovers_interrupted_state_staging_in_a_batch() {
        for partial in [false, true] {
            let f = Fx::new().await;
            let source = f.run("ab10", true).await;
            let first = f.run("ab11", false).await;
            let second = f.run("ab12", false).await;
            let task = f.task(&[source.clone(), first.clone(), second.clone()]);
            crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
            let plan = reconciliation_plan(&f, &task, &source).await;
            let id = plan["id"].as_str().unwrap();
            let staging = f.home.join("runs").join(&second.id).join("run.json.tmp");
            // Matching bytes without this operation's fence are not trusted.
            std::fs::write(&staging, b"").unwrap();
            assert!(
                reconcile::run(&f.home, &task.id, &source.id, Some(id))
                    .await
                    .is_err()
            );
            assert!(staging.exists());
            std::fs::remove_file(&staging).unwrap();
            let error = reconcile::interrupt_during_state_save(&f.home, &task, id, partial)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("before run state rename"),
                "{error:#}"
            );
            assert!(
                RunState::load_under(&first.id, &f.home)
                    .unwrap()
                    .retired_to
                    .is_some()
            );
            assert!(
                RunState::load_under(&second.id, &f.home)
                    .unwrap()
                    .retired_to
                    .is_none()
            );
            assert!(first.candidates[0].worktree.exists());
            assert!(second.candidates[0].worktree.exists());
            let interrupted = std::fs::read(&staging).unwrap();
            // An unrelated file must survive a refused retry unchanged.
            std::fs::write(&staging, b"unrelated recovery source").unwrap();
            assert!(
                reconcile::run(&f.home, &task.id, &source.id, Some(id))
                    .await
                    .is_err()
            );
            assert_eq!(
                std::fs::read(&staging).unwrap(),
                b"unrelated recovery source"
            );
            std::fs::write(&staging, interrupted).unwrap();
            reconcile::run(&f.home, &task.id, &source.id, Some(id))
                .await
                .unwrap();
            assert!(!staging.exists());
            assert!(!first.candidates[0].worktree.exists());
            assert!(!second.candidates[0].worktree.exists());
            assert_eq!(
                RunState::load_under(&second.id, &f.home)
                    .unwrap()
                    .retired_to,
                Some(source.id)
            );
        }
    }

    #[tokio::test]
    async fn reconciliation_resumes_only_its_exact_durable_fence() {
        let f = Fx::new().await;
        let source = f.run("ad00", true).await;
        let empty = f.run("ad01", false).await;
        let task = f.task(&[source.clone(), empty.clone()]);
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let plan = reconciliation_plan(&f, &task, &source).await;
        let id = plan["id"].as_str().unwrap();
        assert!(
            reconcile::interrupt_after_fence(&f.home, &task, id)
                .await
                .is_err()
        );
        assert!(empty.candidates[0].worktree.exists());
        assert_eq!(
            RunState::load_under(&empty.id, &f.home)
                .unwrap()
                .retired_to
                .as_ref(),
            Some(&source.id)
        );
        reconcile::run(&f.home, &task.id, &source.id, Some(id))
            .await
            .unwrap();
        assert!(!empty.candidates[0].worktree.exists());
        // Model an interrupted removal: restore precisely the approved base worktree.
        git::worktree_add_branch(
            &f.repo,
            &empty.candidates[0].worktree,
            "restore-empty",
            &f.base,
        )
        .await
        .unwrap();
        // Recreated Git administration differs from approval and is refused.
        assert!(
            reconcile::run(&f.home, &task.id, &source.id, Some(id))
                .await
                .is_err()
        );
        assert!(empty.candidates[0].worktree.exists());
        let mut changed = RunState::load_under(&empty.id, &f.home).unwrap();
        changed.prep_failure = Some("new owner edit".into());
        changed.save_under(&f.home).unwrap();
        assert!(
            reconcile::run(&f.home, &task.id, &source.id, Some(id))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reconciliation_checks_prep_intent_reflogs_and_unknown_run_fields() {
        let f = Fx::new().await;
        let source = f.run("ae00", true).await;
        let mut prep = f.run("ae01", false).await;
        let candidate = prep.candidates.pop().unwrap();
        prep.event("prep-intent", serde_json::json!({"branch":candidate.branch,"worktree":candidate.worktree,"start":f.base,"picks":[]}).to_string());
        prep.save_under(&f.home).unwrap();
        let reset = f.run("ae02", false).await;
        let wt = &reset.candidates[0].worktree;
        std::fs::write(wt.join("source"), "unique hidden in reflog").unwrap();
        git::git(wt, &["add", "."]).await.unwrap();
        git::git(wt, &["commit", "-m", "unique implementation"])
            .await
            .unwrap();
        git::git(wt, &["reset", "--hard", &f.base]).await.unwrap();
        let unknown = f.run("ae03", false).await;
        let unknown_path = f.home.join("runs").join(&unknown.id).join("run.json");
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&unknown_path).unwrap()).unwrap();
        raw["unknown_checkpoint"] = serde_json::json!({"source":"retain"});
        std::fs::write(unknown_path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let task = f.task(&[source.clone(), prep.clone(), reset.clone(), unknown.clone()]);
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let plan = reconciliation_plan(&f, &task, &source).await;
        let selected: Vec<_> = plan["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["selected"] == true)
            .collect();
        assert_eq!(selected.len(), 1, "{plan:#}");
        assert_eq!(selected[0]["run"], prep.id);
        reconcile::run(&f.home, &task.id, &source.id, plan["id"].as_str())
            .await
            .unwrap();
        assert!(!candidate.worktree.exists());
        assert!(reset.candidates[0].worktree.exists());
        assert!(unknown.candidates[0].worktree.exists());
    }

    #[tokio::test]
    async fn cap_reclaims_blocked_candidates_and_landing_reviewers_across_repositories() {
        let f = Fx::new().await;
        let mut state = f.run("aaaa", true).await;
        let candidate = state.candidates[0].worktree.clone();
        std::fs::write(candidate.join("source"), "uncommitted work").unwrap();
        Fx::target(&candidate);
        for seat in ["review-1", "review-2"] {
            let wt = state.worktree_root().join(seat);
            git::worktree_add_detached(&f.repo, &wt, &f.base)
                .await
                .unwrap();
            Fx::target(&wt);
        }
        state.status = RunStatus::Landing;
        state.save_under(&f.home).unwrap();
        let second = Fx::new().await;
        let other = second.run("bbbb", true).await;
        // A global store may contain repositories with different worktree roots.
        let mut imported = other.clone();
        imported.save_under(&f.home).unwrap();
        Fx::target(&other.candidates[0].worktree);
        let guard = OutputLease::borrow(&candidate).unwrap().unwrap();
        let out = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert!(
            candidate.join("target/debug/output").exists(),
            "active writer protected"
        );
        assert!(
            !state
                .worktree_root()
                .join("review-1/target/debug/output")
                .exists()
        );
        assert!(
            !state
                .worktree_root()
                .join("review-2/target/debug/output")
                .exists()
        );
        assert!(
            !other.candidates[0]
                .worktree
                .join("target/debug/output")
                .exists()
        );
        assert!(out.freed_bytes >= 600);
        drop(guard);
        let out = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert!(out.freed_bytes >= 200);
        assert_eq!(
            std::fs::read_to_string(candidate.join("source")).unwrap(),
            "uncommitted work"
        );
        assert!(
            git::branch_exists(&f.repo, &state.candidates[0].branch)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn dep_info_reclamation_preserves_unmanifested_nested_files() {
        let f = Fx::new().await;
        let state = f.run("cc01", false).await;
        let wt = &state.candidates[0].worktree;
        Fx::target(wt);
        for dir in ["debug", "release", "debug/deps", "debug/build/script/out"] {
            let dir = wt.join("target").join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("notes.txt"), "user evidence").unwrap();
        }
        // A dep-info dependency is an input, never deletion authority.
        let manifest = wt.join("target/debug/output.d");
        let output = wt.join("target/debug/output");
        std::fs::write(
            &manifest,
            format!(
                "{}: {}\n",
                output.display(),
                wt.join("target/debug/notes.txt").display()
            ),
        )
        .unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.freed_bytes, 200);
        assert!(!output.exists());
        for dir in ["debug", "release", "debug/deps", "debug/build/script/out"] {
            assert_eq!(
                std::fs::read_to_string(wt.join("target").join(dir).join("notes.txt")).unwrap(),
                "user evidence"
            );
        }
        // Metadata remains available for inspection and future safe rebuilds.
        assert!(manifest.exists());
        assert!(report.retained.iter().any(|s| s.contains("unmanifested")));
        std::fs::write(&output, "user note replacing an old binary").unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.freed_bytes, 0);
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "user note replacing an old binary"
        );
    }

    #[tokio::test]
    async fn measurement_cache_expires_and_disabled_eviction_does_not_walk_outputs() {
        let f = Fx::new().await;
        let state = f.run("cc02", false).await;
        let wt = &state.candidates[0].worktree;
        Fx::target(wt);
        let mut disk = Fx::disk();
        disk.worktree_limit_bytes = u64::MAX;
        let now = Fx::now();
        let first = sweep(&f.home, &disk, now).await;
        std::fs::write(wt.join("target/debug/output"), [1; 1000]).unwrap();
        assert_eq!(
            sweep(&f.home, &disk, now).await.measured_bytes,
            first.measured_bytes
        );
        assert!(
            sweep(&f.home, &disk, now + jiff::SignedDuration::from_secs(301))
                .await
                .measured_bytes
                > first.measured_bytes
        );
        disk.worktree_limit_bytes = 0;
        assert_eq!(sweep(&f.home, &disk, now).await.measured_bytes, 0);
    }

    #[tokio::test]
    async fn unfinished_source_fold_reports_its_retention_reason() {
        let f = Fx::new().await;
        let mut source = f.run("cc03", true).await;
        let task = f.task(&[source.clone()]);
        let error = crate::graph::fold_run(&mut source, true, &f.home)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&task.id));
        assert!(source.candidates[0].worktree.exists());
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        let error = crate::graph::fold_run(&mut source, true, &f.home)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("explicitly retained"));
    }

    #[tokio::test]
    async fn confirmed_merged_source_folds_while_task_is_held_but_retention_still_wins() {
        let f = Fx::new().await;
        let mut source = f.run("cc04", true).await;
        let mut task = f.task(&[source.clone()]);
        task.hold_manual(Some("queue settlement pending".into()));
        let queue = crate::queue::Queue::at(f.home.join("queue"));
        queue.put(&mut task).unwrap();
        source.merge = Some(crate::run::MergeOutcome {
            mode: crate::config::MergeMode::Pr,
            ok: true,
            detail: "confirmed merged".into(),
            empty: false,
        });
        // Publication success and terminal labels alone are insufficient.
        for status in [RunStatus::Ready, RunStatus::Failed, RunStatus::Superseded] {
            source.status = status;
            assert!(
                crate::graph::fold_run(&mut source, true, &f.home)
                    .await
                    .is_err()
            );
            assert!(source.candidates[0].worktree.exists());
        }
        git::git(
            &f.repo,
            &["merge", "--ff-only", &source.candidates[0].branch],
        )
        .await
        .unwrap();
        git::git(&f.repo, &["update-ref", "refs/remotes/origin/main", "HEAD"])
            .await
            .unwrap();
        source.status = RunStatus::Merged;
        source.save_under(&f.home).unwrap();
        assert!(crate::pr_recovery::source_retention_reason(&f.home, &source).is_none());
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        assert!(
            crate::graph::fold_run(&mut source, true, &f.home)
                .await
                .unwrap_err()
                .to_string()
                .contains("explicitly retained")
        );
        // The fixture's explicit marker is removed only to exercise ordinary
        // confirmed-merge cleanup; no production cleanup removes such markers.
        std::fs::remove_file(
            f.home
                .join("runs")
                .join(&source.id)
                .join("retain-recovery-sources"),
        )
        .unwrap();
        let (folded, unreadable) = crate::clean::fold_due(
            &f.home.join("runs"),
            &f.home,
            &f.bay,
            &Fx::disk(),
            Fx::now(),
        )
        .await
        .unwrap();
        assert_eq!((folded, unreadable), (1, 0));
        assert!(!source.candidates[0].worktree.exists());
        let evidence = f
            .home
            .join("runs")
            .join(&source.id)
            .join("artifacts/fold-evidence");
        let names: Vec<_> = std::fs::read_dir(evidence)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("manifest.json")]);
        assert_eq!(
            queue.get(&task.id).unwrap().status,
            crate::queue::TaskStatus::Held
        );
    }

    #[tokio::test]
    async fn merged_fold_skips_proven_squash_and_archives_unpublished_sources() {
        for variant in ["exact", "partial", "checkpoint", "loser", "reviewer"] {
            let f = Fx::new().await;
            let mut state = f.run("cc06", true).await;
            let candidate = state.candidates[0].clone();
            let intermediate = git::rev_parse(&candidate.worktree, "HEAD").await.unwrap();
            std::fs::write(candidate.worktree.join("second"), [0, 1, 255, 42]).unwrap();
            git::rescue_commit(&candidate.worktree, "second candidate change")
                .await
                .unwrap();
            let head = git::rev_parse(&candidate.worktree, "HEAD").await.unwrap();
            state.tally = Some(crate::run::Tally {
                first_choice: Default::default(),
                borda: Default::default(),
                winner: 'A',
                rankings: 0,
                unanimous_initial: true,
                deliberated: false,
                changed_votes: 0,
                unanimous_final: true,
                tie_break: None,
                judges: 0,
                present: 0,
                quorum: 0,
                met_quorum: true,
                uncontested: Some("fixture".into()),
            });
            // A reviewer pinned to an intermediate candidate commit must also
            // be reclaimable, although neither commit ID lands in the squash.
            let reviewer = state.worktree_root().join("review-1");
            git::worktree_add_detached(&f.repo, &reviewer, &intermediate)
                .await
                .unwrap();
            // Concurrent base changes and later base advances prevent a mere
            // equality check against the current base tree from passing.
            std::fs::write(f.repo.join("unrelated-before"), "base advance").unwrap();
            git::rescue_commit(&f.repo, "advance before squash")
                .await
                .unwrap();
            let published = if variant == "partial" {
                &intermediate
            } else {
                &head
            };
            git::git(&f.repo, &["merge", "--squash", published])
                .await
                .unwrap();
            git::git(&f.repo, &["commit", "-m", "squash candidate"])
                .await
                .unwrap();
            let squash = git::rev_parse(&f.repo, "HEAD").await.unwrap();
            assert!(
                !git::git_raw(&f.repo, &["merge-base", "--is-ancestor", &head, &squash])
                    .await
                    .unwrap()
                    .ok()
            );
            std::fs::write(f.repo.join("unrelated-after"), "later base advance").unwrap();
            git::rescue_commit(&f.repo, "advance after squash")
                .await
                .unwrap();
            git::git(&f.repo, &["update-ref", "refs/remotes/origin/main", "HEAD"])
                .await
                .unwrap();
            if matches!(variant, "checkpoint" | "reviewer") {
                // A reset hides unique work from HEAD, but not its reflog.
                std::fs::write(reviewer.join("unpublished"), "reviewer checkpoint").unwrap();
                git::rescue_commit(&reviewer, "unpublished checkpoint")
                    .await
                    .unwrap();
                if variant == "checkpoint" {
                    git::git(&reviewer, &["reset", "--hard", &intermediate])
                        .await
                        .unwrap();
                }
            }
            if variant == "loser" {
                let mut loser = candidate.clone();
                loser.label = 'B';
                loser.branch = "magi/cc06/B".into();
                loser.worktree = state.worktree_root().join("cand-B");
                git::worktree_add_branch(&f.repo, &loser.worktree, &loser.branch, &f.base)
                    .await
                    .unwrap();
                std::fs::write(loser.worktree.join("unique"), "losing candidate").unwrap();
                git::rescue_commit(&loser.worktree, "independent loser")
                    .await
                    .unwrap();
                state.candidates.push(loser);
            }
            state.status = RunStatus::Merged;
            state.merge = Some(crate::run::MergeOutcome {
                mode: crate::config::MergeMode::Pr,
                ok: true,
                detail: "candidate A squash merged".into(),
                empty: false,
            });
            state.save_under(&f.home).unwrap();
            let review_head = git::rev_parse(&reviewer, "HEAD").await.unwrap();
            let result = crate::graph::fold_run(&mut state, true, &f.home).await;
            result.unwrap();
            assert!(
                !candidate.worktree.exists() && !reviewer.exists(),
                "{variant}"
            );
            assert!(
                !git::branch_exists(&f.repo, &candidate.branch)
                    .await
                    .unwrap()
            );
            let artifacts = f
                .home
                .join("runs")
                .join(&state.id)
                .join("artifacts/fold-evidence");
            assert_eq!(
                artifacts.join(format!("{head}.patch")).exists(),
                variant == "partial",
                "only a partially published winner needs evidence: {variant}"
            );
            assert_eq!(
                artifacts.join(format!("{intermediate}.patch")).exists(),
                variant == "partial",
                "squash proof also covers intermediate reviewer tips: {variant}"
            );
            let patches: Vec<_> = std::fs::read_dir(&artifacts)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "patch"))
                .collect();
            let expected = match variant {
                "partial" => 2,
                "loser" | "reviewer" => 1,
                _ => 0,
            };
            assert_eq!(patches.len(), expected, "{variant}");
            if variant == "loser" {
                assert!(
                    std::fs::read_to_string(&patches[0])
                        .unwrap()
                        .contains("losing candidate")
                );
            }
            assert!(artifacts.join("manifest.json").exists());
            if variant == "reviewer" {
                assert!(
                    std::fs::read_to_string(artifacts.join(format!("{review_head}.patch")))
                        .unwrap()
                        .contains("reviewer checkpoint")
                );
            }
            crate::graph::fold_run(&mut state, true, &f.home)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn merged_fold_missing_review_source_preserves_artifacts_and_retries() {
        for missing in [true, false] {
            let f = Fx::new().await;
            let mut state = f.run("cc05", true).await;
            // A fetched base exists, but these local review changes were not
            // incorporated by ancestry, so their recovery evidence is required.
            git::git(
                &f.repo,
                &["update-ref", "refs/remotes/origin/main", &f.base],
            )
            .await
            .unwrap();
            let candidate = state.candidates[0].clone();
            if !missing {
                std::fs::write(f.repo.join("base-advance"), "new base").unwrap();
                git::rescue_commit(&f.repo, "advance base before rebase")
                    .await
                    .unwrap();
                git::git(&candidate.worktree, &["rebase", "main"])
                    .await
                    .unwrap();
                // Match the runner's post-rebase synchronization, which
                // clears the merge backend's transient AUTO_MERGE checkpoint.
                git::sync_to_head(&candidate.worktree).await.unwrap();
            }
            let head = git::rev_parse(&candidate.worktree, "HEAD").await.unwrap();
            // Model review-only state: no implementation-time snapshot exists.
            state.reviewed_commits = Some(Vec::new());
            state.candidates[0].worktree = state.worktree_root().join("under-review");
            git::git(
                &f.repo,
                &[
                    "worktree",
                    "move",
                    candidate.worktree.to_str().unwrap(),
                    state.candidates[0].worktree.to_str().unwrap(),
                ],
            )
            .await
            .unwrap();
            let reviewer = state.worktree_root().join("review-1");
            git::worktree_add_detached(&f.repo, &reviewer, &head)
                .await
                .unwrap();
            // A reset/rebase can leave unpublished recovery history. It must
            // not keep any remaining owned worktree alive indefinitely.
            std::fs::write(reviewer.join("obsolete"), "old history").unwrap();
            git::rescue_commit(&reviewer, "old review history")
                .await
                .unwrap();
            git::git(&reviewer, &["reset", "--hard", &head])
                .await
                .unwrap();
            if missing {
                std::fs::remove_dir_all(&state.candidates[0].worktree).unwrap();
            }
            let run_dir = f.home.join("runs").join(&state.id);
            let artifacts = run_dir.join("artifacts");
            std::fs::create_dir_all(&artifacts).unwrap();
            std::fs::write(artifacts.join("review.json"), "review findings").unwrap();
            if missing {
                std::fs::write(artifacts.join("cand-A.patch"), "original snapshot").unwrap();
            }
            state.status = RunStatus::Merged;
            state.merge = Some(crate::run::MergeOutcome {
                mode: crate::config::MergeMode::Pr,
                ok: true,
                detail: "confirmed squash merge".into(),
                empty: false,
            });
            state.save_under(&f.home).unwrap();
            // Unexpected external paths remain protected, even if absent.
            let owned = state.candidates[0].worktree.clone();
            state.candidates[0].worktree = f.home.join("missing-external");
            assert!(
                crate::graph::fold_run(&mut state, true, &f.home)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("outside the inventoried")
            );
            state.candidates[0].worktree = owned;
            // A live output borrower blocks the entire preflight.
            let guard = OutputLease::borrow(&reviewer).unwrap().unwrap();
            assert!(
                crate::graph::fold_run(&mut state, true, &f.home)
                    .await
                    .is_err()
            );
            assert!(reviewer.exists());
            drop(guard);
            // Failure to persist evidence must not cause partial removal.
            let evidence = artifacts.join("fold-evidence");
            std::fs::write(&evidence, "blocked").unwrap();
            assert!(
                crate::graph::fold_run(&mut state, true, &f.home)
                    .await
                    .is_err()
            );
            assert!(reviewer.exists());
            std::fs::remove_file(&evidence).unwrap();
            std::fs::create_dir_all(&evidence).unwrap();
            let oversized = evidence.join("capacity-fixture");
            std::fs::File::create(&oversized)
                .unwrap()
                .set_len(64 * 1024 * 1024)
                .unwrap();
            assert!(
                crate::graph::fold_run(&mut state, true, &f.home)
                    .await
                    .is_err()
            );
            assert!(reviewer.exists());
            std::fs::remove_file(oversized).unwrap();
            // Ignored recovery files and unfinished Git operations still hold
            // all sources, even for a confirmed merge.
            std::fs::create_dir_all(reviewer.join("scratch")).unwrap();
            std::fs::write(reviewer.join("scratch/recovery"), "keep").unwrap();
            assert!(
                crate::graph::fold_run(&mut state, true, &f.home)
                    .await
                    .is_err()
            );
            std::fs::remove_dir_all(reviewer.join("scratch")).unwrap();
            let checkpoint = git::git(&reviewer, &["rev-parse", "--git-path", "CHERRY_PICK_HEAD"])
                .await
                .unwrap();
            std::fs::write(&checkpoint, &head).unwrap();
            assert!(
                crate::graph::fold_run(&mut state, true, &f.home)
                    .await
                    .is_err()
            );
            std::fs::remove_file(checkpoint).unwrap();
            let (folded, unreadable) = crate::clean::fold_due(
                &f.home.join("runs"),
                &f.home,
                &f.bay,
                &Fx::disk(),
                Fx::now(),
            )
            .await
            .unwrap();
            if folded != 1 {
                panic!(
                    "normal cleanup failed: {:#}",
                    crate::graph::fold_run(&mut state, true, &f.home)
                        .await
                        .unwrap_err()
                );
            }
            assert_eq!((folded, unreadable), (1, 0));
            assert!(!reviewer.exists() && !state.candidates[0].worktree.exists());
            state = RunState::load_under(&state.id, &f.home).unwrap();
            assert!(state.candidates[0].folded);
            let manifest = std::fs::read(evidence.join("manifest.json")).unwrap();
            crate::graph::fold_run(&mut state, true, &f.home)
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(evidence.join("manifest.json")).unwrap(),
                manifest
            );
            git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
                .await
                .unwrap();
            git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
            assert!(
                !git::git_raw(&f.repo, &["cat-file", "-e", &head])
                    .await
                    .unwrap()
                    .ok()
            );
            // Restore the modified file into a fresh repository from artifact
            // bytes alone; no original commit or blob is consulted.
            let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
            let old_blob = manifest[format!("head-{head}")]["base_blobs"][0]
                .as_str()
                .unwrap();
            let recovered = f.home.join("recovered");
            std::fs::create_dir_all(&recovered).unwrap();
            git::git(&recovered, &["init"]).await.unwrap();
            std::fs::write(
                recovered.join("source"),
                std::fs::read(evidence.join(format!("blob-{old_blob}"))).unwrap(),
            )
            .unwrap();
            git::git(
                &recovered,
                &[
                    "apply",
                    evidence.join(format!("{head}.patch")).to_str().unwrap(),
                ],
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(recovered.join("source")).unwrap(),
                "cc05"
            );
            assert!(
                !std::fs::read(evidence.join(format!("{head}.patch")))
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                std::fs::read_to_string(artifacts.join("review.json")).unwrap(),
                "review findings"
            );
            if missing {
                assert_eq!(
                    std::fs::read_to_string(artifacts.join("cand-A.patch")).unwrap(),
                    "original snapshot"
                );
            } else {
                assert!(!artifacts.join("cand-A.patch").exists());
            }
            assert!(run_dir.join("run.json").exists());
        }
    }

    #[tokio::test]
    async fn slow_retirement_proof_allows_task_updates_and_aborts_on_new_owner_pin() {
        use std::os::unix::fs::PermissionsExt as _;
        let f = Fx::new().await;
        let source = f.run("dd03", true).await;
        let empty = f.run("dd04", false).await;
        let task = f.task(&[source, empty.clone()]);
        let entered = f.repo.join("proof-entered");
        let release = f.repo.join("proof-release");
        let hook = f.repo.join("proof-monitor");
        std::fs::write(&hook, format!(
            "#!/bin/sh\n: > '{}'\nwhile ! test -f '{}'; do sleep 0.01; done\nprintf 'probe\\0'\n",
            entered.display(), release.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git::git(
            &f.repo,
            &["config", "core.fsmonitor", hook.to_str().unwrap()],
        )
        .await
        .unwrap();
        struct ReleaseProof(PathBuf);
        impl Drop for ReleaseProof {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.0, "continue");
            }
        }
        let _release_on_panic = ReleaseProof(release.clone());
        let home = f.home.clone();
        let sweep = tokio::spawn(async move {
            let mut report = Report::default();
            retire_empty(&home, &Fx::disk(), Fx::now(), &mut report).await;
            report
        });
        // These are deadlock watchdogs, not speed assertions. The proof stays
        // blocked until the task update completes, even on a loaded runner.
        let watchdog = std::time::Duration::from_secs(60);
        tokio::time::timeout(watchdog, async {
            while !entered.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Git retirement proof did not enter the blocking hook");
        let queue = crate::queue::Queue::at(f.home.join("queue"));
        let mut selected = queue.get(&task.id).unwrap();
        selected.resume_override = Some(crate::queue::OperatorResume {
            question_id: "during-slow-proof".into(),
            at: Fx::now(),
            conductor_rehold: None,
            forced: false,
            pinned_run: Some(empty.id.clone()),
        });
        let mut update = tokio::task::spawn_blocking(move || queue.put(&mut selected));
        // The proof is released only after the update returns, so correctness
        // is decided by ordering, not speed. The 60-second bound only detects
        // a writer blocked behind the proof; it is not a performance threshold.
        let updated_while_blocked = match tokio::time::timeout(watchdog, &mut update).await {
            Ok(result) => {
                result.unwrap().unwrap();
                true
            }
            Err(_) => {
                std::fs::write(&release, "continue").unwrap();
                update.await.unwrap().unwrap();
                false
            }
        };
        std::fs::write(release, "continue").unwrap();
        let report = tokio::time::timeout(watchdog, sweep)
            .await
            .expect("Git retirement proof did not finish after release")
            .unwrap();
        assert!(
            updated_while_blocked,
            "task writers must not wait behind Git retirement proof"
        );
        assert_eq!(report.retired, 0);
        assert!(report.retained.iter().any(|r| r.contains("task changed")));
        assert!(empty.candidates[0].worktree.exists());
        assert!(
            RunState::load_under(&empty.id, &f.home)
                .unwrap()
                .retired_to
                .is_none()
        );
    }

    #[tokio::test]
    async fn retirement_commit_rechecks_task_after_unlocked_proof_and_preserves_new_pin() {
        let f = Fx::new().await;
        let source = f.run("dd01", true).await;
        let mut empty = f.run("dd02", false).await;
        let expected = f.task(&[source.clone(), empty.clone()]);
        let queue = crate::queue::Queue::at(f.home.join("queue"));
        let _claim = queue.claim(&expected.id).unwrap();
        // The proof holds run/output leases, never the task write lock.
        let _owner = crate::pr_head::owner_lock(&f.home, &empty.id).unwrap();
        let _driver = crate::driver::maintenance(&f.home, &empty.id).unwrap();
        let _outputs = OutputLease::take(&empty.candidates[0].worktree, true).unwrap();
        let mut selected = queue.get(&expected.id).unwrap();
        selected.resume_override = Some(crate::queue::OperatorResume {
            question_id: "new-owner-decision".into(),
            at: Fx::now(),
            conductor_rehold: None,
            forced: false,
            pinned_run: Some(empty.id.clone()),
        });
        queue.put(&mut selected).unwrap();
        let error = commit_retirement(
            &f.home,
            &queue,
            &expected,
            &mut empty,
            &source.id,
            &expected.runs,
            Fx::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("task changed"));
        assert!(
            RunState::load_under(&empty.id, &f.home)
                .unwrap()
                .retired_to
                .is_none()
        );
        assert!(empty.candidates[0].worktree.exists());
        assert_eq!(
            queue
                .get(&expected.id)
                .unwrap()
                .resume_override
                .unwrap()
                .pinned_run,
            Some(empty.id.clone())
        );
        assert!(
            !f.home
                .join("recovery-lineages")
                .join(format!("{}.json", expected.id))
                .exists()
        );
        // Re-reading the new snapshot must not bypass its explicit pin either.
        assert!(
            commit_retirement(
                &f.home,
                &queue,
                &selected,
                &mut empty,
                &source.id,
                &selected.runs,
                Fx::now()
            )
            .unwrap_err()
            .to_string()
            .contains("selected this recovery source")
        );
    }

    #[tokio::test]
    async fn cargo_owners_and_driver_resumes_are_protected_with_retained_root_data() {
        let f = Fx::new().await;
        let state = f.run("cccc", false).await;
        let wt = &state.candidates[0].worktree;
        Fx::target(wt);
        std::fs::write(wt.join("target/notes"), "user file").unwrap();
        std::os::unix::fs::symlink(f.repo.join("source"), wt.join("target/debug/link")).unwrap();
        assert!(
            !declared_outputs(&wt.join("target"))
                .unwrap()
                .contains(&wt.join("target/debug/link"))
        );
        std::fs::remove_file(wt.join("target/debug/link")).unwrap();
        let cargo = cargo_locks(&wt.join("target")).unwrap();
        let out = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(out.freed_bytes, 0);
        assert!(out.retained.iter().any(|s| s.contains("Cargo build")));
        drop(cargo);
        let driver = crate::driver::maintenance(&f.home, &state.id).unwrap();
        let out = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(out.freed_bytes, 0);
        drop(driver);
        let exclusive = OutputLease::take(wt, true).unwrap();
        assert!(
            OutputLease::borrow(wt).is_err(),
            "resume cannot start a writer during cleanup"
        );
        drop(exclusive);
        assert!(sweep(&f.home, &Fx::disk(), Fx::now()).await.freed_bytes > 0);
        assert_eq!(
            std::fs::read(wt.join("target/notes")).unwrap(),
            b"user file"
        );
    }

    #[tokio::test]
    async fn root_leftovers_allow_eviction_but_keep_empty_attempts_registered() {
        for leftover in ["tmp", "magi-tools", "lifecycle-client-check.log"] {
            let f = Fx::new().await;
            let source = f.run("aa10", true).await;
            let empty = f.run("aa11", false).await;
            f.task(&[source, empty.clone()]);
            let wt = &empty.candidates[0].worktree;
            Fx::target(wt);
            let path = wt.join("target").join(leftover);
            if leftover.ends_with(".log") {
                std::fs::write(&path, "keep log\n").unwrap();
            } else {
                // Empty unknown directories are not present in residue_files.
                std::fs::create_dir(&path).unwrap();
            }
            let mut disk = Fx::disk();
            disk.auto_fold = true;
            let report = sweep(&f.home, &disk, Fx::now()).await;
            assert_eq!(report.freed_bytes, 200, "{report:?}");
            assert_eq!(report.retired, 0, "{report:?}");
            assert!(!wt.join("target/debug/output").exists());
            assert!(!residue_proof(&f.home, &empty.id, wt).unwrap().exists());
            assert!(path.exists());
            if leftover.ends_with(".log") {
                assert_eq!(std::fs::read(&path).unwrap(), b"keep log\n");
            }
            assert_eq!(std::fs::read(wt.join("source")).unwrap(), b"base\n");
            assert!(worktrees(&empty).await.unwrap().contains(wt));
            assert!(
                git::branch_exists(&f.repo, &empty.candidates[0].branch)
                    .await
                    .unwrap()
            );
            assert!(
                RunState::load_under(&empty.id, &f.home)
                    .unwrap()
                    .retired_to
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn residue_proof_rejects_new_unknown_root_entries_including_empty_directories() {
        let f = Fx::new().await;
        let state = f.run("aa12", false).await;
        let wt = &state.candidates[0].worktree;
        Fx::target(wt);
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.freed_bytes, 200, "{report:?}");
        assert!(verify_residue(&f.home, &state.id, wt).await.is_ok());
        for name in ["tmp", "magi-tools", "log"] {
            let path = wt.join("target").join(name);
            std::fs::create_dir(&path).unwrap();
            assert!(verify_residue(&f.home, &state.id, wt).await.is_err());
            std::fs::remove_dir(path).unwrap();
        }
        assert!(verify_residue(&f.home, &state.id, wt).await.is_ok());
    }

    #[tokio::test]
    async fn target_identity_and_known_profiles_still_fail_closed_with_root_leftovers() {
        for invalid in [
            "tag",
            "identity",
            "tracked",
            "profile-file",
            "profile-symlink",
            "identity-symlink",
        ] {
            let f = Fx::new().await;
            let state = f.run("aa13", false).await;
            let wt = &state.candidates[0].worktree;
            Fx::target(wt);
            std::fs::create_dir(wt.join("target/tmp")).unwrap();
            let target = wt.join("target");
            match invalid {
                "tag" => std::fs::write(target.join("CACHEDIR.TAG"), "not Cargo").unwrap(),
                "identity" => std::fs::write(target.join(".rustc_info.json"), "{}").unwrap(),
                "tracked" => {
                    git::git(wt, &["add", "-f", "target/debug/output"])
                        .await
                        .unwrap();
                }
                "profile-file" => {
                    std::fs::write(target.join("release"), "keep").unwrap();
                }
                "profile-symlink" => {
                    std::os::unix::fs::symlink(target.join("debug"), target.join("release"))
                        .unwrap();
                }
                "identity-symlink" => {
                    std::fs::rename(target.join(".rustc_info.json"), target.join("saved-info"))
                        .unwrap();
                    std::os::unix::fs::symlink(
                        target.join("saved-info"),
                        target.join(".rustc_info.json"),
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let before = std::fs::read(target.join("debug/output")).unwrap();
            let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
            assert_eq!(report.freed_bytes, 0, "{invalid}: {report:?}");
            assert_eq!(std::fs::read(target.join("debug/output")).unwrap(), before);
            assert!(target.join("tmp").is_dir());
        }
    }

    #[tokio::test]
    async fn reclaimed_cargo_residue_allows_retirement_only_while_exact_and_unowned() {
        let f = Fx::new().await;
        let source = f.run("0000", true).await;
        let empty = f.run("0001", false).await;
        let changed = f.run("0002", false).await;
        let unknown = f.run("0003", false).await;
        let unproven = f.run("0004", false).await;
        f.task(&[
            source.clone(),
            empty.clone(),
            changed.clone(),
            unknown.clone(),
            unproven.clone(),
        ]);
        for run in [&empty, &changed, &unknown] {
            Fx::target(&run.candidates[0].worktree);
        }
        let reviewer = empty.worktree_root().join("review-1");
        git::worktree_add_detached(&f.repo, &reviewer, &f.base)
            .await
            .unwrap();
        Fx::target(&reviewer);
        let mut disk = Fx::disk();
        disk.auto_fold = false;
        let report = sweep(&f.home, &disk, Fx::now()).await;
        assert_eq!(report.freed_bytes, 800, "{report:?}");
        let wt = &empty.candidates[0].worktree;
        assert!(wt.join("target/debug/output.d").exists());
        assert!(!wt.join("target/debug/output").exists());
        let proof_path = residue_proof(&f.home, &empty.id, wt).unwrap();
        let proof: CargoResidue =
            serde_json::from_slice(&std::fs::read(&proof_path).unwrap()).unwrap();
        assert_eq!(
            proof.files[Path::new("debug/output.d")],
            std::fs::read(wt.join("target/debug/output.d")).unwrap()
        );
        std::fs::write(
            changed.candidates[0].worktree.join("target/debug/output.d"),
            "owner notes",
        )
        .unwrap();
        std::fs::write(
            unknown.candidates[0].worktree.join("target/debug/notes"),
            "keep",
        )
        .unwrap();
        Fx::target(&unproven.candidates[0].worktree);
        std::fs::remove_file(unproven.candidates[0].worktree.join("target/debug/output")).unwrap();
        disk.auto_fold = true;
        let locks = cargo_locks(&wt.join("target")).unwrap();
        let mut report = Report::default();
        retire_empty(&f.home, &disk, Fx::now(), &mut report).await;
        assert_eq!(report.retired, 0, "{report:?}");
        assert!(
            report
                .retained
                .iter()
                .any(|reason| reason.contains("Cargo build"))
        );
        drop(locks);
        retire_empty(&f.home, &disk, Fx::now(), &mut report).await;
        assert_eq!(report.retired, 1, "{report:?}");
        assert!(!wt.exists());
        assert!(!reviewer.exists());
        assert!(
            residue_proof(&f.home, &empty.id, &reviewer)
                .unwrap()
                .exists()
        );
        assert!(
            proof_path.exists(),
            "metadata evidence outlives the worktree"
        );
        assert_eq!(
            RunState::load_under(&empty.id, &f.home).unwrap().retired_to,
            Some(source.id)
        );
        for run in [&changed, &unknown, &unproven] {
            assert!(run.candidates[0].worktree.exists());
            assert!(
                RunState::load_under(&run.id, &f.home)
                    .unwrap()
                    .retired_to
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn empty_backlog_retires_to_useful_older_source_in_bounded_idempotent_passes() {
        let f = Fx::new().await;
        let source = f.run("0000", true).await;
        let source_head = git::rev_parse(&f.repo, &source.candidates[0].branch)
            .await
            .unwrap();
        let mut runs = vec![source.clone()];
        for n in 1..=12 {
            runs.push(f.run(&format!("{n:04}"), false).await);
        }
        let dirty = runs[1].candidates[0].worktree.clone();
        std::fs::write(dirty.join("unrecorded"), "preparation changes").unwrap();
        let ignored = runs[2].candidates[0].worktree.clone();
        std::fs::create_dir(ignored.join("scratch")).unwrap();
        std::fs::write(ignored.join("scratch/note"), "keep").unwrap();
        // A missing candidate array does not prove that an unrecorded worktree
        // is empty: discovery must inspect Git and the worktree itself.
        runs[1].candidates.clear();
        runs[1].save_under(&f.home).unwrap();
        let task = f.task(&runs);
        let mut total = 0;
        for _ in 0..4 {
            let mut report = Report::default();
            retire_empty(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
            assert!(report.retired <= 5);
            total += report.retired;
        }
        assert_eq!(total, 10);
        assert!(dirty.exists());
        assert!(ignored.exists());
        assert_eq!(
            git::rev_parse(&f.repo, &source.candidates[0].branch)
                .await
                .unwrap(),
            source_head
        );
        let retired = RunState::load_under(&runs[3].id, &f.home).unwrap();
        assert_eq!(retired.retired_to.as_ref(), Some(&source.id));
        assert!(crate::pr_head::ensure_owner_record(&retired, Some(&retired)).is_err());
        let mut released = task.clone();
        released.release();
        assert_eq!(
            crate::daemon::unfinished_run_with(&released.runs, released.short(), |id| {
                RunState::load_under(id, &f.home)
            }),
            Some(source.id.clone())
        );

        assert!(
            f.home
                .join("runs")
                .join(&retired.id)
                .join("run.json")
                .exists()
        );
        assert!(
            f.home
                .join("recovery-lineages")
                .join(format!("{}.json", task.id))
                .exists()
        );
    }

    #[tokio::test]
    async fn distinct_candidates_and_explicitly_retained_sources_are_never_discarded() {
        let f = Fx::new().await;
        let a = f.run("dddd", true).await;
        let b = f.run("eeee", true).await;
        let empty = f.run("ffff", false).await;
        let task = f.task(&[a.clone(), b.clone(), empty.clone()]);
        assert!(
            crate::pr_recovery::source_retention_reason(&f.home, &a).is_some(),
            "unfinished queue history protects older source branches from ordinary folding"
        );
        let mut prior = a.clone();
        assert!(
            crate::graph::fold_run(&mut prior, true, &f.home)
                .await
                .unwrap_err()
                .to_string()
                .contains(&task.id)
        );
        let mut report = Report::default();
        retire_empty(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.retired, 0);
        assert!(report.retained.iter().any(|r| r.contains("ambiguous")));
        crate::pr_recovery::retain_sources(&task, &f.home).unwrap();
        Fx::target(&a.candidates[0].worktree);
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert!(
            report.freed_bytes > 0,
            "source retention does not retain regenerable outputs"
        );
        assert!(a.candidates[0].worktree.exists());
        assert!(b.candidates[0].worktree.exists());
        assert!(empty.candidates[0].worktree.exists());
    }

    #[tokio::test]
    async fn decisions_checkpoints_and_partial_retirement_remain_safe() {
        let f = Fx::new().await;
        let authority = f.run("p000", true).await;
        let question_run = f.run("p001", false).await;
        let mut checkpoint = f.run("p002", false).await;
        checkpoint.land_approval = Some(crate::run::LandApproval {
            question: "pending".into(),
            head: f.base.clone(),
        });
        checkpoint.save_under(&f.home).unwrap();
        let retained = f.run("p003", false).await;
        std::fs::write(
            f.home
                .join("runs")
                .join(&retained.id)
                .join("retain-recovery-sources"),
            "owner",
        )
        .unwrap();
        let mut partial = f.run("p004", false).await;
        let reviewer = partial.worktree_root().join("review-1");
        git::worktree_add_detached(&f.repo, &reviewer, &f.base)
            .await
            .unwrap();
        // Model a crash after the retirement fence but only one removal.
        partial.retired_to = Some(authority.id.clone());
        partial.save_under(&f.home).unwrap();
        assert!(
            git::worktree_remove_clean(&f.repo, &partial.candidates[0].worktree)
                .await
                .unwrap()
        );
        let git_checkpoint = f.run("p005", false).await;
        let location = git::git(
            &git_checkpoint.candidates[0].worktree,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "sequencer",
            ],
        )
        .await
        .unwrap();
        std::fs::create_dir(location.trim()).unwrap();
        let mut preservation = f.run("p006", false).await;
        preservation
            .candidate_preservation
            .insert('A', "pending preservation".into());
        preservation.save_under(&f.home).unwrap();
        let task = f.task(&[
            preservation.clone(),
            git_checkpoint.clone(),
            authority.clone(),
            question_run.clone(),
            checkpoint.clone(),
            retained.clone(),
            partial.clone(),
        ]);
        let questions = crate::ask::Questions::at(f.home.join("questions"));
        let mut question = crate::ask::Question::new(
            question_run.id.clone(),
            "implement".into(),
            "fixture".into(),
            "owner decision".into(),
            String::new(),
            vec!["keep".into()],
        );
        questions.put(&mut question).unwrap();
        let mut report = Report::default();
        retire_empty(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.retired, 1, "{report:?}");
        assert!(!reviewer.exists());
        for state in [
            &question_run,
            &checkpoint,
            &retained,
            &git_checkpoint,
            &preservation,
        ] {
            assert!(state.candidates[0].worktree.exists());
            assert!(
                RunState::load_under(&state.id, &f.home)
                    .unwrap()
                    .retired_to
                    .is_none()
            );
        }
        assert!(questions.get(&question.id).unwrap().status.open());
        assert_eq!(
            crate::queue::Queue::at(f.home.join("queue"))
                .get(&task.id)
                .unwrap()
                .status,
            task.status
        );
    }

    #[tokio::test]
    async fn preservation_schema_migration_remains_distinct_from_retirement() {
        let f = Fx::new().await;
        let mut state = f.run("p007", false).await;
        state.schema = 15;
        state.save_under(&f.home).unwrap();
        let native = RunState::load_under(&state.id, &f.home).unwrap();
        assert_eq!(native.schema, crate::run::SCHEMA);
        assert!(native.candidate_preservation.is_empty());
        assert!(native.retired_to.is_none());
        state.schema = 14;
        state.save_under(&f.home).unwrap();
        let mut legacy = RunState::load_under(&state.id, &f.home).unwrap();
        assert!(legacy.candidate_preservation.contains_key(&'A'));
        legacy.save_under(&f.home).unwrap();
        let restored = RunState::load_under(&state.id, &f.home).unwrap();
        assert_eq!(
            restored.candidate_preservation,
            legacy.candidate_preservation
        );
    }

    #[tokio::test]
    async fn cache_cleanup_excludes_process_cleanup_and_missing_path_aliases() {
        let f = Fx::new().await;
        let cache = f.repo.join("cache");
        let guard = OutputLease::cache(&f.home, &cache, false).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        assert!(OutputLease::cache(&f.home, &cache, true).is_err());
        assert!(
            crate::cache::maintenance_prune(&f.home, &cache, 1)
                .unwrap()
                .is_none()
        );
        drop(guard);
        assert!(OutputLease::cache(&f.home, &cache, true).is_ok());
    }

    #[tokio::test]
    async fn launcher_death_does_not_release_live_tool_output_reservations() {
        let f = Fx::new().await;
        let run = f.run("abcd", false).await;
        let wt = &run.candidates[0].worktree;
        let mut helper = tokio::process::Command::new(std::env::current_exe().unwrap());
        helper
            .quiet()
            .args(["--exact", "seat_process::tests::crashed_launcher_helper"])
            .env("MAGI_TEST_CRASHED_LAUNCHER", &f.home)
            .env("MAGI_TEST_WORKTREE", wt)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(8), helper.status())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        let pid: i32 = std::fs::read_to_string(f.home.join("tool.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        struct KillTool(i32);
        impl Drop for KillTool {
            fn drop(&mut self) {
                // SAFETY: PID belongs to the bounded helper above.
                unsafe { libc::kill(self.0, libc::SIGKILL) };
            }
        }
        let tool = KillTool(pid);
        assert!(OutputLease::take(wt, true).is_err());
        assert!(OutputLease::cache(&f.home, &f.home.join("cache"), true).is_err());
        drop(tool);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if OutputLease::take(wt, true).is_ok()
                    && OutputLease::cache(&f.home, &f.home.join("cache"), true).is_ok()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn orphaned_tool_keeps_output_and_cache_leases_after_cli_exit() {
        let f = Fx::new().await;
        let run = f.run("abcd", false).await;
        let wt = &run.candidates[0].worktree;
        let cache = f.repo.join("cache");
        std::fs::create_dir(&cache).unwrap();
        for (cancel, clear_owner) in [(false, true), (true, true), (false, false), (true, false)] {
            let pid_file = f.repo.join(format!("orphan-{cancel}-{clear_owner}"));
            let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
            cmd.quiet()
                .args(["--exact", "seat_process::tests::orphan_tool_helper"])
                .env("MAGI_TEST_ORPHAN_PID", &pid_file)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            if clear_owner {
                cmd.env("MAGI_TEST_CLEAR_OWNER", "1");
            } else {
                cmd.env("MAGI_TEST_CLOSE_FDS", "1");
            }
            let gib = 1024 * 1024 * 1024;
            let mut leases = vec![
                OutputLease::borrow(wt).unwrap().unwrap(),
                OutputLease::cache(&f.home, &cache, false).unwrap(),
            ];
            leases.extend(
                OutputLease::admit_with(
                    &f.home,
                    std::slice::from_ref(wt),
                    8 * gib,
                    |_| Ok(9 * gib),
                )
                .unwrap(),
            );
            let mut running = crate::seat_process::Running::spawn(cmd, None, leases).unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(5), running.child().wait())
                    .await
                    .unwrap()
                    .unwrap()
                    .success()
            );
            let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
            struct KillTool(Option<i32>);
            impl Drop for KillTool {
                fn drop(&mut self) {
                    // SAFETY: PID from our helper; kill also bounds panic paths.
                    if let Some(pid) = self.0 {
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
            }
            let mut tool = KillTool(Some(pid));
            // The CLI has already exited and cannot supply a parent-tree
            // relationship. Either the witness or inherited environment must
            // retain ownership; test each independently.
            assert!(OutputLease::take(wt, true).is_err());
            let finish = if cancel {
                drop(running);
                None
            } else {
                Some(tokio::spawn(async move { running.finish().await }))
            };
            if clear_owner {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                assert!(crate::proc::pid_alive(pid as u32));
                assert!(OutputLease::take(wt, true).is_err(), "cancel={cancel}");
                assert!(OutputLease::cache(&f.home, &cache, true).is_err());
                assert!(
                    OutputLease::admit_with(&f.home, std::slice::from_ref(wt), 8 * gib, |_| Ok(
                        9 * gib
                    ))
                    .is_err(),
                    "orphan still owns its disk reservation"
                );
                assert!(
                    crate::cache::maintenance_prune(&f.home, &cache, 0)
                        .unwrap()
                        .is_none()
                );
                if let Some(finish) = &finish {
                    assert!(!finish.is_finished());
                }
                // Witness-only ownership waits for natural/external tool exit.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            if let Some(finish) = finish {
                tokio::time::timeout(std::time::Duration::from_secs(5), finish)
                    .await
                    .unwrap()
                    .unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if OutputLease::take(wt, true).is_ok()
                        && OutputLease::cache(&f.home, &cache, true).is_ok()
                        && OutputLease::admit_with(
                            &f.home,
                            std::slice::from_ref(wt),
                            8 * gib,
                            |_| Ok(9 * gib),
                        )
                        .is_ok()
                    {
                        // Token-owned orphans must be stopped by cleanup itself.
                        if !clear_owner {
                            assert!(!crate::proc::pid_alive(pid as u32));
                        }
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            tool.0 = None;
        }
    }

    #[tokio::test]
    async fn cancellation_keeps_output_and_cache_ownership_until_tools_terminate() {
        let f = Fx::new().await;
        let run = f.run("abcd", false).await;
        let wt = &run.candidates[0].worktree;
        let cache = f.repo.join("cache");
        std::fs::create_dir(&cache).unwrap();
        let pid_file = f.repo.join("child-pid");
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $! > \"$CHILD_PID_FILE\"; wait"])
            .quiet()
            .env("CHILD_PID_FILE", &pid_file)
            .process_group(0)
            .kill_on_drop(true);
        let running = crate::seat_process::Running::spawn(
            cmd,
            None,
            vec![
                OutputLease::borrow(wt).unwrap().unwrap(),
                OutputLease::cache(&f.home, &cache, false).unwrap(),
            ],
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !pid_file.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(OutputLease::take(wt, true).is_err());
        assert!(
            crate::cache::maintenance_prune(&f.home, &cache, 0)
                .unwrap()
                .is_none()
        );
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        drop(running);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(guard) = OutputLease::take(wt, true) {
                    assert!(
                        !crate::proc::pid_alive(pid),
                        "output lock released before tool death"
                    );
                    drop(guard);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(OutputLease::cache(&f.home, &cache, true).is_ok());
    }

    #[tokio::test]
    async fn emergency_pressure_closes_writes_and_zero_floor_opts_out() {
        let f = Fx::new().await;
        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            emergency(vec![f.repo.clone()], u64::MAX),
        )
        .await
        .unwrap();
        assert!(reason.contains("disk pressure"));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                emergency(vec![f.repo.clone()], 0)
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(f.repo.join("source")).unwrap(),
            "base\n"
        );
    }

    #[tokio::test]
    async fn automatic_requeues_cannot_refund_failed_input_generation() {
        let f = Fx::new().await;
        let mut run = f.run("1234", false).await;
        run.prep_failure = Some("cherry-pick conflict".into());
        let mut task = f.task(std::slice::from_ref(&run));
        let inputs = crate::queue::recovery_inputs(&run.sources);
        for _ in 0..108 {
            task.recovery_failed(&run);
            task.requeue();
            assert_eq!(task.attempts, 0);
        }
        let bytes = serde_json::to_vec(&task).unwrap();
        let mut restored: Task = serde_json::from_slice(&bytes).unwrap();
        assert!(
            restored
                .recovery_exhausted(&run.base_commit, &inputs, 2)
                .is_some()
        );
        assert!(
            restored
                .recovery_exhausted("different-base", &inputs, 2)
                .is_none()
        );
        let original_base = run.base_commit.clone();
        run.base_commit = "different-base".into();
        restored.recovery_failed(&run);
        assert!(
            restored
                .recovery_exhausted(&original_base, &inputs, 2)
                .is_some(),
            "returning to old inputs does not replenish their budget"
        );
        assert!(
            restored
                .recovery_exhausted(&run.base_commit, &inputs, 2)
                .is_none()
        );
        restored.release_operator("test-owner", None);
        assert!(
            restored
                .recovery_exhausted(&run.base_commit, &inputs, 2)
                .is_none()
        );
    }
    async fn inactive_source(f: &Fx, tag: &str, useful: bool) -> RunState {
        let mut state = f.run(tag, useful).await;
        state.status = RunStatus::Superseded;
        state.save_under(&f.home).unwrap();
        state
    }

    fn historical_review(state: &mut RunState, head: &str) {
        state.reviews.push(
            serde_json::from_value(serde_json::json!({
                "round": state.reviews.len() + 1, "head": head, "reviews": []
            }))
            .unwrap(),
        );
    }

    async fn reviewer(f: &Fx, state: &RunState, head: &str) -> PathBuf {
        let path = state.worktree_root().join("review-1");
        git::worktree_add_detached(&f.repo, &path, head)
            .await
            .unwrap();
        path
    }

    #[tokio::test]
    async fn source_eviction_separates_run_authority_from_read_seats() {
        for status in [RunStatus::Superseded, RunStatus::Ready, RunStatus::Merged] {
            let f = Fx::new().await;
            let mut state = inactive_source(&f, "ef01", true).await;
            state.status = status;
            state.pr = Some(
                serde_json::from_value(serde_json::json!({
                    "url":"https://example.invalid/pull/1", "number":1, "state":"merged",
                    "checks":"green", "round":1, "rounds":1
                }))
                .unwrap(),
            );
            state.pr_head = Some(state.candidates[0].branch.clone());
            state.pr_merged_head = Some(f.base.clone());
            state.recovery = Some(
                serde_json::from_value(serde_json::json!({
                    "from_run": "20260101-000000-abcd", "generation": 1,
                    "mode": "full-review"
                }))
                .unwrap(),
            );
            state.pr_recovery = Some(
                serde_json::from_value(serde_json::json!({
                    "url": "https://example.invalid/pull/1", "branch": "public",
                    "head": f.base, "source_run": state.id,
                    "instruction": "keep implementation", "approved_at": Fx::now()
                }))
                .unwrap(),
            );
            state.save_under(&f.home).unwrap();
            let review = reviewer(&f, &state, &f.base).await;
            let mut other = inactive_source(&f, "ef02", true).await;
            other.pr_recovery = state.pr_recovery.clone();
            other.save_under(&f.home).unwrap();
            let mut task = f.task(&[state.clone(), other]);
            task.pr_recovery = state.pr_recovery.clone();
            crate::queue::Queue::at(f.home.join("queue"))
                .put(&mut task)
                .unwrap();
            let run_path = f.home.join("runs").join(&state.id).join("run.json");
            let before = std::fs::read(&run_path).unwrap();
            let task_before = serde_json::to_value(&task).unwrap();
            let mut report = Report::default();
            copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
            assert_eq!(report.evicted_copies, 1, "{status:?}: {report:?}");
            assert!(!review.exists());
            assert!(state.candidates[0].worktree.exists());
            assert!(
                git::branch_exists(&f.repo, &state.candidates[0].branch)
                    .await
                    .unwrap()
            );
            assert_eq!(std::fs::read(&run_path).unwrap(), before);
            assert_eq!(
                serde_json::to_value(
                    crate::queue::Queue::at(f.home.join("queue"))
                        .get(&task.id)
                        .unwrap()
                )
                .unwrap(),
                task_before
            );
        }
    }

    #[tokio::test]
    async fn source_eviction_retains_own_paths_and_unfinished_continuations() {
        for kind in ["own-path", "active-seat", "paused-session"] {
            let f = Fx::new().await;
            let mut state = inactive_source(&f, "ef03", true).await;
            let review = reviewer(&f, &state, &f.base).await;
            let expected = match kind {
                "own-path" => {
                    state.attachments.push(review.join("source"));
                    "run.attachments[0]"
                }
                "active-seat" => {
                    state.active.insert(
                        "review-1".into(),
                        serde_json::from_value(serde_json::json!({
                            "node":"review", "started_at":Fx::now(), "timeout_secs":60
                        }))
                        .unwrap(),
                    );
                    "continuation lease"
                }
                _ => {
                    state.status = RunStatus::Blocked;
                    state.parked = true;
                    let mut seat = crate::agent::SeatState::new("review-1", "fixture", 1);
                    seat.turns = 2;
                    seat.captured_session = Some("saved-session".into());
                    state.seats.insert("review-1".into(), seat);
                    "resume contract"
                }
            };
            state.save_under(&f.home).unwrap();
            let mut report = Report::default();
            copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
            assert_eq!(report.evicted_copies, 0, "{kind}: {report:?}");
            assert!(review.exists());
            assert!(
                report.retained.iter().any(|r| r.contains(expected)),
                "{kind}: {report:?}"
            );
            // The execution restoration boundary leaves protected existing
            // seats and their sessions untouched; prep need not recreate them.
            restore_execution_seats(&f.home, &state).await.unwrap();
            assert_eq!(git::rev_parse(&review, "HEAD").await.unwrap(), f.base);
        }
    }

    #[tokio::test]
    async fn source_eviction_distinguishes_historical_cwd_from_question_dependencies() {
        for kind in ["waiting", "lease", "historical", "unknown"] {
            let f = Fx::new().await;
            let state = inactive_source(&f, "ef06", true).await;
            let review = reviewer(&f, &state, &f.base).await;
            let store = crate::ask::Questions::at(f.home.join("questions"));
            let mut question = crate::ask::Question::new(
                "20260101-000000-ffff".into(),
                "review".into(),
                "review-1".into(),
                "fixture".into(),
                "fixture".into(),
                Vec::new(),
            );
            question.cwd = Some(review.to_string_lossy().into_owned());
            if kind != "waiting" {
                question.status = crate::ask::QuestionStatus::Abandoned;
            }
            store.put(&mut question).unwrap();
            if kind == "lease" {
                store.beat(&question.id, crate::ask::WaiterKind::Daemon);
            }
            if kind == "unknown" {
                let path = store.path_of(&question.id);
                let mut raw: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                raw["future_cwd_authority"] = serde_json::Value::Null;
                std::fs::write(path, serde_json::to_vec(&raw).unwrap()).unwrap();
            }
            let mut report = Report::default();
            copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
            assert_eq!(
                report.evicted_copies,
                usize::from(kind == "historical"),
                "{kind}: {report:?}"
            );
            assert_eq!(review.exists(), kind != "historical");
            if kind == "waiting" || kind == "lease" {
                assert!(
                    report
                        .retained
                        .iter()
                        .any(|r| r.contains("question") && r.contains("cwd")),
                    "{report:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn source_eviction_failed_execution_setup_keeps_its_checkpoint() {
        let f = Fx::new().await;
        let mut state = inactive_source(&f, "ef07", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        state.config.worktree.setup = vec![crate::config::SetupStep {
            run: Some("printf preserved > source; exit 1".into()),
            ..Default::default()
        }];
        state.save_under(&f.home).unwrap();
        assert!(restore_execution_seats(&f.home, &state).await.is_err());
        let marker = f
            .home
            .join("runs")
            .join(&state.id)
            .join("artifacts/restore-review-1.pending");
        assert!(marker.exists());
        assert_eq!(
            std::fs::read_to_string(review.join("source")).unwrap(),
            "preserved"
        );
        assert!(
            restore_execution_seats(&f.home, &state)
                .await
                .unwrap_err()
                .to_string()
                .contains("interrupted execution restoration")
        );
        let mut retry = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut retry).await;
        assert_eq!(retry.evicted_copies, 0, "{retry:?}");
        assert!(
            retry
                .retained
                .iter()
                .any(|r| r.contains("restoration checkpoint")),
            "{retry:?}"
        );
        assert!(marker.exists());
        assert_eq!(
            std::fs::read_to_string(review.join("source")).unwrap(),
            "preserved"
        );
    }

    #[tokio::test]
    async fn source_eviction_retains_borrowed_or_promisor_anchor_stores() {
        for kind in [
            "alternates",
            "promisor-config",
            "promisor-pack",
            "objects-symlink",
        ] {
            let f = Fx::new().await;
            let state = inactive_source(&f, "ef11", true).await;
            let review = reviewer(&f, &state, &f.base).await;
            let common = crate::repo_lock::common_dir(&f.repo)
                .await
                .unwrap()
                .unwrap();
            match kind {
                "alternates" => {
                    let borrowed = f._temp.path().join("borrowed-objects");
                    std::fs::create_dir(&borrowed).unwrap();
                    std::fs::write(
                        common.join("objects/info/alternates"),
                        format!("{}\n", borrowed.display()),
                    )
                    .unwrap();
                }
                "promisor-config" => {
                    git::git(&f.repo, &["config", "remote.origin.promisor", "true"])
                        .await
                        .unwrap();
                }
                "promisor-pack" => {
                    std::fs::write(common.join("objects/pack/fixture.promisor"), "").unwrap();
                }
                _ => {
                    let borrowed = f._temp.path().join("borrowed-objects");
                    std::fs::rename(common.join("objects"), &borrowed).unwrap();
                    std::os::unix::fs::symlink(borrowed, common.join("objects")).unwrap();
                }
            }
            let mut report = Report::default();
            copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
            assert_eq!(report.evicted_copies, 0, "{kind}: {report:?}");
            assert!(review.exists());
            assert!(
                report.retained.iter().any(|r| r.contains("source anchor")),
                "{kind}: {report:?}"
            );
        }
    }

    #[tokio::test]
    async fn source_eviction_bounds_long_shared_history_and_requires_its_retained_anchor() {
        let mut f = Fx::new().await;
        crate::fold_evidence::fixtures::shared_history(&f.repo, &f.base, 5000).await;
        f.base = git::rev_parse(&f.repo, "HEAD").await.unwrap();
        assert_eq!(
            git::git(&f.repo, &["rev-list", "--count", &f.base])
                .await
                .unwrap(),
            "5001"
        );
        let state = inactive_source(&f, "ef10", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        std::fs::write(review.join("source"), "bounded review delta").unwrap();
        git::git(&review, &["add", "."]).await.unwrap();
        let tree = git::git(&review, &["write-tree"]).await.unwrap();
        let head = git::git(
            &review,
            &["commit-tree", &tree, "-p", &f.base, "-m", "review delta"],
        )
        .await
        .unwrap();
        git::git(&review, &["checkout", "--detach", &head])
            .await
            .unwrap();
        let run_file = f.home.join("runs").join(&state.id).join("run.json");
        let run_before = std::fs::read(&run_file).unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert!(!review.exists());
        assert!(state.candidates[0].worktree.exists());
        assert!(
            git::branch_exists(&f.repo, &state.candidates[0].branch)
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read(&run_file).unwrap(), run_before);
        let archive = f
            .home
            .join("runs")
            .join(&state.id)
            .join("artifacts/source-evidence")
            .join(format!("{head}-{}", f.base));
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["anchor"], f.base);
        assert!(
            std::fs::metadata(archive.join("objects.pack"))
                .unwrap()
                .len()
                < 16 * 1024
        );
        let pin = format!("refs/magi/source-anchors/{}", f.base);
        assert_eq!(git::rev_parse(&f.repo, &pin).await.unwrap(), f.base);
        git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
            .await
            .unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        assert!(
            !git::git_raw(&f.repo, &["cat-file", "-e", &head])
                .await
                .unwrap()
                .ok()
        );
        let other = git::rev_parse(&state.candidates[0].worktree, "HEAD")
            .await
            .unwrap();
        // Missing, moved, symbolic and symlink pins cannot authorize import.
        for kind in ["missing", "moved", "symbolic", "symlink"] {
            git::git(&f.repo, &["update-ref", "--no-deref", "-d", &pin])
                .await
                .unwrap();
            match kind {
                "moved" => {
                    git::git(&f.repo, &["update-ref", &pin, &other])
                        .await
                        .unwrap();
                }
                "symbolic" => {
                    git::git(&f.repo, &["symbolic-ref", &pin, "refs/heads/main"])
                        .await
                        .unwrap();
                }
                "symlink" => {
                    let common = crate::repo_lock::common_dir(&f.repo)
                        .await
                        .unwrap()
                        .unwrap();
                    let target = f._temp.path().join("operator-anchor");
                    std::fs::write(&target, format!("{}\n", f.base)).unwrap();
                    let file = common.join(&pin);
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::os::unix::fs::symlink(target, file).unwrap();
                }
                _ => {}
            }
            assert!(
                restore_execution_seats(&f.home, &state).await.is_err(),
                "{kind}"
            );
            assert!(!review.exists());
            assert!(
                !git::git_raw(&f.repo, &["cat-file", "-e", &head])
                    .await
                    .unwrap()
                    .ok()
            );
            if kind == "symlink" {
                let common = crate::repo_lock::common_dir(&f.repo)
                    .await
                    .unwrap()
                    .unwrap();
                std::fs::remove_file(common.join(&pin)).unwrap();
            } else {
                git::git(&f.repo, &["update-ref", "--no-deref", "-d", &pin])
                    .await
                    .unwrap();
            }
            git::git(&f.repo, &["update-ref", &pin, &f.base])
                .await
                .unwrap();
        }
        // Fixture-only ref deletion proves the pin, rather than an incidental
        // main/candidate branch, keeps all shared ancestors through GC.
        assert!(
            git::worktree_remove_clean(&f.repo, &state.candidates[0].worktree)
                .await
                .unwrap()
        );
        git::git(
            &f.repo,
            &[
                "update-ref",
                "-d",
                &format!("refs/heads/{}", state.candidates[0].branch),
            ],
        )
        .await
        .unwrap();
        git::git(&f.repo, &["update-ref", "-d", "refs/heads/main"])
            .await
            .unwrap();
        git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
            .await
            .unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        assert_eq!(
            git::git(&f.repo, &["for-each-ref", "--format=%(refname)"])
                .await
                .unwrap(),
            pin
        );
        assert_eq!(
            git::git(&f.repo, &["rev-list", "--count", &f.base])
                .await
                .unwrap(),
            "5001"
        );
        restore_execution_seats(&f.home, &state).await.unwrap();
        assert_eq!(
            git::git(&review, &["rev-list", "--count", "HEAD"])
                .await
                .unwrap(),
            "5002"
        );
        assert_eq!(
            git::git(&f.repo, &["rev-parse", "--is-shallow-repository"])
                .await
                .unwrap(),
            "false"
        );
        git::git(&f.repo, &["fsck", "--full"]).await.unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        let patch = seat_diff(&f.home, &state.id, &head).await.unwrap();
        assert!(String::from_utf8_lossy(&patch).contains("bounded review delta"));
        std::fs::remove_dir_all(&f.repo).unwrap();
        assert_eq!(seat_diff(&f.home, &state.id, &head).await.unwrap(), patch);
        let inspection = f._temp.path().join("shared-history-inspection");
        restore_seat(&f.home, &state.id, &head, &inspection)
            .await
            .unwrap();
        assert_eq!(git::rev_parse(&inspection, "HEAD").await.unwrap(), head);
        assert_eq!(
            git::git(&inspection, &["rev-parse", "--is-shallow-repository"])
                .await
                .unwrap(),
            "true"
        );
        assert_eq!(
            git::git(&inspection, &["log", "--format=%H"])
                .await
                .unwrap(),
            format!("{head}\n{}", f.base)
        );
        assert!(!inspection.join(".git/objects/info/alternates").exists());
        git::git(&inspection, &["fsck", "--full"]).await.unwrap();
        git::git(&inspection, &["gc", "--prune=now"]).await.unwrap();
    }

    #[tokio::test]
    async fn source_eviction_retains_tree_only_archives_and_rejects_import_before_writes() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ef08", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        let tree = git::rev_parse(&review, "HEAD^{tree}").await.unwrap();
        let parent = git::git(&review, &["commit-tree", &tree, "-m", "U0"])
            .await
            .unwrap();
        let head = git::git(&review, &["commit-tree", &tree, "-p", &parent, "-m", "U1"])
            .await
            .unwrap();
        git::git(&review, &["checkout", "--detach", &head])
            .await
            .unwrap();
        let archive =
            crate::fold_evidence::fixtures::tree_only_archive(&state, &f.home, &head).await;
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 0, "{report:?}");
        assert!(review.exists());
        assert!(
            report
                .retained
                .iter()
                .any(|r| r.contains("incomplete archived commit ancestry")),
            "{report:?}"
        );
        // Simulate a historical checkout already removed by the old writer.
        assert!(git::worktree_remove_clean(&f.repo, &review).await.unwrap());
        git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
            .await
            .unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        for oid in [&parent, &head] {
            assert!(
                !git::git_raw(&f.repo, &["cat-file", "-e", oid])
                    .await
                    .unwrap()
                    .ok()
            );
        }
        let patch = source_diff(&archive).await.unwrap();
        assert!(
            crate::fold_evidence::import_snapshot(&archive, &f.repo)
                .await
                .is_err()
        );
        assert!(
            !git::git_raw(&f.repo, &["cat-file", "-e", &head])
                .await
                .unwrap()
                .ok()
        );
        git::git(&f.repo, &["fsck", "--full"]).await.unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        let inspection = f._temp.path().join("legacy-tree-inspection");
        restore_source(&archive, &inspection).await.unwrap();
        assert_eq!(git::git(&inspection, &["write-tree"]).await.unwrap(), tree);
        assert!(
            !git::git_raw(&inspection, &["rev-parse", "--verify", "HEAD"])
                .await
                .unwrap()
                .ok()
        );
        git::git(&inspection, &["fsck", "--full"]).await.unwrap();
        git::git(&inspection, &["gc", "--prune=now"]).await.unwrap();
        assert_eq!(source_diff(&archive).await.unwrap(), patch);
        assert!(state.candidates[0].worktree.exists());
    }

    #[tokio::test]
    async fn source_eviction_retains_ancestry_above_expanded_budget() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ef09", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        std::fs::File::create(review.join("ancestor-only"))
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        git::git(&review, &["add", "."]).await.unwrap();
        let parent_tree = git::git(&review, &["write-tree"]).await.unwrap();
        let parent = git::git(
            &review,
            &["commit-tree", &parent_tree, "-m", "large ancestor"],
        )
        .await
        .unwrap();
        std::fs::remove_file(review.join("ancestor-only")).unwrap();
        git::git(&review, &["add", "."]).await.unwrap();
        let tree = git::git(&review, &["write-tree"]).await.unwrap();
        let head = git::git(
            &review,
            &[
                "commit-tree",
                &tree,
                "-p",
                &parent,
                "-m",
                "small final tree",
            ],
        )
        .await
        .unwrap();
        git::git(&review, &["checkout", "--detach", &head])
            .await
            .unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 0, "{report:?}");
        assert!(
            report
                .retained
                .iter()
                .any(|r| r.contains("expanded object limit exceeded")),
            "{report:?}"
        );
        assert!(review.exists());
        assert!(state.candidates[0].worktree.exists());
    }

    #[tokio::test]
    async fn source_eviction_restores_execution_after_gc_and_preserves_generations() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ef04", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        std::fs::write(review.join("ancestor-only"), "ancestral content").unwrap();
        git::git(&review, &["add", "."]).await.unwrap();
        let parent_tree = git::git(&review, &["write-tree"]).await.unwrap();
        let parent = git::git(
            &review,
            &["commit-tree", &parent_tree, "-m", "unrelated parent"],
        )
        .await
        .unwrap();
        std::fs::remove_file(review.join("ancestor-only")).unwrap();
        std::fs::write(review.join("source"), "unrelated review checkpoint").unwrap();
        git::git(&review, &["add", "."]).await.unwrap();
        let tree = git::git(&review, &["write-tree"]).await.unwrap();
        let head = git::git(
            &review,
            &[
                "commit-tree",
                &tree,
                "-p",
                &parent,
                "-m",
                "unrelated review checkpoint",
            ],
        )
        .await
        .unwrap();
        git::git(&review, &["checkout", "--detach", &head])
            .await
            .unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        let ledger = f
            .home
            .join("runs")
            .join(&state.id)
            .join("artifacts/eviction-review-1.json");
        let original = std::fs::read(&ledger).unwrap();
        let diff = seat_diff(&f.home, &state.id, &head).await.unwrap();
        assert!(String::from_utf8_lossy(&diff).contains("unrelated review checkpoint"));
        git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
            .await
            .unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        assert!(
            !git::git_raw(&f.repo, &["cat-file", "-e", &head])
                .await
                .unwrap()
                .ok()
        );
        assert!(
            !git::git_raw(&f.repo, &["cat-file", "-e", &parent])
                .await
                .unwrap()
                .ok()
        );
        assert_eq!(seat_diff(&f.home, &state.id, &head).await.unwrap(), diff);
        assert!(
            seat_heads(&f.home, &state.id)
                .await
                .unwrap()
                .contains_key(&head)
        );
        restore_execution_seats(&f.home, &state).await.unwrap();
        assert_eq!(git::rev_parse(&review, "HEAD").await.unwrap(), head);
        assert_eq!(
            git::git(&review, &["log", "--format=%H"]).await.unwrap(),
            format!("{head}\n{parent}")
        );
        git::git(&f.repo, &["fsck", "--full"]).await.unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        let inspection = f._temp.path().join("complete-history-inspection");
        restore_seat(&f.home, &state.id, &head, &inspection)
            .await
            .unwrap();
        assert_eq!(
            git::git(&inspection, &["log", "--format=%H"])
                .await
                .unwrap(),
            format!("{head}\n{parent}")
        );
        git::git(&inspection, &["fsck", "--full"]).await.unwrap();
        git::git(&inspection, &["gc", "--prune=now"]).await.unwrap();

        assert_eq!(git::git(&review, &["write-tree"]).await.unwrap(), tree);
        assert!(
            !git::git_raw(&review, &["symbolic-ref", "-q", "HEAD"])
                .await
                .unwrap()
                .ok()
        );
        assert_eq!(
            std::fs::read_to_string(review.join("source")).unwrap(),
            "unrelated review checkpoint"
        );
        std::fs::write(review.join("operator-notes"), "keep").unwrap();
        assert!(restore_execution_seats(&f.home, &state).await.is_err());
        assert_eq!(
            std::fs::read_to_string(review.join("operator-notes")).unwrap(),
            "keep"
        );
        std::fs::remove_file(review.join("operator-notes")).unwrap();
        restore_execution_seats(&f.home, &state).await.unwrap();
        let mut second = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut second).await;
        assert_eq!(second.evicted_copies, 1, "{second:?}");
        assert_eq!(std::fs::read(&ledger).unwrap(), original);
        assert!(ledger.with_file_name("eviction-review-1-2.json").exists());
        // A historical v1 fixture has no explicit base field. Neither schema
        // may reinterpret its immutable archive when the owning run advances.
        let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
        legacy["version"] = 1.into();
        legacy.as_object_mut().unwrap().remove("base");
        std::fs::write(&ledger, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let mut advanced = state.clone();
        advanced.base_commit = git::rev_parse(&state.candidates[0].worktree, "HEAD")
            .await
            .unwrap();
        advanced.save_under(&f.home).unwrap();
        assert_eq!(seat_diff(&f.home, &state.id, &head).await.unwrap(), diff);
        // Corruption blocks execution before a replacement checkout is made.
        let raw: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let archive = PathBuf::from(raw["evidence"].as_str().unwrap());
        let patch = std::fs::read(archive.join("change.patch")).unwrap();
        std::fs::write(archive.join("change.patch"), "corrupt").unwrap();
        assert!(restore_execution_seats(&f.home, &state).await.is_err());
        assert!(!review.exists());
        std::fs::write(archive.join("change.patch"), patch).unwrap();
        std::fs::create_dir(&review).unwrap();
        std::fs::write(review.join("operator-notes"), "keep").unwrap();
        assert!(restore_execution_seats(&f.home, &state).await.is_err());
        assert_eq!(
            std::fs::read_to_string(review.join("operator-notes")).unwrap(),
            "keep"
        );
    }

    #[tokio::test]
    async fn source_eviction_preserves_history_and_restores_binary_modes_after_gc() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = Fx::new().await;
        std::fs::write(f.repo.join("binary"), b"old\0binary\xff").unwrap();
        git::git(&f.repo, &["add", "."]).await.unwrap();
        git::git(&f.repo, &["commit", "-m", "binary base"])
            .await
            .unwrap();
        f.base = git::rev_parse(&f.repo, "HEAD").await.unwrap();
        let mut state = inactive_source(&f, "ec01", true).await;
        let candidate = state.candidates[0].worktree.clone();
        let earlier = git::rev_parse(&candidate, "HEAD").await.unwrap();
        historical_review(&mut state, &earlier);
        let review = reviewer(&f, &state, &earlier).await;
        std::fs::remove_file(candidate.join("source")).unwrap();
        std::fs::write(candidate.join("binary"), b"new\0binary\xfe").unwrap();
        std::fs::write(candidate.join("script"), "run\n").unwrap();
        std::fs::set_permissions(
            candidate.join("script"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        git::git(&candidate, &["add", "."]).await.unwrap();
        git::git(&candidate, &["commit", "-m", "binary deletion and mode"])
            .await
            .unwrap();
        let head = git::rev_parse(&candidate, "HEAD").await.unwrap();
        let tree = git::rev_parse(&candidate, "HEAD^{tree}").await.unwrap();
        let base_tree = git::rev_parse(&candidate, &format!("{}^{{tree}}", f.base))
            .await
            .unwrap();
        historical_review(&mut state, &head);
        historical_review(&mut state, &head);
        state.status = RunStatus::Superseded;
        state.save_under(&f.home).unwrap();
        let task = f.task(std::slice::from_ref(&state));
        let before = serde_json::to_value(&state).unwrap();
        git::git(&review, &["reset", "--hard", &head])
            .await
            .unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert!(!review.exists());
        assert!(candidate.exists(), "selected implementation survives");
        assert!(report.source_unlinked_logical_bytes > 0);
        assert!(report.source_unlinked_allocated_bytes.is_some());
        assert!(report.source_evidence_added_allocated_bytes.is_some());
        assert_eq!(
            serde_json::to_value(RunState::load_under(&state.id, &f.home).unwrap()).unwrap(),
            before
        );
        assert_eq!(
            crate::queue::Queue::at(f.home.join("queue"))
                .get(&task.id)
                .unwrap()
                .runs,
            task.runs
        );
        let archive = f
            .home
            .join("runs")
            .join(&state.id)
            .join("artifacts/source-evidence")
            .join(format!("{head}-{}", f.base));
        let patch = source_diff(&archive).await.unwrap();
        assert!(String::from_utf8_lossy(&patch).contains("GIT binary patch"));
        assert!(String::from_utf8_lossy(&patch).contains("100755"));
        assert!(
            git::worktree_remove_clean(&f.repo, &candidate)
                .await
                .unwrap()
        );
        git::git(&f.repo, &["branch", "-D", &state.candidates[0].branch])
            .await
            .unwrap();
        git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
            .await
            .unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        assert!(
            !git::git_raw(&f.repo, &["cat-file", "-e", &head])
                .await
                .unwrap()
                .ok()
        );
        // Even the remaining base repository is unavailable to the reader.
        std::fs::remove_dir_all(&f.repo).unwrap();
        assert_eq!(source_diff(&archive).await.unwrap(), patch);
        let restored = f._temp.path().join("restored");
        restore_source(&archive, &restored).await.unwrap();
        assert_eq!(git::git(&restored, &["write-tree"]).await.unwrap(), tree);
        assert_eq!(
            std::fs::read(restored.join("binary")).unwrap(),
            b"new\0binary\xfe"
        );
        assert!(!restored.join("source").exists());
        assert_ne!(
            std::fs::metadata(restored.join("script"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert!(restore_source(&archive, &restored).await.is_err());
        // Reconstruct from the archived base plus the displayed binary patch,
        // rather than relying only on checking out the archived final tree.
        git::git(&restored, &["read-tree", "--reset", "-u", &base_tree])
            .await
            .unwrap();
        let patch_file = f._temp.path().join("saved.patch");
        std::fs::write(&patch_file, &patch).unwrap();
        git::git(
            &restored,
            &["apply", "--index", "--binary", patch_file.to_str().unwrap()],
        )
        .await
        .unwrap();
        assert_eq!(git::git(&restored, &["write-tree"]).await.unwrap(), tree);
        assert_eq!(
            std::fs::read(restored.join("binary")).unwrap(),
            b"new\0binary\xfe"
        );
    }

    #[tokio::test]
    async fn source_eviction_archives_unrelated_tips_and_allows_lineage_references() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ec10", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        let mut successor = inactive_source(&f, "ec11", true).await;
        successor.retired_to = Some(state.id.clone());
        successor.save_under(&f.home).unwrap();
        f.task(&[state.clone(), successor]);

        let mut tips = Vec::new();
        for contents in ["reflog-only evidence", "ORIG_HEAD-only evidence"] {
            std::fs::write(review.join("source"), contents).unwrap();
            git::git(&review, &["add", "."]).await.unwrap();
            let tree = git::git(&review, &["write-tree"]).await.unwrap();
            let tip = git::git(
                &review,
                &["commit-tree", &tree, "-p", &f.base, "-m", contents],
            )
            .await
            .unwrap();
            git::git(&review, &["reset", "--hard", &tip]).await.unwrap();
            tips.push(tip);
            git::git(&review, &["reset", "--hard", &f.base])
                .await
                .unwrap();
        }
        // Keep one tip exclusively in ORIG_HEAD, the other in the HEAD log.
        git::git(&review, &["reflog", "delete", "--rewrite", "HEAD@{1}"])
            .await
            .unwrap();
        git::git(&review, &["update-ref", "ORIG_HEAD", &tips[1]])
            .await
            .unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert!(!review.exists());
        git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
            .await
            .unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        for tip in &tips {
            assert!(
                !git::git_raw(&f.repo, &["cat-file", "-e", tip])
                    .await
                    .unwrap()
                    .ok()
            );
        }
        // The run-oriented interface also works without the source object store.
        std::fs::remove_dir_all(&f.repo).unwrap();
        let shown = seat_evidence(&f.home, &state.id).await.unwrap();
        for (i, tip) in tips.iter().enumerate() {
            assert!(
                shown
                    .iter()
                    .any(|row| row.contains(tip) && row.contains("patch_bytes="))
            );
            let destination = f._temp.path().join(format!("restore-history-{i}"));
            restore_seat(&f.home, &state.id, tip, &destination)
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(destination.join("source")).unwrap(),
                ["reflog-only evidence", "ORIG_HEAD-only evidence"][i]
            );
        }
        assert!(
            restore_seat(
                &f.home,
                &state.id,
                "unknown",
                &f._temp.path().join("missing")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn source_eviction_revalidates_archive_immediately_before_unlink() {
        fn corrupt(path: &Path) -> Result<()> {
            let root = path.parent().unwrap();
            // Locate the fixture's durable artifact store through its run record
            // path supplied below by a test-only hook in the artifact tree.
            let marker = root.join("archive-to-corrupt");
            let archive = std::fs::read_to_string(&marker)?;
            std::fs::write(Path::new(&archive).join("change.patch"), "corrupt")?;
            Ok(())
        }
        let f = Fx::new().await;
        let state = inactive_source(&f, "ec12", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        let archive = f
            .home
            .join("runs")
            .join(&state.id)
            .join("artifacts/source-evidence")
            .join(format!("{}-{}", f.base, f.base));
        std::fs::write(
            state.worktree_root().join("archive-to-corrupt"),
            archive.to_str().unwrap(),
        )
        .unwrap();
        let mut report = Report::default();
        copies::evict_with(&f.home, &Fx::disk(), Fx::now(), &mut report, corrupt).await;
        assert_eq!(report.evicted_copies, 0, "{report:?}");
        assert!(review.exists());
        assert!(
            report
                .retained
                .iter()
                .any(|r| r.contains("corrupt snapshot")),
            "{report:?}"
        );
    }

    #[tokio::test]
    async fn source_eviction_fails_closed_for_dirty_ignored_borrowed_and_authoritative_copies() {
        for kind in [
            "dirty",
            "hidden",
            "ignored",
            "empty-directory",
            "untracked",
            "borrowed",
            "borrowed-symlink",
            "lease",
            "owner",
            "driver",
            "task-pin",
            "task-replacement",
            "malformed-orig-head",
            "grace",
            "resumable",
            "archive-symlink",
            "root-symlink",
            "checkpoint",
            "symlink",
            "retained",
            "unreadable",
            "unknown-field",
            "unknown-task-field",
        ] {
            let f = Fx::new().await;
            let mut state = inactive_source(&f, "ec02", true).await;
            let head = git::rev_parse(&state.candidates[0].worktree, "HEAD")
                .await
                .unwrap();
            historical_review(&mut state, &head);
            let review = reviewer(&f, &state, &head).await;
            let mut output = None;
            let mut owner = None;
            let mut driver = None;
            let mut disk = Fx::disk();
            match kind {
                "dirty" => std::fs::write(review.join("source"), "notes").unwrap(),
                "hidden" => {
                    git::git(&review, &["update-index", "--assume-unchanged", "source"])
                        .await
                        .unwrap();
                    std::fs::write(review.join("source"), "notes").unwrap();
                }
                "ignored" => {
                    std::fs::create_dir(review.join("scratch")).unwrap();
                    std::fs::write(review.join("scratch/notes"), "notes").unwrap();
                }
                "empty-directory" => std::fs::create_dir(review.join("owner-notes")).unwrap(),
                "untracked" => std::fs::write(review.join("notes"), "notes").unwrap(),
                "borrowed" => {
                    let mut other = f.run("ec03", true).await;
                    other.candidates[0].worktree = review.clone();
                    other.save_under(&f.home).unwrap();
                }
                "borrowed-symlink" => {
                    let alias = f._temp.path().join("borrowed-alias");
                    std::os::unix::fs::symlink(&review, &alias).unwrap();
                    let mut other = f.run("ec03", true).await;
                    other.candidates[0].worktree = alias;
                    other.save_under(&f.home).unwrap();
                }
                "lease" => output = OutputLease::borrow(&review).unwrap(),
                "owner" => owner = Some(crate::pr_head::owner_lock(&f.home, &state.id).unwrap()),
                "driver" => driver = Some(crate::driver::maintenance(&f.home, &state.id).unwrap()),
                "task-pin" => {
                    let mut task = f.task(std::slice::from_ref(&state));
                    task.resume_override = Some(crate::queue::OperatorResume {
                        question_id: "fixture-pin".into(),
                        at: Fx::now(),
                        conductor_rehold: None,
                        forced: true,
                        pinned_run: Some(state.id.clone()),
                    });
                    crate::queue::Queue::at(f.home.join("queue"))
                        .put(&mut task)
                        .unwrap();
                }
                "task-replacement" => {
                    let mut task = f.task(std::slice::from_ref(&state));
                    task.replacement = Some(crate::merge_decision::Replacement {
                        question: "fixture-replacement".into(),
                        run: state.id.clone(),
                        pr: "https://example.invalid/pull/1".into(),
                        agent: "worker".into(),
                        context: String::new(),
                    });
                    crate::queue::Queue::at(f.home.join("queue"))
                        .put(&mut task)
                        .unwrap();
                }
                "malformed-orig-head" => {
                    let admin = git::git(&review, &["rev-parse", "--absolute-git-dir"])
                        .await
                        .unwrap();
                    std::fs::write(Path::new(&admin).join("ORIG_HEAD"), "not-an-object-id")
                        .unwrap();
                }
                "grace" => disk.fold_grace_secs = 3600,
                "resumable" => state.status = RunStatus::Blocked,
                "archive-symlink" => {
                    let artifacts = f.home.join("runs").join(&state.id).join("artifacts");
                    std::fs::create_dir_all(&artifacts).unwrap();
                    let outside = f._temp.path().join("outside");
                    std::fs::create_dir(&outside).unwrap();
                    std::os::unix::fs::symlink(&outside, artifacts.join("source-evidence"))
                        .unwrap();
                }
                "root-symlink" => {
                    let root = state.worktree_root();
                    let outside = f._temp.path().join("outside-root");
                    std::fs::rename(&root, &outside).unwrap();
                    std::os::unix::fs::symlink(&outside, &root).unwrap();
                }
                "checkpoint" => {
                    let admin = git::git(&review, &["rev-parse", "--absolute-git-dir"])
                        .await
                        .unwrap();
                    std::fs::write(Path::new(&admin).join("MERGE_MSG"), "checkpoint").unwrap();
                }
                "symlink" => {
                    std::fs::remove_file(review.join("source")).unwrap();
                    std::os::unix::fs::symlink(".gitignore", review.join("source")).unwrap();
                }
                "retained" => std::fs::write(
                    f.home
                        .join("runs")
                        .join(&state.id)
                        .join("retain-recovery-sources"),
                    "owner",
                )
                .unwrap(),
                "unreadable" => {
                    std::fs::create_dir_all(f.home.join("runs/20260101-000000-ffff")).unwrap();
                }
                "unknown-field" | "unknown-task-field" => {}
                _ => unreachable!(),
            }
            state.save_under(&f.home).unwrap();
            if kind == "unknown-field" {
                let path = f.home.join("runs").join(&state.id).join("run.json");
                let mut raw: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                raw["future_source_owner"] = serde_json::json!({"path": review});
                std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
            }
            if kind == "unknown-task-field" {
                let task = f.task(std::slice::from_ref(&state));
                let path = crate::queue::Queue::at(f.home.join("queue")).path_of(&task.id);
                let mut raw: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                raw["future_source_owner"] = serde_json::json!({"path": review});
                std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
            }
            let mut report = Report::default();
            copies::evict(&f.home, &disk, Fx::now(), &mut report).await;
            assert_eq!(report.evicted_copies, 0, "{kind}: {report:?}");
            assert!(review.exists(), "{kind}: {report:?}");
            assert!(!report.retained.is_empty(), "{kind}: {report:?}");
            drop(output);
            drop(owner);
            drop(driver);
        }
    }

    #[tokio::test]
    async fn source_eviction_reconciles_interruption_and_refuses_corrupt_archives() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ec04", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        let artifacts = f.home.join("runs").join(&state.id).join("artifacts");
        let ledger = artifacts.join("eviction-review-1.json");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        record["phase"] = "pending".into();
        std::fs::write(&ledger, serde_json::to_vec(&record).unwrap()).unwrap();
        let archive = PathBuf::from(record["evidence"].as_str().unwrap());
        let pack = std::fs::read(archive.join("objects.pack")).unwrap();
        std::fs::write(archive.join("objects.pack"), b"corrupt").unwrap();
        let mut blocked = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut blocked).await;
        assert!(
            blocked
                .retained
                .iter()
                .any(|r| r.contains("corrupt snapshot")),
            "{blocked:?}"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&ledger).unwrap()).unwrap()
                ["phase"],
            "pending"
        );
        assert!(
            restore_source(&archive, &f._temp.path().join("bad-restore"))
                .await
                .is_err()
        );
        std::fs::write(archive.join("objects.pack"), &pack).unwrap();
        let mut retried = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut retried).await;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&ledger).unwrap()).unwrap()
                ["phase"],
            "evicted"
        );
        assert_eq!(retried.evicted_copies, 0);
        assert!(!review.exists());
        // A stopped archive publisher can resume its immutable staging files.
        let pending = archive.with_extension("pending");
        std::fs::rename(&archive, &pending).unwrap();
        assert_eq!(
            crate::fold_evidence::snapshot(&state, &f.home, &f.base, 64 * 1024 * 1024)
                .await
                .unwrap(),
            archive
        );
        assert!(!pending.exists());
    }

    #[tokio::test]
    async fn source_eviction_reclaims_superseded_empty_attempt_without_retiring_lineage() {
        let f = Fx::new().await;
        let mut empty = inactive_source(&f, "ec05", false).await;
        historical_review(&mut empty, &f.base);
        empty.status = RunStatus::Superseded;
        empty.save_under(&f.home).unwrap();
        let source = inactive_source(&f, "ec06", true).await;
        let task = f.task(&[empty.clone(), source.clone()]);
        let before = serde_json::to_value(&empty).unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert!(!empty.candidates[0].worktree.exists());
        assert!(source.candidates[0].worktree.exists());
        assert!(
            git::branch_exists(&f.repo, &empty.candidates[0].branch)
                .await
                .unwrap()
        );
        assert_eq!(
            serde_json::to_value(RunState::load_under(&empty.id, &f.home).unwrap()).unwrap(),
            before
        );
        let mut retry = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut retry).await;
        assert_eq!(retry.removed_empty_roots, 1, "{retry:?}");
        assert!(!empty.worktree_root().exists());
        assert_eq!(
            crate::queue::Queue::at(f.home.join("queue"))
                .get(&task.id)
                .unwrap()
                .runs,
            task.runs
        );
    }
    #[tokio::test]
    async fn source_eviction_retries_pending_intent_and_exact_missing_registration() {
        fn interrupt(_: &Path) -> Result<()> {
            anyhow::bail!("fixture interruption before unlink")
        }
        fn vanish(path: &Path) -> Result<()> {
            std::fs::remove_dir_all(path)?;
            anyhow::bail!("fixture interruption after checkout unlink")
        }
        for stage in [
            "intent",
            "missing-registration",
            "changed-admin",
            "missing-archive",
            "wrong-identity",
        ] {
            let f = Fx::new().await;
            let state = inactive_source(&f, "ec07", true).await;
            let review = reviewer(&f, &state, &f.base).await;
            let admin = PathBuf::from(
                git::git(&review, &["rev-parse", "--absolute-git-dir"])
                    .await
                    .unwrap(),
            );
            let mut interrupted = Report::default();
            copies::evict_with(
                &f.home,
                &Fx::disk(),
                Fx::now(),
                &mut interrupted,
                if stage == "missing-registration" || stage == "changed-admin" {
                    vanish
                } else {
                    interrupt
                },
            )
            .await;
            assert_eq!(interrupted.evicted_copies, 0, "{stage}: {interrupted:?}");
            let ledger = f
                .home
                .join("runs")
                .join(&state.id)
                .join("artifacts/eviction-review-1.json");
            let record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&ledger).unwrap_or_else(|error| {
                    panic!("{stage}: missing pending ledger: {error}; {interrupted:?}")
                }))
                .unwrap();
            assert_eq!(record["phase"], "pending");
            if stage == "changed-admin" {
                std::fs::write(admin.join("owner-note"), "keep").unwrap();
            }
            if stage == "missing-archive" {
                std::fs::remove_file(
                    Path::new(record["evidence"].as_str().unwrap()).join("objects.pack"),
                )
                .unwrap();
            }
            if stage == "wrong-identity" {
                let head = git::rev_parse(&state.candidates[0].worktree, "HEAD")
                    .await
                    .unwrap();
                let other =
                    crate::fold_evidence::snapshot(&state, &f.home, &head, 64 * 1024 * 1024)
                        .await
                        .unwrap();
                let original = Path::new(record["evidence"].as_str().unwrap());
                for name in [
                    "manifest.json",
                    "objects.pack",
                    "change.patch",
                    "change.index",
                ] {
                    std::fs::copy(other.join(name), original.join(name)).unwrap();
                }
            }
            let mut retried = Report::default();
            copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut retried).await;
            let phase =
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&ledger).unwrap())
                    .unwrap()["phase"]
                    .clone();
            if stage == "changed-admin" || stage == "missing-archive" || stage == "wrong-identity" {
                assert_eq!(phase, "pending", "{stage}: {retried:?}");
                assert!(!retried.retained.is_empty());
                if stage == "changed-admin" {
                    assert!(admin.join("owner-note").exists());
                } else {
                    assert!(review.exists());
                }
            } else {
                assert_eq!(phase, "evicted", "{stage}: {retried:?}");
                assert!(!review.exists());
                assert!(!admin.exists());
            }
        }
    }
    fn source_census(home: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(home.join("source-eviction-cursor")).unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn source_eviction_scan_bounds_missing_root_checkpoints_and_eventually_reclaims() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ffff", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        for i in 0..130 {
            let mut old = state.clone();
            old.id = format!("20260101-000000-{i:04x}");
            old.candidates.clear();
            old.save_under(&f.home).unwrap();
            // Missing roots with ledger records must still be inspected.
            if i == 0 {
                let artifacts = f.home.join("runs").join(&old.id).join("artifacts");
                std::fs::create_dir_all(&artifacts).unwrap();
                std::fs::write(artifacts.join("eviction-review-1.json"), "{}").unwrap();
            }
        }
        // Each invocation reloads the durable cursor. The corrupt ledger uses
        // one normal inspection slot; the remaining missing roots use a
        // separate 64-entry skip budget, bounding full-census checkpoint writes.
        // Repeat through wrap so idle sweeps cannot reintroduce an unbounded pass.
        for cycle in 1..=2 {
            for (pass, expected_next) in [65, 129, 131].into_iter().enumerate() {
                let mut report = Report::default();
                copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
                assert!(report.failures.is_empty(), "{report:?}");
                assert_eq!(report.evicted_copies, usize::from(cycle == 1 && pass == 2));
                assert_eq!(review.exists(), cycle == 1 && pass < 2);
                let census = source_census(&f.home);
                assert_eq!(census["current"]["next"], expected_next);
                assert_eq!(census["current"]["completed_at"].is_null(), pass < 2);
                assert_eq!(census["current"]["number"], cycle);
                assert_eq!(
                    census["current"]["runs"]["20260101-000000-0000"]["root"]["outcome"],
                    "error"
                );
                assert_eq!(
                    census["current"]["runs"]["20260101-000000-0001"]["root"]["detail"],
                    "root absent and no eviction records; nothing to reclaim"
                );
                if pass < 2 {
                    let next_id = format!("20260101-000000-{expected_next:04x}");
                    assert_eq!(
                        census["current"]["runs"][next_id]["root"]["outcome"],
                        "not-yet-inspected"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn source_eviction_scan_reaches_old_seats_despite_empty_roots_and_timestamp_churn() {
        let f = Fx::new().await;
        let mut state = inactive_source(&f, "ffff", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        // Empty checkout roots still need ownership checks and consume a
        // discovery slot; mutable timestamp ordering must not move the cursor.
        let mut history = Vec::new();
        for i in 0..130 {
            let mut old = state.clone();
            old.id = format!("20260101-000000-{i:04x}");
            old.candidates.clear();
            old.save_under(&f.home).unwrap();
            std::fs::create_dir_all(old.worktree_root()).unwrap();
            history.push(old);
        }
        state
            .save_under_at(
                &f.home,
                Timestamp::now() + jiff::SignedDuration::from_secs(10),
            )
            .unwrap();
        for pass in 0..3 {
            let mut report = Report::default();
            copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
            assert!(report.failures.is_empty(), "{report:?}");
            let census = source_census(&f.home);
            assert_eq!(
                census["current"]["next"].as_u64().unwrap(),
                ((pass + 1) * 64).min(131)
            );
            if pass < 2 {
                assert!(review.exists());
                assert!(census["current"]["completed_at"].is_null());
                assert_eq!(
                    census["current"]["runs"][&state.id]["root"]["outcome"],
                    "not-yet-inspected"
                );
            }
            // Reorder the records on disk, including the cursor's former ID.
            // Loading a fresh scan on the next invocation models a restart.
            for old in history.iter_mut().take(64) {
                old.updated_at = Timestamp::now();
                old.save_under(&f.home).unwrap();
            }
            if pass == 1 {
                // Reproduce the old cursor's non-progress: moving its timestamp
                // before already visited records makes its next 64 slots revisit
                // those roots, excluding the eligible tail again. Repeating this
                // bookkeeping permutation can keep the tail out indefinitely.
                // Anchor the permutation to the saved tail, so a slow scan
                // cannot move these records past it as wall-clock time advances.
                let older = state.updated_at - jiff::SignedDuration::from_secs(100);
                for old in history.iter_mut().take(128) {
                    if f.home.join("runs").join(&old.id).exists() {
                        old.save_under_at(&f.home, older).unwrap();
                    }
                }
                history[127]
                    .save_under_at(&f.home, older - jiff::SignedDuration::from_secs(100))
                    .unwrap();
                let mut legacy: Vec<_> = history
                    .iter()
                    .filter(|r| f.home.join("runs").join(&r.id).exists())
                    .cloned()
                    .collect();
                legacy.push(state.clone());
                legacy.sort_by_key(|r| r.updated_at);
                let index = legacy.iter().position(|r| r.id == history[127].id).unwrap();
                let len = legacy.len();
                legacy.rotate_left((index + 1) % len);
                assert!(legacy.iter().take(64).all(|r| r.id != state.id));
            }
            if pass == 0 {
                std::fs::remove_dir_all(f.home.join("runs").join(&history[64].id)).unwrap();
                let mut added = state.clone();
                added.id = "20260101-000000-fffe".into();
                added.candidates.clear();
                added.save_under(&f.home).unwrap();
            }
        }
        assert!(!review.exists());
        let census = source_census(&f.home);
        assert!(!census["current"]["completed_at"].is_null());
        assert_eq!(
            census["current"]["runs"][&state.id]["seats"]["review-1"]["outcome"],
            "evicted"
        );
        assert_eq!(
            census["current"]["runs"][&history[64].id]["root"]["outcome"],
            "error"
        );
        let mut next = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut next).await;
        let wrapped = source_census(&f.home);
        assert_eq!(wrapped["current"]["number"], 2);
        assert_eq!(wrapped["previous"], census["current"]);
        assert!(
            wrapped["current"]["runs"]
                .get("20260101-000000-fffe")
                .is_some()
        );
    }

    #[tokio::test]
    async fn source_eviction_scan_resumes_within_run_and_bounds_failed_seats() {
        fn fail_one(path: &Path) -> Result<()> {
            ensure!(
                path.file_name().unwrap() != "review-2",
                "injected checkpoint failure"
            );
            Ok(())
        }
        let f = Fx::new().await;
        let state = inactive_source(&f, "ef20", true).await;
        let mut paths = Vec::new();
        for i in 1..=8 {
            let path = state.worktree_root().join(format!("review-{i}"));
            git::worktree_add_detached(&f.repo, &path, &f.base)
                .await
                .unwrap();
            paths.push(path);
        }
        std::fs::write(paths[2].join("owner-data"), "retain me").unwrap();
        let mut first = Report::default();
        copies::evict_with(&f.home, &Fx::disk(), Fx::now(), &mut first, fail_one).await;
        assert_eq!(first.evicted_copies, 3, "{first:?}");
        let census = source_census(&f.home);
        let seats = &census["current"]["runs"][&state.id]["seats"];
        assert_eq!(census["current"]["after_seat"], "review-5");
        assert_eq!(seats["review-2"]["outcome"], "error");
        assert_eq!(seats["review-2"]["stage"], "unlink");
        assert_eq!(seats["review-3"]["outcome"], "protected");
        assert_eq!(seats["review-6"]["outcome"], "not-yet-inspected");
        assert!(first.source_unlinked_logical_bytes > 0);
        assert!(first.source_evidence_added_logical_bytes > 0);
        let mut second = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut second).await;
        assert_eq!(second.evicted_copies, 3, "{second:?}");
        assert!(paths[1].exists(), "failed seat waits until next cycle");
        assert!(paths[2].exists());
        assert!(!source_census(&f.home)["current"]["completed_at"].is_null());
        let mut third = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut third).await;
        assert_eq!(third.evicted_copies, 1, "{third:?}");
        assert!(!paths[1].exists());
        assert_eq!(
            std::fs::read_to_string(paths[2].join("owner-data")).unwrap(),
            "retain me"
        );
        let shown = seat_inspections(&f.home, &state.id).unwrap().join("\n");
        assert!(
            shown.contains("cycle=1 complete=true review-2 error stage=unlink"),
            "{shown}"
        );
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        let destination = f.bay.join("restored-scan-seat");
        restore_seat(&f.home, &state.id, &f.base, &destination)
            .await
            .unwrap();
        assert_eq!(git::rev_parse(&destination, "HEAD").await.unwrap(), f.base);
    }

    #[tokio::test]
    async fn source_eviction_scan_continues_after_archive_failure_and_busy_output() {
        let f = Fx::new().await;
        let failed = inactive_source(&f, "ef30", true).await;
        let failed_seat = reviewer(&f, &failed, &f.base).await;
        let evidence = f
            .home
            .join("runs")
            .join(&failed.id)
            .join("artifacts/source-evidence");
        std::fs::create_dir_all(&evidence).unwrap();
        std::fs::File::create(evidence.join("reserved-evidence"))
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        let state = inactive_source(&f, "ef31", true).await;
        let busy = reviewer(&f, &state, &f.base).await;
        let available = state.worktree_root().join("review-2");
        git::worktree_add_detached(&f.repo, &available, &f.base)
            .await
            .unwrap();
        let output = OutputLease::take(&busy, true).unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert!(failed_seat.exists());
        assert!(busy.exists());
        assert!(!available.exists());
        let census = source_census(&f.home);
        assert!(!census["current"]["completed_at"].is_null());
        assert_eq!(
            census["current"]["runs"][&failed.id]["seats"]["review-1"]["outcome"],
            "error"
        );
        assert_eq!(
            census["current"]["runs"][&state.id]["seats"]["review-1"]["outcome"],
            "protected"
        );
        drop(output);
        let mut retry = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut retry).await;
        assert_eq!(retry.evicted_copies, 1, "{retry:?}");
        assert!(!busy.exists());
        assert!(failed_seat.exists());
    }

    #[tokio::test]
    async fn source_eviction_scan_checkpoint_failure_stops_before_later_seats() {
        fn block_checkpoint(path: &Path) -> Result<()> {
            let home = path
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("home");
            let cursor = home.join("source-eviction-cursor");
            std::fs::rename(&cursor, home.join("saved-cursor"))?;
            std::fs::create_dir(&cursor)?;
            Ok(())
        }
        let f = Fx::new().await;
        let state = inactive_source(&f, "ef32", true).await;
        let first = reviewer(&f, &state, &f.base).await;
        let second = state.worktree_root().join("review-2");
        git::worktree_add_detached(&f.repo, &second, &f.base)
            .await
            .unwrap();
        let mut report = Report::default();
        copies::evict_with(
            &f.home,
            &Fx::disk(),
            Fx::now(),
            &mut report,
            block_checkpoint,
        )
        .await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert!(!first.exists());
        assert!(second.exists());
        assert!(
            report
                .failures
                .iter()
                .any(|e| e.contains("persist source scan census"))
        );
        assert_eq!(report.source_scan.as_ref().unwrap()["outcome"], "error");
        std::fs::remove_dir(f.home.join("source-eviction-cursor")).unwrap();
        std::fs::rename(
            f.home.join("saved-cursor"),
            f.home.join("source-eviction-cursor"),
        )
        .unwrap();
        let mut retry = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut retry).await;
        assert_eq!(retry.evicted_copies, 1, "{retry:?}");
        assert!(!second.exists());
    }

    #[tokio::test]
    async fn source_eviction_scan_reports_invalid_cursor_and_global_ownership() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ef21", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        let cursor = f.home.join("source-eviction-cursor");
        std::fs::write(&cursor, "broken cursor").unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(std::fs::read_to_string(&cursor).unwrap(), "broken cursor");
        assert_eq!(report.source_scan.as_ref().unwrap()["outcome"], "error");
        assert!(review.exists());
        // Migrate a valid legacy cursor even if its target no longer exists.
        std::fs::write(&cursor, "20260101-000000-abcd").unwrap();
        let mut disabled = Fx::disk();
        disabled.auto_fold = false;
        let mut report = Report::default();
        copies::evict(&f.home, &disabled, Fx::now(), &mut report).await;
        assert_eq!(report.source_scan.as_ref().unwrap()["outcome"], "disabled");
        let record = f.home.join("runs").join(&state.id).join("run.json");
        let original = std::fs::read(&record).unwrap();
        std::fs::write(&record, "invalid ownership").unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert!(
            report
                .failures
                .iter()
                .any(|e| e.contains("global run ownership validation"))
        );
        assert!(review.exists());
        std::fs::write(record, original).unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert_eq!(source_census(&f.home)["version"], 1);
    }

    #[tokio::test]
    async fn source_eviction_batches_reproducible_seats_without_dropping_task_history() {
        let f = Fx::new().await;
        let mut runs = Vec::new();
        let mut paths = Vec::new();
        for tag in ["ec10", "ec11", "ec12", "ec13"] {
            let state = inactive_source(&f, tag, true).await;
            for name in ["judge-1", "advisor-1"] {
                let path = state.worktree_root().join(name);
                git::worktree_add_detached(&f.repo, &path, &f.base)
                    .await
                    .unwrap();
                paths.push(path);
            }
            runs.push(state);
        }
        let task = f.task(&runs);
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 5, "{report:?}");
        let mut next = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut next).await;
        assert_eq!(next.evicted_copies, 3, "{next:?}");
        assert!(paths.iter().all(|p| !p.exists()));
        assert!(runs.iter().all(|r| r.candidates[0].worktree.exists()));
        assert_eq!(
            crate::queue::Queue::at(f.home.join("queue"))
                .get(&task.id)
                .unwrap()
                .runs,
            task.runs
        );
        let limited = Fx::new().await;
        let state = inactive_source(&limited, "ec15", true).await;
        let copy = reviewer(&limited, &state, &limited.base).await;
        let evidence = limited
            .home
            .join("runs")
            .join(&state.id)
            .join("artifacts/source-evidence");
        std::fs::create_dir_all(&evidence).unwrap();
        let retained = evidence.join("reserved-evidence");
        std::fs::File::create(&retained)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        let mut blocked = Report::default();
        copies::evict(&limited.home, &Fx::disk(), Fx::now(), &mut blocked).await;
        assert_eq!(blocked.evicted_copies, 0, "{blocked:?}");
        assert!(copy.exists());
        assert_eq!(
            std::fs::metadata(&retained).unwrap().len(),
            64 * 1024 * 1024
        );
        assert!(
            blocked
                .retained
                .iter()
                .any(|r| r.contains("per-run storage limit")),
            "{blocked:?}"
        );
    }
    #[tokio::test]
    async fn source_eviction_migrates_historical_records_without_rewriting_them() {
        let f = Fx::new().await;
        let mut state = inactive_source(&f, "ec14", true).await;
        historical_review(&mut state, &f.base);
        historical_review(&mut state, &f.base);
        state.status = RunStatus::Superseded;
        state.save_under(&f.home).unwrap();
        let review = reviewer(&f, &state, &f.base).await;
        let task = f.task(std::slice::from_ref(&state));
        let path = f.home.join("runs").join(&state.id).join("run.json");
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["schema"] = 10.into();
        raw.as_object_mut().unwrap().remove("output_activity_at");
        // Historical writers persisted null/empty defaults that current serde
        // omits. These are known fields, not unknown recovery authorities.
        raw["resource_wait"] = serde_json::Value::Null;
        raw["resource_block"] = serde_json::Value::Null;
        raw["driver_claimed"] = serde_json::Value::Null;
        raw["sources"] = serde_json::json!({"start_from":null,"pick_commits":[]});
        // Actual legacy spelling accepted by Graph's serde alias.
        let graph = raw["config"]["graph"].as_object_mut().unwrap();
        let count = graph.remove("implementers").unwrap();
        graph.insert("candidates".into(), count);
        let queue = crate::queue::Queue::at(f.home.join("queue"));
        let task_path = queue.path_of(&task.id);
        let mut task_raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&task_path).unwrap()).unwrap();
        task_raw["sources"] = serde_json::json!({"start_from":null,"pick_commits":[]});
        let task_historical = serde_json::to_vec_pretty(&task_raw).unwrap();
        std::fs::write(&task_path, &task_historical).unwrap();
        let historical = serde_json::to_vec_pretty(&raw).unwrap();
        std::fs::write(&path, &historical).unwrap();
        let report = sweep(&f.home, &Fx::disk(), Fx::now()).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        assert!(!review.exists());
        assert_eq!(std::fs::read(&path).unwrap(), historical);
        assert_eq!(std::fs::read(&task_path).unwrap(), task_historical);
        assert!(state.candidates[0].worktree.exists());
        assert_eq!(
            crate::queue::Queue::at(f.home.join("queue"))
                .get(&task.id)
                .unwrap()
                .runs,
            task.runs
        );
    }
    #[tokio::test]
    async fn source_eviction_archives_old_only_reflog_oid_after_unrewritten_deletion() {
        let f = Fx::new().await;
        let state = inactive_source(&f, "ed01", true).await;
        let review = reviewer(&f, &state, &f.base).await;
        std::fs::write(review.join("old-only"), b"old-only\0binary\xff").unwrap();
        git::git(&review, &["add", "."]).await.unwrap();
        let tree = git::git(&review, &["write-tree"]).await.unwrap();
        let tip = git::git(
            &review,
            &[
                "commit-tree",
                &tree,
                "-p",
                &f.base,
                "-m",
                "temporary detached source",
            ],
        )
        .await
        .unwrap();
        git::git(&review, &["checkout", "--detach", &tip])
            .await
            .unwrap();
        git::git(&review, &["checkout", "--detach", &f.base])
            .await
            .unwrap();
        git::git(&review, &["reflog", "delete", "HEAD@{1}"])
            .await
            .unwrap();
        git::git(&review, &["update-ref", "-d", "ORIG_HEAD"])
            .await
            .unwrap();
        let shown = git::git(&review, &["reflog", "show", "--format=%H", "HEAD"])
            .await
            .unwrap();
        assert!(!shown.lines().any(|oid| oid == tip));
        let admin = PathBuf::from(
            git::git(&review, &["rev-parse", "--absolute-git-dir"])
                .await
                .unwrap(),
        );
        let raw = std::fs::read_to_string(admin.join("logs/HEAD")).unwrap();
        assert!(raw.lines().any(|line| line.starts_with(&format!("{tip} "))));
        fn interrupt(_: &Path) -> Result<()> {
            anyhow::bail!("fixture interruption before unlink")
        }
        let mut pending = Report::default();
        copies::evict_with(&f.home, &Fx::disk(), Fx::now(), &mut pending, interrupt).await;
        assert!(review.exists());
        let ledger = f
            .home
            .join("runs")
            .join(&state.id)
            .join("artifacts/eviction-review-1.json");
        let complete = std::fs::read(&ledger).unwrap();
        let mut incomplete: serde_json::Value = serde_json::from_slice(&complete).unwrap();
        incomplete["history"]
            .as_object_mut()
            .unwrap()
            .remove(&tip)
            .unwrap();
        std::fs::write(&ledger, serde_json::to_vec(&incomplete).unwrap()).unwrap();
        let mut blocked = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut blocked).await;
        assert!(review.exists());
        assert!(
            blocked
                .retained
                .iter()
                .any(|reason| reason.contains("incomplete reflog checkpoint")),
            "{blocked:?}"
        );
        std::fs::write(&ledger, complete).unwrap();
        let mut report = Report::default();
        copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
        assert_eq!(report.evicted_copies, 1, "{report:?}");
        git::git(&f.repo, &["reflog", "expire", "--expire=now", "--all"])
            .await
            .unwrap();
        git::git(&f.repo, &["gc", "--prune=now"]).await.unwrap();
        assert!(
            !git::git_raw(&f.repo, &["cat-file", "-e", &tip])
                .await
                .unwrap()
                .ok()
        );
        std::fs::remove_dir_all(&f.repo).unwrap();
        let destination = f._temp.path().join("restore-old-only");
        restore_seat(&f.home, &state.id, &tip, &destination)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(destination.join("old-only")).unwrap(),
            b"old-only\0binary\xff"
        );
    }

    #[tokio::test]
    async fn source_eviction_crashed_driver_lease_does_not_block_cleanup_or_retry() {
        fn interrupt(_: &Path) -> Result<()> {
            anyhow::bail!("fixture interruption before unlink")
        }
        fn vanish(path: &Path) -> Result<()> {
            std::fs::remove_dir_all(path)?;
            anyhow::bail!("fixture interruption before unregistration")
        }
        for stage in ["ordinary", "intent", "missing-registration", "live-driver"] {
            let f = Fx::new().await;
            let mut state = inactive_source(&f, "ed02", true).await;
            state.driver_exited = false;
            state.driver_claimed = Some(true);
            state.driver_pid = Some(std::process::id());
            state.driver_started_at = crate::proc::process_started_at(std::process::id());
            state.begin_pass();
            state.status = RunStatus::Superseded;
            state.save_under(&f.home).unwrap();
            let review = reviewer(&f, &state, &f.base).await;
            // The same matching identity written by DriverGuard::record, left
            // behind when a process exits without its cleanup guard running.
            let identity = serde_json::json!({"pid":state.driver_pid,"started_at":state.driver_started_at,
                "pass":state.passes.last().unwrap().started_at,"pass_index":state.passes.last().unwrap().index});
            std::fs::write(
                f.home.join("runs").join(&state.id).join("driver.json"),
                serde_json::to_vec(&identity).unwrap(),
            )
            .unwrap();
            let live = if stage == "live-driver" {
                Some(crate::driver::maintenance(&f.home, &state.id).unwrap())
            } else {
                None
            };
            assert_eq!(
                state.liveness_under(&f.home, false),
                if live.is_some() {
                    Liveness::Live
                } else {
                    Liveness::Dead
                }
            );
            let mut report = Report::default();
            if stage == "intent" || stage == "missing-registration" {
                copies::evict_with(
                    &f.home,
                    &Fx::disk(),
                    Fx::now(),
                    &mut report,
                    if stage == "intent" { interrupt } else { vanish },
                )
                .await;
                assert_eq!(report.evicted_copies, 0, "{stage}: {report:?}");
                let mut retry = Report::default();
                copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut retry).await;
                assert!(!review.exists(), "{stage}: {retry:?}");
                let ledger = f
                    .home
                    .join("runs")
                    .join(&state.id)
                    .join("artifacts/eviction-review-1.json");
                let receipt: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(ledger).unwrap()).unwrap();
                assert_eq!(receipt["phase"], "evicted", "{stage}: {retry:?}");
            } else {
                copies::evict(&f.home, &Fx::disk(), Fx::now(), &mut report).await;
                assert_eq!(
                    report.evicted_copies,
                    usize::from(live.is_none()),
                    "{stage}: {report:?}"
                );
                assert_eq!(review.exists(), live.is_some());
            }
            drop(live);
            assert!(
                !RunState::load_under(&state.id, &f.home)
                    .unwrap()
                    .driver_exited
            );
        }
    }
}
