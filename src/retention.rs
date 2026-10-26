//! Bounded reclamation of retained attempts and Cargo outputs. Source recovery
//! and build eviction have separate authority: evicting a target never retires a run.
pub mod reconcile;

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
    _file: File,
}

impl OutputLease {
    #[cfg(unix)]
    pub(crate) fn descriptor(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self._file.as_raw_fd()
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
            .open(path)?;
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
                "worktree outputs are in use or being reclaimed"
            );
        }
        #[cfg(not(unix))]
        let _ = exclusive;
        Ok(Self { _file: file })
    }
}
#[cfg(unix)]
impl Drop for OutputLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: unlock this guard's live descriptor, including inherited copies.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Report {
    pub checked_at: Option<Timestamp>,
    pub limit_bytes: u64,
    pub reclaim_budget_bytes: u64,
    pub measured_bytes: u64,
    pub freed_bytes: u64,
    pub retired: usize,
    pub retained: Vec<String>,
    pub failures: Vec<String>,
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
async fn worktrees(state: &RunState) -> Result<Vec<PathBuf>> {
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
/// Unknown root entries and symlinked cache identity/profile paths fail closed.
async fn cargo_target(worktree: &Path) -> Result<PathBuf> {
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
    for name in ["CACHEDIR.TAG", ".rustc_info.json"] {
        let path = target.join(name);
        ensure!(
            std::fs::symlink_metadata(&path)?.is_file() && path.canonicalize()? == path,
            "cache identity is not a plain file"
        );
    }
    let tag = std::fs::read_to_string(target.join("CACHEDIR.TAG"))?;
    ensure!(
        tag.starts_with("Signature: 8a477f597d28d172789f06886806bc55d"),
        "unknown Cargo cache tag"
    );
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(target.join(".rustc_info.json"))?)?;
    ensure!(
        info.get("rustc_fingerprint").is_some(),
        "unknown Cargo identity"
    );
    for entry in std::fs::read_dir(&target)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("non-UTF8 output")?;
        ensure!(
            matches!(
                name,
                "debug" | "release" | "CACHEDIR.TAG" | ".rustc_info.json"
            ),
            "unrecognized target entry {name}"
        );
        if matches!(name, "debug" | "release") {
            ensure!(
                plain_dir(&entry.path()) && entry.path().canonicalize()? == entry.path(),
                "profile is not a plain directory"
            );
        }
    }
    Ok(target)
}

