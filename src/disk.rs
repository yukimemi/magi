//! Disk accounting: how much magi's own directories occupy, how much space is
//! left on the volume they live on, and how the shared build cache is pruned.
//!
//! The whole module grew out of one incident: a machine with 951.8 GB of disk
//! ran a handful of competitions and ended up with 6.7 GB free and a pile of
//! multi-gigabyte `target/` directories. Every function here exists to keep
//! that from being a discovery, and every number is substituted at a pure
//! boundary so the policy can be tested without asking the OS anything.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

#[cfg(unix)]
use {crate::proc::Quiet as _, anyhow::bail};

/// Are `free` bytes above the floor for starting a run?
///
/// Pure on purpose: the threshold logic is asserted against injected numbers,
/// and the only place the machine is actually asked anything is [`free_bytes`].
pub fn enough_space(free: u64, min_free: u64) -> bool {
    free >= min_free
}

/// Why a run must not start, given measured free bytes and the floor —
/// `None` means the gate is open. Pure, so the policy is asserted directly.
///
/// A gate that cannot measure also closes (see [`crate::daemon::disk_gate`]):
/// starting a run on a disk that may already be full is the incident this
/// whole module exists to prevent.
pub fn gate(free: u64, min_free: u64) -> Option<String> {
    gate_in(free, min_free, "en")
}

/// Prefix of [`gate_in`]'s reason, per language. [`is_gate_reason`] matches
/// both, so translating the text cannot stop triage's auto-requeue.
const SHORT_EN: &str = "not enough free space to start a run:";
const SHORT_JA: &str = "run を開始するための空き容量が不足しています:";
/// Prefix of a resource hold after emergency writer termination.
pub const RUNNING_PRESSURE: &str = "disk pressure while a run was writing: ";
/// Prefix of a failed free-space measurement.
pub const UNMEASURED_EN: &str = "could not measure free space on ";
/// See [`UNMEASURED_EN`].
pub const UNMEASURED_JA: &str = "空き容量を測定できませんでした: ";

/// [`gate`], with the reason in `language` (`en`, or Japanese; anything else
/// is English).
pub fn gate_in(free: u64, min_free: u64, language: &str) -> Option<String> {
    if enough_space(free, min_free) {
        None
    } else if crate::lang::is_japanese(language) {
        Some(format!(
            "{SHORT_JA} 空き {free} バイト、`[disk] min_free_bytes` の要求は {min_free} バイト"
        ))
    } else {
        Some(format!(
            "{SHORT_EN} {free} bytes free, \
             {min_free} required by `[disk] min_free_bytes`"
        ))
    }
}

/// The reason for a measurement that failed, in `language`.
pub fn unmeasured_in(
    path: &std::path::Path,
    err: &dyn std::fmt::Display,
    language: &str,
) -> String {
    if crate::lang::is_japanese(language) {
        format!(
            "{UNMEASURED_JA}{} ({err}); ディスクゲートは測定できないまま run を開始させません",
            path.display()
        )
    } else {
        format!(
            "{UNMEASURED_EN}{} ({err}); the disk gate refuses \
             to let a run start blind",
            path.display()
        )
    }
}

/// Is `reason` one of this module's disk-gate reasons, in either language?
/// What triage reads back to decide a hold is disk pressure.
pub fn is_gate_reason(reason: &str) -> bool {
    fn numbers(text: &str, middle: &str, suffix: &str) -> bool {
        text.strip_suffix(suffix)
            .and_then(|s| s.split_once(middle))
            .is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().all(|c| c.is_ascii_digit())
                    && b.bytes().all(|c| c.is_ascii_digit())
                    && matches!((a.parse::<u64>(), b.parse::<u64>()), (Ok(free), Ok(required)) if free < required)
            })
    }
    reason
        .strip_prefix(&format!("{SHORT_EN} "))
        .is_some_and(|s| numbers(s, " bytes free, ", " required by `[disk] min_free_bytes`"))
        || reason
            .strip_prefix(&format!("{SHORT_JA} 空き "))
            .is_some_and(|s| numbers(s, " バイト、`[disk] min_free_bytes` の要求は ", " バイト"))
        || [
            (
                UNMEASURED_EN,
                "); the disk gate refuses to let a run start blind",
            ),
            (
                UNMEASURED_JA,
                "); ディスクゲートは測定できないまま run を開始させません",
            ),
        ]
        .iter()
        .any(|(prefix, suffix)| {
            reason
                .strip_prefix(prefix)
                .and_then(|s| s.strip_suffix(suffix))
                .is_some_and(|s| {
                    s.split_once(" (").is_some_and(|(path, error)| {
                        !path.is_empty() && !error.is_empty() && !s.contains('\n')
                    })
                })
        })
}

/// Is `size` past `limit`? One comparison, shared by the janitor and the
/// health view, so both answer "is the cache over its cap" identically.
pub fn over_limit(size: u64, limit: u64) -> bool {
    size > limit
}