fn artifact_signature(path: &Path) -> Result<bool> {
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

/// Legacy tags identify a cache, not ownership of every file inside it.
/// Only direct rustc outputs named by dep-info may be reclaimed. Build-script
/// output, incremental caches, unknown files and the manifests themselves stay.
fn declared_outputs(target: &Path) -> Result<Vec<PathBuf>> {
    let mut outputs = std::collections::BTreeSet::new();
    for profile in ["debug", "release"] {
        for dir in [target.join(profile), target.join(profile).join("deps")] {
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

fn record_residue(home: &Path, id: &str, target: &Path, outputs: &[PathBuf]) -> Result<()> {
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
    for profile in ["debug", "release"] {
        let lock = target.join(profile).join(".cargo-lock");
        if lock.exists() {
            ensure!(
                std::fs::symlink_metadata(&lock)?.is_file() && lock.canonicalize()? == lock,
                "Cargo lock is not a plain file"
            );
            let file = OpenOptions::new().read(true).write(true).open(lock)?;
            crate::pr_head::try_lock_file(&file).context("Cargo build is active")?;
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
            targets.push((state.updated_at, id.clone(), path, size));
        }
    }
    targets.sort_by_key(|t| t.0);
    let mut remaining = report.measured_bytes;
    for (updated, id, path, size) in targets {
        if disk.worktree_limit_bytes == 0
            || remaining <= disk.worktree_limit_bytes
                && crate::disk::free_bytes(&path).is_ok_and(|free| free >= disk.min_free_bytes)
        {
            report.retained.push(format!("{id}: output reclamation disabled or aggregate usage and free space are within policy"));
            continue;
        }
        if report.freed_bytes.saturating_add(size) > disk.worktree_reclaim_bytes {
            report.retained.push(format!(
                "{id}: target exceeds remaining per-pass byte budget"
            ));
            continue;
        }
        let result = async {
            ensure!(
                crate::clean::due(now, updated, disk.fold_grace_secs),
                "within grace period"
            );
            let _owner = crate::pr_head::owner_lock(home, &id)?;
            let state = RunState::load_under(&id, home)?;
            ensure!(
                state.liveness_under(home, crate::daemon::is_working_on(home, &id, now))
                    == Liveness::Dead,
                "run live or ownership unknown"
            );
            ensure!(crate::clean::due(now, state.updated_at, disk.fold_grace_secs), "recently used after inventory");
            let _driver = crate::driver::maintenance(home, &id)?;
            let _outputs = OutputLease::take(&path, true)?;
            ensure!(
                worktrees(&state).await?.contains(&path),
                "worktree registration changed"
            );
            let target = cargo_target(&path).await?;
            let _cargo = cargo_locks(&target)?;
            let outputs = declared_outputs(&target)?;
            ensure!(!outputs.is_empty(), "no dep-info-proven outputs; unknown files retained");
            let current_size = outputs.iter().try_fold(0u64, |bytes, p| {
                Ok::<_, std::io::Error>(bytes.saturating_add(std::fs::metadata(p)?.len()))
            })?;
            ensure!(report.freed_bytes.saturating_add(current_size) <= disk.worktree_reclaim_bytes,
                "target grew beyond remaining per-pass budget");
            // Record adoption before deletion; interrupted deletion is revalidated
            // on the next pass. No source file or branch is modified.
            let ledger = home.join("runs").join(&id).join("build-outputs.json");
            std::fs::write(
                ledger,
                serde_json::to_vec(
                    &serde_json::json!({"target":target,"bytes":current_size,"at":now,"action":"reclaim-declared-outputs","files":outputs}),
                )?,
            )?;
            // Unknown leftovers still permit artifact-only eviction, but never
            // authorize retirement. Only a complete proven residue is adopted.
            if let Err(error) = record_residue(home, &id, &target, &outputs) {
                report.retained.push(format!("{id}: residue not adopted for retirement: {error:#}"));
            }
            for output in &outputs {
                std::fs::remove_file(output)?;
            }
            measurements.remove(&target);
            report.retained.push(format!("{id}: unmanifested files and Cargo metadata retained"));
            Ok::<_, anyhow::Error>(current_size)
        }
        .await;
        match result {
            Ok(freed) => {
                report.freed_bytes = report.freed_bytes.saturating_add(freed);
                remaining = remaining.saturating_sub(freed);
            }
            Err(e) => report.retained.push(format!(
                "{id}/{}: {e:#}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    measurements.retain(|path, _| observed.contains(path));
    if let Ok(body) = serde_json::to_vec(&measurements)
        && let Err(e) = std::fs::write(&measurements_path, body)
    {
        report.failures.push(format!("output measurements: {e}"));
    }
    retire_empty(home, disk, now, &mut report).await;
    if !report.failures.is_empty()
        || disk.worktree_limit_bytes > 0
            && remaining > disk.worktree_limit_bytes
            && report.freed_bytes == 0
    {
        crate::notices::raise_in(
            home,
            crate::notices::Notice::warn(
                "disk:retention",
                "Retained build outputs could not be brought under the disk cap. Inspect the cleanup report for active owners, retained recovery sources and failures.",
            ),
        );
    }
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
    let open = questions.list();
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
async fn tracked_base(worktree: &Path, base: &str) -> Result<()> {
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
        files.push(path.to_str().context("non UTF-8 source")?.to_owned());
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
            .list()
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
                "Signature: 8a477f597d28d172789f06886806bc55d\n",
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
    async fn unknown_targets_cargo_owners_and_driver_resumes_are_protected() {
        let f = Fx::new().await;
        let state = f.run("cccc", false).await;
        let wt = &state.candidates[0].worktree;
        Fx::target(wt);
        std::fs::write(wt.join("target/notes"), "user file").unwrap();
        assert!(cargo_target(wt).await.is_err());
        std::fs::remove_file(wt.join("target/notes")).unwrap();
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
            let mut running = crate::seat_process::Running::spawn(
                cmd,
                None,
                vec![
                    OutputLease::borrow(wt).unwrap().unwrap(),
                    OutputLease::cache(&f.home, &cache, false).unwrap(),
                ],
            )
            .unwrap();
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
}