/// A cache's target size under its cap and the volume's free space: the one
/// rule every caller of pressure collection goes through.
///
/// Three bands, by `free` against the admission floor `min_free` and the
/// collection threshold `threshold`
/// ([`Disk::reclaim_threshold`](crate::config::Disk::reclaim_threshold)):
///
/// * `free >= threshold`: nothing is short; the limit is the configured cap
///   (never more than `size`, so nothing is asked to shrink).
/// * `min_free <= free < threshold`: the limit shrinks by the deficit
///   (`threshold - free`) but never below a **reserve**, half of `cap`
///   (see [`pressure_reserve`]; never above the cache's own `size`, so a cache
///   at or below its reserve is left alone). A build cache is only worth
///   anything while some of it is left: on a disk whose free space other
///   software controls, a rule without a reserve would empty every cache on
///   every pass and start every build cold. The ratio is fixed on purpose,
///   not a setting. The limit alone does not keep the reserve: the pruning
///   routine is also given the reserve as a bound ([`prune_dir_keeping`]) and
///   refuses any removal that would cross it.
/// * `free < min_free`: below the admission floor work is held anyway and
///   getting space back outranks a warm cache, so this is exactly the rule
///   from before the threshold existed: the limit shrinks by the deficit
///   against `min_free`, down to zero if need be.
///
/// A zero `cap` yields zero here; callers that treat a zero cap as "pruning
/// off" check it before asking. The limit is an *estimate*: the caller removes
/// that many logical bytes oldest-first and does not re-measure within the
/// pass (see [`PressureOutcome`]).
pub fn pressure_limit(size: u64, cap: u64, min_free: u64, threshold: u64, free: u64) -> u64 {
    if free >= threshold {
        return cap.min(size);
    }
    if free < min_free {
        return cap.min(size.saturating_sub(min_free - free));
    }
    let reserve = pressure_reserve(cap, min_free, threshold, free).min(size);
    cap.min(reserve.max(size.saturating_sub(threshold - free)))
}

/// The size a build cache must keep while free space is between the admission
/// floor and the collection threshold: half its configured `cap`. Zero in
/// every other band (at or above the threshold nothing is collected beyond the
/// cap; below the floor getting space back outranks a warm cache), which is
/// also what makes manual `magi cache prune` and the plain cap unchanged.
pub fn pressure_reserve(cap: u64, min_free: u64, threshold: u64, free: u64) -> u64 {
    if free >= min_free && free < threshold {
        cap / 2
    } else {
        0
    }
}

/// How one pressure-collection pass over a build cache ended, as the pass's
/// log line says it. None of them is an error: a pass that cannot make
/// progress says so and the service's next period tries again.
///
/// A pass is *estimated-deficit collection*: it measures free space once,
/// computes the deficit against the collection threshold, and removes that
/// many *logical* bytes oldest-first, bounded by [`pressure_limit`]. It does
/// not re-measure in a loop. Hard links and other writers mean the space
/// actually freed can be less (or more); the next pass measures again and
/// continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureOutcome {
    /// Maintenance could not take the cache (a build holds it); nothing was
    /// inspected or removed.
    LeaseHeld,
    /// Files were removed.
    Collected,
    /// Nothing needed removing, because the cache is at or below the reserve
    /// it keeps while free space is below the collection threshold.
    AtReserve,
    /// The cache was over its limit but no file could be removed: none left,
    /// all of them protected by the reserve, or every removal failed.
    NothingEligible,
    /// The cache was within its limit; nothing needed removing.
    WithinLimit,
}

impl PressureOutcome {
    /// The wording of the log line.
    pub fn as_str(self) -> &'static str {
        match self {
            PressureOutcome::LeaseHeld => "lease held",
            PressureOutcome::Collected => "collected",
            PressureOutcome::AtReserve => "at reserve",
            PressureOutcome::NothingEligible => "nothing eligible",
            PressureOutcome::WithinLimit => "within limit",
        }
    }
}

/// Classify a pass, maintenance refusals first. `blocked` is whether
/// maintenance could not take the cache; `removed` the logical bytes the pass
/// removed; `size` the cache's size going into the deletion (the locked
/// inventory's when one was taken, else the estimate the limit came from);
/// `limit` what [`pressure_limit`] allowed; `keep` the reserve the pruning was
/// bounded by ([`pressure_reserve`]).
pub fn pressure_outcome(
    blocked: bool,
    removed: u64,
    size: u64,
    limit: u64,
    keep: u64,
) -> PressureOutcome {
    if blocked {
        PressureOutcome::LeaseHeld
    } else if removed > 0 {
        PressureOutcome::Collected
    } else if size > limit {
        PressureOutcome::NothingEligible
    } else if keep > 0 && size > 0 && size <= keep {
        PressureOutcome::AtReserve
    } else {
        PressureOutcome::WithinLimit
    }
}

/// The path a rendered command sets `CARGO_TARGET_DIR=` to, if any.
///
/// magi never computes the cache path itself. The operator's `magi.toml` is
/// the only place that knows it, and by the time a [`crate::config::Config`]
/// exists that template has been rendered — so the concrete path is read back
/// out of the verify commands (`CARGO_TARGET_DIR={{ vars.cache }}/magi-target
/// cargo …` becomes `C:\…\Temp\magi-target`). This is what lets the janitor
/// prune exactly the directory the gate and the seats build into. `None` when
/// no command sets the variable: there is then no cache to aggregate or prune,
/// and agents build wherever the repository's own defaults put them.
///
/// The value may be quoted with `'` or `"`; both are understood, as is no
/// quoting (up to the next whitespace).
pub fn extract_cargo_target_dir(command: &str) -> Option<PathBuf> {
    const KEY: &str = "CARGO_TARGET_DIR=";
    let rest = command.split_once(KEY)?.1.trim_start();
    let value = if let Some(s) = rest.strip_prefix('\'') {
        s.split('\'').next().unwrap_or("")
    } else if let Some(s) = rest.strip_prefix('"') {
        s.split('"').next().unwrap_or("")
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        &rest[..end]
    };
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

/// Free bytes on the volume containing `path`.
///
/// Unix parses `df`; Windows asks the volume directly through kernel32, without
/// starting a subprocess on the synchronous health-request path.
pub fn free_bytes(path: &Path) -> Result<u64> {
    free_bytes_by_os(path)
}

/// Free bytes on the volume containing `path`.
#[cfg(unix)]
fn free_bytes_by_os(path: &Path) -> Result<u64> {
    let out = std::process::Command::new("df")
        .args(["-k", "-P"])
        .arg(path)
        .quiet()
        .output()
        .with_context(|| format!("run `df` for {}", path.display()))?;
    if !out.status.success() {
        bail!(
            "`df` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .skip(1)
        .find_map(parse_df_available)
        .with_context(|| format!("parse `df` output for {}", path.display()))
}

/// Free bytes on the volume containing `path`.
#[cfg(windows)]
fn free_bytes_by_os(path: &Path) -> Result<u64> {
    let abs = std::path::absolute(path)
        .with_context(|| format!("absolute path for {}", path.display()))?;
    let root = volume_root(&abs)
        .with_context(|| format!("no volume root in {} to measure", abs.display()))?;
    windows_free_bytes(Path::new(&root))
}

pub(crate) fn managed_free_bytes(path: &Path) -> Result<u64> {
    #[cfg(windows)]
    return windows_free_bytes(path);
    #[cfg(not(windows))]
    free_bytes(path)
}

#[cfg(windows)]
fn windows_free_bytes(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt as _;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0_u64;
    // SAFETY: wide is a live NUL-terminated UTF-16 root, available is writable,
    // and the optional totals are null. The API retains none of these pointers.
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return Err(error).with_context(|| format!("query free bytes for {}", path.display()));
    }
    // Caller-available bytes, not total free bytes: preserve quota semantics.
    Ok(available)
}

/// One `df -k -P` data row: `Filesystem 1024-blocks Used Available …`.
///
/// The value is 1024-byte blocks, so the parse returns bytes.
pub fn parse_df_available(line: &str) -> Option<u64> {
    let mut fields = line.split_whitespace();
    fields.next()?; // filesystem
    fields.next()?; // 1024-blocks
    fields.next()?; // used
    let blocks: u64 = fields.next()?.parse().ok()?;
    Some(blocks.saturating_mul(1024))
}

/// The drive root used for querying an absolute Windows path:
/// `C:\`, never `C:\Users\...` and never a verbatim `\\?\C:\...`.
///
/// Pure and platform-independent so the verbatim form - which is what the
/// queue stores and what closed the disk gate on every task - is asserted
/// without a Windows runner. `None` when there is no drive letter to name:
/// UNC shares remain unsupported, rather than inventing a drive volume.
pub fn volume_root(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    // Verbatim (`\\?\C:\x`) and verbatim-UNC (`\\?\UNC\server\share`) prefixes.
    let bare = text
        .strip_prefix(r"\\?\")
        .or_else(|| text.strip_prefix("//?/"))
        .unwrap_or(text);
    let mut chars = bare.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() || chars.next()? != ':' {
        return None;
    }
    Some(format!(r"{letter}:\"))
}

/// Total bytes under `path`, without following symlinks.
///
/// A symlinked directory counts as the link itself, not its contents: mutable
/// worktrees are real directories, and following an accidental link into a
/// clone of the repository would count the same bytes twice.
pub fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            // `DirEntry::metadata` reports the entry itself, so a symlink is
            // never traversed.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// What a prune removed, for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Prune {
    /// Bytes actually freed.
    pub freed: u64,
    /// Files deleted.
    pub files: usize,
    /// Bytes still under the directory afterwards.
    pub remaining: u64,
}

/// What [`plan_prune`] would remove, without removing it.
///
/// Oldest-first, the same order [`prune_dir`] deletes in — a caller previewing
/// the plan (`magi cache prune --dry-run`) must see exactly what an immediate
/// `prune_dir` call would do, not an approximation of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    /// Files this plan would delete, oldest-first, paired with their size.
    pub files: Vec<(PathBuf, u64)>,
    /// Bytes this plan would free, assuming every listed file is removable.
    pub freed: u64,
    /// Bytes that would remain under the directory once the plan applies
    /// cleanly.
    pub remaining: u64,
}

/// The selection [`prune_dir`] would act on *if every one of these deletions
/// succeeds*, computed without touching disk.
///
/// Read-only on purpose: a preview must never take the cache's lease (see
/// `cache::maintenance_prune`'s doc) — it does not write anything, so it
/// cannot race a build the way a real prune would, and a caller only wanting
/// to show an operator a plan should not have to wait out contention to do
/// it. This is necessarily an idealized selection, not a prediction of every
/// file [`prune_dir`] will end up touching: a build finishing between the
/// preview and a real prune can change what gets deleted, and so can a single
/// locked file on the real pass, which - unlike this preview - has to keep
/// reaching past its own plan for another (newer) file when an older one it
/// counted on turns out to be unremovable. The CLI surface that shows this
/// preview says so.
#[must_use]
pub fn plan_prune(dir: &Path, limit: u64) -> PrunePlan {
    let Some(tree) = Tree::of(dir) else {
        return PrunePlan::default();
    };
    let mut total = tree.total;
    if !over_limit(total, limit) {
        return PrunePlan {
            files: Vec::new(),
            freed: 0,
            remaining: total,
        };
    }
    let mut freed = 0u64;
    let mut files = Vec::new();
    for (_, size, path) in &tree.files {
        if !over_limit(total, limit) {
            break;
        }
        total = total.saturating_sub(*size);
        freed += *size;
        files.push((path.clone(), *size));
    }
    PrunePlan {
        files,
        freed,
        remaining: total,
    }
}

/// Delete files under `dir` oldest-first until its size is at or below `limit`.
///
/// The comparison is [`over_limit`], so a directory exactly at the cap is left
/// alone. Oldest-first keeps the newest generation of artifacts — the one the
/// next run reuses — and sheds the generations that only compile history. A
/// deleted file costs the next build a rebuild of that one unit; deleting the
/// whole directory would cost it everything, which is precisely the work
/// [`prune_dir`] is keeping for it.
///
/// Empty directories left behind are swept depth-first, so cargo's deep
/// `fingerprint`/`deps` trees do not outlive the files that made them.
///
/// Nothing is deleted when the directory is missing.
pub fn prune_dir(dir: &Path, limit: u64) -> Result<Prune> {
    prune_dir_keeping(dir, limit, 0)
}

/// [`prune_dir`] that never takes the directory below `keep` bytes.
///
/// Pressure collection between the admission floor and the collection
/// threshold keeps a reserve of a build cache ([`pressure_reserve`]): a limit
/// alone cannot do that, because files are removed whole and one large
/// artifact can be larger than `size - reserve`. A file whose removal would
/// leave less than `keep` is *skipped*, and the walk goes on to later (newer)
/// candidates while the directory is still over `limit`. The inventory is the
/// one this call takes itself, under whatever maintenance locks the caller
/// holds. `keep == 0` is exactly [`prune_dir`].
pub fn prune_dir_keeping(dir: &Path, limit: u64, keep: u64) -> Result<Prune> {
    let Some(tree) = Tree::of(dir) else {
        return Ok(Prune {
            freed: 0,
            files: 0,
            remaining: 0,
        });
    };
    let mut total = tree.total;
    if !over_limit(total, limit) {
        return Ok(Prune {
            freed: 0,
            files: 0,
            remaining: total,
        });
    }
    let mut freed = 0u64;
    let mut removed = 0usize;
    // Deliberately walks every file in `tree`, not a fixed plan computed up
    // front: `total` only drops on a successful removal, so a file that is
    // being read elsewhere (a concurrent build, a snapshot) and fails to
    // delete on Windows costs this pass nothing but that one file - it is
    // skipped, and the loop keeps reaching for the next-oldest file until the
    // real, achieved total is at or below `limit` or there is nothing left to
    // try. A version of this that instead deleted only a pre-computed
    // selection would stop short of the cap on the first locked file, every
    // pass, on exactly the machines where locked files are common.
    for (_, size, path) in &tree.files {
        if !over_limit(total, limit) {
            break;
        }
        if total.saturating_sub(*size) < keep {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*size);
            freed += *size;
            removed += 1;
        }
    }
    strip_empty_dirs(&tree.dirs);
    Ok(Prune {
        freed,
        files: removed,
        remaining: total,
    })
}

/// The one file a prune never selects, when it sits directly in the pruned root.
///
/// cargo writes `CACHEDIR.TAG` once, when it creates a target directory, so in
/// a long-lived cache it is the oldest file and an oldest-first prune reaches it
/// first. Without it `cargo clean -p` refuses ("missing or invalid
/// `CACHEDIR.TAG` file"). The tag belongs to the cargo that made it, not to the
/// prune. It still counts toward the directory's size; a same-named file in a
/// subdirectory is an ordinary candidate.
const PRESERVED_AT_ROOT: &str = "CACHEDIR.TAG";

/// Files and directories under one root, walked up-front.
///
/// `files` are the deletion candidates; `total` also counts what is preserved.
struct Tree {
    total: u64,
    files: Vec<(u128, u64, PathBuf)>,
    dirs: Vec<(usize, PathBuf)>,
}

impl Tree {
    /// Walk `dir`, collecting files (mtime-nanoseconds, size, path) and
    /// directories (depth, path). `None` when the directory does not exist.
    fn of(dir: &Path) -> Option<Tree> {
        if dir.symlink_metadata().ok()?.is_dir() {
            Some(Tree::from_dir(dir))
        } else {
            None
        }
    }

    fn from_dir(dir: &Path) -> Tree {
        let mut total = 0u64;
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        // Depth-first so directories are recorded before their contents; the
        // dir list is then sorted by descending depth for the sweep.
        let mut stack: Vec<(usize, PathBuf)> = vec![(0, dir.to_path_buf())];
        while let Some((depth, d)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                let Ok(meta) = path.symlink_metadata() else {
                    continue;
                };
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt as _;
                    if meta.file_attributes() & 0x400 != 0 {
                        continue;
                    }
                }
                if meta.is_dir() {
                    dirs.push((depth + 1, path.clone()));
                    stack.push((depth + 1, path));
                } else if meta.is_file() {
                    let size = meta.len();
                    total += size;
                    if depth == 0
                        && (entry.file_name() == PRESERVED_AT_ROOT
                            || entry.file_name() == crate::build_target::TARGET_PROOF)
                    {
                        continue;
                    }
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    files.push((mtime, size, path));
                }
            }
        }
        // Oldest first, and on a tie the larger file: a whole generation of
        // cargo artifacts is written within one filesystem timestamp tick, so
        // mtime alone leaves the order to `read_dir` and the sort's
        // instability - the same cache pruned twice would shed different
        // files, and a test over two same-tick files passed on one platform
        // and failed on another. Larger-first also reaches the cap in fewer
        // deletions, which is fewer rebuilt units for the next run.
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        Tree { total, files, dirs }
    }
}

/// Remove empty directories, deepest first, never the root itself.
fn strip_empty_dirs(dirs: &[(usize, PathBuf)]) {
    let mut by_depth: Vec<&PathBuf> = dirs.iter().map(|(_, d)| d).collect();
    by_depth.sort_unstable_by_key(|d| std::cmp::Reverse(d.iter().count()));
    for d in by_depth {
        let _ = std::fs::remove_dir(d);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gate_reasons_follow_the_language_and_stay_recognisable() {
        let en = gate_in(1, 10, "en").unwrap();
        let ja = gate_in(1, 10, "日本語").unwrap();
        assert_eq!(gate_in(1, 10, "fr").unwrap(), en);
        assert_eq!(en, gate(1, 10).unwrap());
        assert!(ja.starts_with("run を開始"), "{ja}");
        assert!(is_gate_reason(&en) && is_gate_reason(&ja));
        let p = std::path::Path::new("/x");
        let m_ja = unmeasured_in(p, &"boom", "ja");
        assert!(m_ja.contains("測定できませんでした") && is_gate_reason(&m_ja));
        assert!(is_gate_reason(&unmeasured_in(p, &"boom", "en")));
        assert!(!is_gate_reason("something else"));
    }

    #[test]
    fn disk_wait_legacy_reasons_require_the_complete_gate_format() {
        let observed = "run を開始するための空き容量が不足しています: 空き 1477050368 バイト、`[disk] min_free_bytes` の要求は 3221225472 バイト";
        assert!(is_gate_reason(observed));
        assert!(is_gate_reason(
            &gate_in(1477050368, 3221225472, "en").unwrap()
        ));
        for ambiguous in [
            format!("{observed}\nowner: hold"),
            format!("{SHORT_EN} inspect manually"),
            format!("{RUNNING_PRESSURE}unknown"),
            format!("{UNMEASURED_EN}/tmp (error)"),
        ] {
            assert!(!is_gate_reason(&ambiguous), "{ambiguous}");
        }
    }

    use super::*;
    use std::fs;

    #[test]
    fn the_free_space_predicate_is_the_boundary() {
        assert!(enough_space(100, 100));
        assert!(enough_space(101, 100));
        assert!(!enough_space(99, 100));
        // A zero floor disables the gate: the operator opted out.
        assert!(enough_space(0, 0));
    }

    #[test]
    fn the_gate_text_conveys_both_numbers_and_opens_with_room() {
        assert_eq!(
            gate(9, 10).expect("closed"),
            "not enough free space to start a run: 9 bytes free, 10 required by `[disk] min_free_bytes`"
        );
        assert_eq!(gate(10, 10), None, "exactly at the floor is open");
        assert_eq!(gate(10_000, 0), None, "a zero floor is an opt-out");
    }

    #[test]
    fn over_limit_uses_strict_greater_than() {
        assert!(over_limit(11, 10));
        assert!(!over_limit(10, 10));
        assert!(!over_limit(9, 10));
    }

    #[test]
    fn df_row_parses_1024_blocks_into_bytes() {
        let row = "/dev/sda1 976762584 808522388 168240196 83% /home";
        assert_eq!(parse_df_available(row), Some(168_240_196 * 1024));
        assert_eq!(parse_df_available("garbage"), None);
        assert_eq!(parse_df_available("a b c x"), None);
    }

    /// The volume query takes a drive root; the queue hands out verbatim paths.
    #[test]
    fn the_volume_root_is_a_drive_not_the_path_it_came_from() {
        // The form that closed the gate on every queued task: the queue
        // records the repo as `\\?\C:\...`.
        assert_eq!(
            volume_root(Path::new(
                r"\\?\C:\Users\yukimemi\src\github.com\yukimemi\magi"
            )),
            Some(r"C:\".to_owned())
        );
        assert_eq!(
            volume_root(Path::new(r"C:\Users\yukimemi")),
            Some(r"C:\".to_owned())
        );
        assert_eq!(volume_root(Path::new(r"D:\")), Some(r"D:\".to_owned()));
        // Forward slashes reach magi from configs written by hand.
        assert_eq!(
            volume_root(Path::new("C:/Users/yukimemi/src")),
            Some(r"C:\".to_owned())
        );
        // No drive to name: UNC shares are unsupported, and a POSIX path has no
        // drive volume. The caller has to report that it cannot measure.
        assert_eq!(volume_root(Path::new(r"\\server\share\dir")), None);
        assert_eq!(volume_root(Path::new(r"\\?\UNC\server\share")), None);
        assert_eq!(volume_root(Path::new("/home/yukimemi")), None);
    }

    #[test]
    fn the_cache_dir_is_read_back_out_of_a_rendered_command() {
        let cmd = r"CARGO_TARGET_DIR=C:\Users\me\Temp\magi-target cargo make check";
        assert_eq!(
            extract_cargo_target_dir(cmd),
            Some(PathBuf::from(r"C:\Users\me\Temp\magi-target"))
        );
        // Quoted forms survive spaces; a config with none stays None.
        assert_eq!(
            extract_cargo_target_dir(r"CARGO_TARGET_DIR='/tmp/a b' cargo test"),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(
            extract_cargo_target_dir(r#"CARGO_TARGET_DIR="/tmp/qq" cargo test"#),
            Some(PathBuf::from("/tmp/qq"))
        );
        assert_eq!(extract_cargo_target_dir("cargo make check"), None);
        assert_eq!(extract_cargo_target_dir("CARGO_TARGET_DIR="), None);
        // Second occurrence is irrelevant: the first is what the build used
        // (a command's environment applies once).
        let two = "CARGO_TARGET_DIR=/first and CARGO_TARGET_DIR=/second cargo x";
        assert_eq!(extract_cargo_target_dir(two), Some(PathBuf::from("/first")));
    }

    #[test]
    fn dir_size_is_zero_for_missing_and_counts_files_without_following_links() {
        let t = tempfile::TempDir::new().expect("temp");
        assert_eq!(dir_size(&t.path().join("nope")), 0);
        fs::write(t.path().join("a"), b"12345").expect("write");
        fs::create_dir(t.path().join("sub")).expect("dir");
        fs::write(t.path().join("sub").join("b"), b"678").expect("write");
        assert_eq!(dir_size(t.path()), 8);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(t.path().join("sub"), t.path().join("link"))
                .expect("symlink");
            assert_eq!(dir_size(t.path()), 8, "a link is counted as a link");
        }
    }

    #[test]
    fn prune_deletes_oldest_first_until_the_cap_is_met() {
        let t = tempfile::TempDir::new().expect("temp");
        let old = t.path().join("old");
        fs::write(&old, b"yyyy").expect("write");
        // Give the older file a measurably older mtime; a second is past the
        // granularity of the filesystems magi runs on.
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        fs::write(t.path().join("new"), b"xxxxx").expect("write");

        // Cap above the total: nothing moves.
        let keep = prune_dir(t.path(), 9).expect("prune");
        assert_eq!(
            keep,
            Prune {
                freed: 0,
                files: 0,
                remaining: 9
            }
        );

        // Cap below: the oldest file goes, the new one stays.
        let pruned = prune_dir(t.path(), 6).expect("prune");
        assert!(pruned.freed > 0);
        assert_eq!(pruned.files, 1);
        assert_eq!(pruned.remaining, 5);
        assert!(!old.exists(), "the older file is the one shed");
        assert!(t.path().join("new").exists());
    }

    #[test]
    fn plan_prune_selects_what_prune_dir_would_delete_without_deleting_it() {
        let t = tempfile::TempDir::new().expect("temp");
        let old = t.path().join("old");
        fs::write(&old, b"yyyy").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        fs::write(t.path().join("new"), b"xxxxx").expect("write");

        let plan = plan_prune(t.path(), 6);
        assert_eq!(plan.files, vec![(old.clone(), 4)]);
        assert_eq!(plan.freed, 4);
        assert_eq!(plan.remaining, 5);
        assert!(old.exists(), "a plan never deletes anything");
        assert!(t.path().join("new").exists());

        // Applying `prune_dir` afterwards removes exactly what the plan named.
        let pruned = prune_dir(t.path(), 6).expect("prune");
        assert_eq!(pruned.freed, plan.freed);
        assert_eq!(pruned.remaining, plan.remaining);
        assert!(!old.exists());
    }

    #[test]
    fn a_root_cachedir_tag_survives_even_as_the_oldest_file() {
        // cargo writes the tag once, so in a long-lived cache it is the oldest
        // file and used to be the first thing an over-cap prune deleted.
        let t = tempfile::TempDir::new().expect("temp");
        let tag = t.path().join("CACHEDIR.TAG");
        fs::write(&tag, b"Signature: x").expect("write");
        let f = fs::File::options().write(true).open(&tag).expect("open");
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .expect("mtime");
        drop(f);
        fs::write(t.path().join("a"), b"aaaa").expect("write");
        fs::create_dir(t.path().join("sub")).expect("mkdir");
        // A same-named file below the root is an ordinary candidate.
        let nested = t.path().join("sub").join("CACHEDIR.TAG");
        fs::write(&nested, b"nn").expect("write");

        let limit = 12; // exactly the tag: every other file must go
        let plan = plan_prune(t.path(), limit);
        assert!(plan.files.iter().all(|(p, _)| p != &tag));
        assert!(plan.remaining <= limit);

        let pruned = prune_dir(t.path(), limit).expect("prune");
        assert!(tag.exists(), "the root tag is never pruned");
        assert!(pruned.remaining <= limit);
        assert_eq!(pruned.freed, plan.freed);
        assert_eq!(pruned.remaining, plan.remaining);
        assert!(!nested.exists());
    }

    #[test]
    fn plan_prune_is_empty_under_the_cap_and_for_a_missing_dir() {
        let t = tempfile::TempDir::new().expect("temp");
        fs::write(t.path().join("a"), b"12345").expect("write");
        let plan = plan_prune(t.path(), 100);
        assert_eq!(
            plan,
            PrunePlan {
                files: Vec::new(),
                freed: 0,
                remaining: 5,
            }
        );

        assert_eq!(
            plan_prune(&t.path().join("absent"), 0),
            PrunePlan::default()
        );
    }

    /// A file `prune_dir` cannot delete - locked by a concurrent reader on
    /// Windows, the exact scenario the function's own doc calls out - must
    /// not make the pass stop short of the cap. The achieved total only
    /// drops on a successful removal, so the loop has to keep reaching for
    /// newer files until *that* total clears `limit`, not stop once a
    /// precomputed selection runs out.
    #[cfg(windows)]
    #[test]
    fn prune_keeps_reaching_past_an_undeletable_file_to_still_reach_the_cap() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let t = tempfile::TempDir::new().expect("temp");
        let old = t.path().join("old");
        fs::write(&old, b"yyyy").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        let mid = t.path().join("mid");
        fs::write(&mid, b"zzzz").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        let new = t.path().join("new");
        fs::write(&new, b"xxxxx").expect("write");

        // A share mode of 0 denies every other handle, including a delete -
        // standing in for a file a concurrent build still has open, which is
        // exactly the case `prune_dir`'s own doc calls out.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&old)
            .expect("lock the old file exclusively");

        let pruned = prune_dir(t.path(), 8).expect("prune");
        drop(lock);

        assert!(old.exists(), "the locked file could not be deleted");
        assert!(!mid.exists(), "the next-oldest file was tried and removed");
        assert!(
            !new.exists(),
            "pruning kept reaching for newer files until the cap was actually met, \
             not just until a fixed selection ran out"
        );
        assert!(
            pruned.remaining <= 8,
            "the achieved total must reach the cap: {pruned:?}"
        );
    }

    #[test]
    fn prune_leaves_a_missing_dir_alone() {
        let t = tempfile::TempDir::new().expect("temp");
        let out = prune_dir(&t.path().join("absent"), 1).expect("prune");
        assert_eq!(out, Prune::default());
    }

    #[test]
    fn prune_sweeps_directories_the_files_leave_empty() {
        let t = tempfile::TempDir::new().expect("temp");
        let deep = t.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).expect("dirs");
        fs::write(deep.join("f"), b"1234").expect("write");
        let out = prune_dir(t.path(), 0).expect("prune");
        assert_eq!(out.files, 1);
        assert_eq!(out.remaining, 0);
        assert!(!t.path().join("a").exists(), "empty chain swept");
    }
}

#[cfg(test)]
mod pressure_tests {
    use super::{
        PressureOutcome as O, Prune, dir_size, pressure_limit, pressure_outcome, pressure_reserve,
        prune_dir, prune_dir_keeping,
    };
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    #[test]
    fn a_threshold_equal_to_the_floor_is_the_rule_from_before_the_threshold() {
        assert_eq!(pressure_limit(100, 200, 80, 80, 30), 50);
        assert_eq!(pressure_limit(100, 200, 80, 80, 100), 100);
        assert_eq!(pressure_limit(100, 200, 800, 800, 0), 0);
        assert_eq!(pressure_limit(100, 50, 0, 0, 0), 50);
        // Free at the floor with threshold == floor: no deficit, no reserve.
        assert_eq!(pressure_limit(100, 200, 10, 10, 10), 100);
    }

    #[test]
    fn above_the_threshold_the_limit_is_the_cap_or_the_size() {
        // floor 8, threshold 16, cap 10 (units: GiB)
        assert_eq!(pressure_limit(9, 10, 8, 16, 16), 9);
        assert_eq!(pressure_limit(9, 10, 8, 16, 500), 9);
        assert_eq!(
            pressure_limit(12, 10, 8, 16, 16),
            10,
            "over the cap still shrinks"
        );
    }

    #[test]
    fn between_the_floor_and_the_threshold_a_cache_keeps_half_its_cap() {
        // floor 8, threshold 16, cap 10, so the reserve is 5.
        // A cache of 4 is below its reserve: untouched (limit == size).
        assert_eq!(pressure_limit(4, 10, 8, 16, 10), 4);
        // 9 with free 10: the deficit would leave 3, the reserve keeps 5.
        assert_eq!(pressure_limit(9, 10, 8, 16, 10), 5);
        // 9 with free 15: a deficit of 1 leaves 8, above the reserve.
        assert_eq!(pressure_limit(9, 10, 8, 16, 15), 8);
        // Exactly at the floor is still this band.
        assert_eq!(pressure_limit(9, 10, 8, 16, 8), 5);
        // The cap still bounds from above, and an odd cap rounds the reserve down.
        assert_eq!(pressure_limit(100, 60, 10, 20, 11), 60);
        assert_eq!(pressure_limit(9, 11, 8, 16, 8), 5);
        // The reserve is a fixed ratio of the cap: it does not move with size.
        assert_eq!(
            pressure_limit(40, 10, 8, 16, 8),
            10,
            "cap bounds the shrink"
        );
    }

    #[test]
    fn below_the_floor_the_rule_is_the_one_from_before_the_threshold() {
        // floor 8, threshold 16, cap 10: size 9, free 7 -> one short of the floor.
        assert_eq!(pressure_limit(9, 10, 8, 16, 7), 8);
        assert_eq!(pressure_limit(9, 10, 8, 16, 0), 1);
        // No reserve here: a deficit larger than the cache empties it.
        assert_eq!(pressure_limit(4, 10, 8, 16, 0), 0);
        assert_eq!(pressure_limit(5, 200, 20, 20, 0), 0);
    }

    #[test]
    fn a_zero_cap_stays_zero_in_every_band() {
        for free in [0, 7, 8, 10, 15, 16, 500] {
            assert_eq!(pressure_limit(100, 0, 8, 16, free), 0, "free {free}");
        }
    }

    #[test]
    fn repeated_passes_with_free_space_pinned_converge() {
        // Free space stays at 10 whatever is removed (another program owns the
        // disk). The first pass takes the cache to its reserve, the second
        // removes nothing.
        let mut size = 9;
        let first = pressure_limit(size, 10, 8, 16, 10);
        assert_eq!(first, 5);
        size = size.min(first);
        let second = pressure_limit(size, 10, 8, 16, 10);
        assert_eq!(second, size, "nothing left to remove");
    }

    #[test]
    fn the_reserve_exists_only_between_the_floor_and_the_threshold() {
        // floor 8, threshold 16, cap 10
        assert_eq!(pressure_reserve(10, 8, 16, 7), 0, "below the floor");
        assert_eq!(pressure_reserve(10, 8, 16, 8), 5);
        assert_eq!(pressure_reserve(10, 8, 16, 15), 5);
        assert_eq!(pressure_reserve(10, 8, 16, 16), 0, "at the threshold");
        assert_eq!(pressure_reserve(11, 8, 16, 10), 5, "odd caps round down");
        assert_eq!(
            pressure_reserve(0, 8, 16, 10),
            0,
            "a zero cap keeps nothing"
        );
        // The collection threshold equal to the floor leaves no band at all.
        assert_eq!(pressure_reserve(10, 8, 8, 8), 0);
    }

    #[test]
    fn the_limit_handles_a_small_cap_a_disabled_floor_and_a_cache_at_its_reserve() {
        // Cap 4 (reserve 2) is smaller than the deficit: the cap bounds it.
        assert_eq!(pressure_limit(4, 4, 8, 16, 8), 2);
        // Admission disabled (floor 0) with a positive threshold still collects.
        assert_eq!(pressure_limit(9, 10, 0, 16, 0), 5);
        // Size exactly at the reserve is left alone.
        assert_eq!(pressure_limit(5, 10, 8, 16, 10), 5);
    }

    #[test]
    fn the_outcome_puts_a_maintenance_refusal_first() {
        // Over the cap *and* above the collection threshold (limit = cap), but
        // maintenance could not take the cache: a refusal, not "within limit".
        let limit = pressure_limit(12, 10, 8, 16, 20);
        assert_eq!(limit, 10);
        assert_eq!(pressure_outcome(true, 0, 12, limit, 0), O::LeaseHeld);
        // Same facts with maintenance granted and nothing removable.
        assert_eq!(pressure_outcome(false, 0, 12, limit, 0), O::NothingEligible);
    }

    #[test]
    fn the_outcome_names_why_a_pass_removed_nothing() {
        // (blocked, removed, size, limit, keep)
        assert_eq!(pressure_outcome(false, 4, 9, 5, 5), O::Collected);
        assert_eq!(pressure_outcome(false, 2, 12, 10, 0), O::Collected);
        // Within the limit with no reserve involved (above the threshold).
        assert_eq!(pressure_outcome(false, 0, 9, 9, 0), O::WithinLimit);
        assert_eq!(pressure_outcome(false, 0, 0, 0, 0), O::WithinLimit);
        // At or below the reserve the band keeps.
        assert_eq!(pressure_outcome(false, 0, 4, 4, 5), O::AtReserve);
        assert_eq!(pressure_outcome(false, 0, 5, 5, 5), O::AtReserve);
        // Over the limit and everything is protected by the reserve (one 6 GiB
        // artifact, reserve 5): nothing eligible, not "at reserve".
        assert_eq!(pressure_outcome(false, 0, 6, 5, 5), O::NothingEligible);
        assert_eq!(pressure_outcome(true, 0, 6, 5, 5), O::LeaseHeld);
    }

    /// Write `size` bytes at `dir/name`, `age` seconds old.
    fn put(dir: &Path, name: &str, size: usize, age: u64) {
        let path = dir.join(name);
        fs::write(&path, vec![b'x'; size]).expect("write");
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(SystemTime::now() - Duration::from_secs(age))
            .expect("set mtime");
    }

    /// One pass the way the pressure paths run it: the limit and the reserve
    /// from `free`, the reserve enforced where files are removed.
    fn pass(dir: &Path, cap: u64, free: u64) -> Prune {
        let size = dir_size(dir);
        let limit = pressure_limit(size, cap, 8, 16, free);
        prune_dir_keeping(dir, limit, pressure_reserve(cap, 8, 16, free)).expect("prune")
    }

    #[test]
    fn an_artifact_larger_than_size_minus_reserve_is_kept_and_never_rebuilt_in_a_loop() {
        // cap 10, reserve 5, one 6-byte artifact, free pinned at 10 (floor 8,
        // threshold 16). The limit (5) alone would delete the whole artifact.
        let t = tempfile::TempDir::new().expect("temp");
        put(t.path(), "big", 6, 100);
        assert_eq!(
            prune_dir(t.path(), 5).expect("prune").files,
            1,
            "without the bound the limit empties the cache"
        );
        for _ in 0..3 {
            // The simulated rebuild between passes.
            put(t.path(), "big", 6, 100);
            let out = pass(t.path(), 10, 10);
            assert_eq!(out.files, 0, "{out:?}");
            assert_eq!(out.remaining, 6);
            assert!(t.path().join("big").exists());
        }
    }

    #[test]
    fn the_reserve_skips_a_file_that_would_cross_it_and_removes_only_small_ones() {
        // total 10 (one 6-byte artifact, oldest, and four 1-byte files), cap 10,
        // free 10: limit 5, reserve 5.
        let t = tempfile::TempDir::new().expect("temp");
        put(t.path(), "big", 6, 100);
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            put(t.path(), name, 1, 50 - i as u64);
        }
        let out = pass(t.path(), 10, 10);
        assert_eq!((out.files, out.freed, out.remaining), (4, 4, 6), "{out:?}");
        assert!(t.path().join("big").exists());
        assert!(out.remaining >= 5, "never below the reserve");
        // A second pass with free space still pinned removes nothing.
        assert_eq!(pass(t.path(), 10, 10).files, 0);
    }

    #[test]
    fn the_reserve_stops_a_walk_that_would_cross_it_where_the_plain_limit_would_not() {
        // Three 3-byte files, total 9, limit 5: the plain limit removes two
        // (to 3); the reserve of 5 lets only the oldest go (to 6).
        let t = tempfile::TempDir::new().expect("temp");
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            put(t.path(), name, 3, 90 - i as u64 * 10);
        }
        let out = pass(t.path(), 10, 10);
        assert_eq!((out.files, out.remaining), (1, 6), "{out:?}");
        assert!(!t.path().join("a").exists());
        assert!(t.path().join("b").exists() && t.path().join("c").exists());

        let plain = tempfile::TempDir::new().expect("temp");
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            put(plain.path(), name, 3, 90 - i as u64 * 10);
        }
        assert_eq!(prune_dir(plain.path(), 5).expect("prune").remaining, 3);
    }

    #[test]
    fn below_the_floor_the_bound_is_off_and_the_cache_can_be_emptied() {
        let t = tempfile::TempDir::new().expect("temp");
        put(t.path(), "big", 6, 100);
        // free 0 < floor 8: exactly the rule from before the threshold.
        let out = pass(t.path(), 10, 0);
        assert_eq!((out.files, out.remaining), (1, 0), "{out:?}");
    }
}
