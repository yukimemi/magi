//! Disk accounting: how much magi's own directories occupy, how much space is
//! left on the volume they live on, and how the shared build cache is pruned.
//!
//! The whole module grew out of one incident: a machine with 951.8 GB of disk
//! ran a handful of competitions and ended up with 6.7 GB free and a pile of
//! multi-gigabyte `target/` directories. Every function here exists to keep
//! that from being a discovery, and every number is substituted at a pure
//! boundary so the policy can be tested without asking the OS anything.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::proc::Quiet as _;

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
    if enough_space(free, min_free) {
        None
    } else {
        Some(format!(
            "not enough free space to start a run: {free} bytes free, \
             {min_free} required by `[disk] min_free_bytes`"
        ))
    }
}

/// Is `size` past `limit`? One comparison, shared by the janitor and the
/// health view, so both answer "is the cache over its cap" identically.
pub fn over_limit(size: u64, limit: u64) -> bool {
    size > limit
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
/// There is no portable way to ask for this, so each platform runs its own
/// tiny command, deliberately not a new dependency. The parsing halves are
/// pure and asserted against fixture text; only the subprocess is live.
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
    // `fsutil volume diskfree` needs an elevated shell; the .NET DriveInfo in
    // the Windows PowerShell that ships with the OS does not.
    //
    // DriveInfo is handed the **volume root**, never the path itself: its
    // constructor accepts a drive letter or a root directory and throws on
    // anything else, including every verbatim path. The queue stores repo
    // paths as `\\?\C:\...` (that is what `std::path::absolute` yields for a
    // canonicalised root), so passing the path through closed the disk gate
    // for every task with `the disk gate refuses to let a run start blind` -
    // nine tasks were `held` for a disk that had 164 GiB free.
    let abs = std::path::absolute(path)
        .with_context(|| format!("absolute path for {}", path.display()))?;
    let root = volume_root(&abs)
        .with_context(|| format!("no volume root in {} to measure", abs.display()))?;
    let quoted = root.replace('\'', "''");
    let script = format!("[System.IO.DriveInfo]::new('{quoted}').AvailableFreeSpace");
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        // Without this the operator watches a console window blink open for
        // every measurement - and the health view measures on every tick, so
        // merely leaving the deck open in a browser flashed one every few
        // seconds. `Quiet` exists for exactly this and the probe skipped it.
        .quiet()
        .output()
        .with_context(|| format!("run PowerShell for {}", abs.display()))?;
    if !out.status.success() {
        bail!(
            "PowerShell failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_u64(&String::from_utf8_lossy(&out.stdout))
        .with_context(|| format!("parse PowerShell bytes for {}", abs.display()))
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

/// The volume root of an absolute Windows path, as DriveInfo wants it:
/// `C:\`, never `C:\Users\...` and never a verbatim `\\?\C:\...`.
///
/// Pure and platform-independent so the verbatim form - which is what the
/// queue stores and what closed the disk gate on every task - is asserted
/// without a Windows runner. `None` when there is no drive letter to name: a
/// UNC share has no DriveInfo of its own, and a caller must say it cannot
/// measure rather than invent a volume.
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

/// A bare unsigned integer line, which is all PowerShell prints for a long.
pub fn parse_u64(text: &str) -> Option<u64> {
    text.trim().parse().ok()
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

/// [`dir_size`]'s own walk, also tracking the most recent modification time
/// found on any file - both in the same pass, so a caller that wants both
/// (see [`crate::clean::classify_neighbor`]) does not pay for walking a
/// multi-gigabyte build cache twice. Same symlink rule as `dir_size`: a
/// linked directory counts as the link itself and is never descended into.
pub fn dir_size_and_newest_mtime(path: &Path) -> (u64, Option<std::time::SystemTime>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return (0, None);
    };
    if meta.is_file() {
        return (meta.len(), meta.modified().ok());
    }
    if !meta.is_dir() {
        return (0, None);
    }
    let mut total = 0u64;
    let mut newest: Option<std::time::SystemTime> = None;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
                if let Ok(modified) = meta.modified()
                    && newest.is_none_or(|n| modified > n)
                {
                    newest = Some(modified);
                }
            }
        }
    }
    (total, newest)
}

/// Does `dir` look like the root of a Cargo build cache — the shape
/// `CARGO_TARGET_DIR` takes — rather than something else that merely shares a
/// parent directory with the one magi is configured to use?
///
/// Detected by content, never by name. The incident this backs
/// (`Temp\magi-land6`, `Temp\magi-landtimedout`, `Temp\jtargetB`, ...) got
/// those names from whatever a seat or an earlier config improvised for its
/// own `CARGO_TARGET_DIR`; matching a name prefix would miss the very thing
/// this exists to find, and could just as easily match an operator's own
/// unrelated entry under the same parent. Cargo writes `CACHEDIR.TAG` at the
/// target directory's own root — the same marker file the
/// [Cache Directory Tagging Specification](https://bford.info/cachedir/)
/// asks every cache-writing tool to leave — the first time anything ever
/// builds into it, so its presence is the one check.
pub fn looks_like_cargo_target(dir: &Path) -> bool {
    dir.join("CACHEDIR.TAG").is_file()
}

/// Does `dir` hold a sign that a build is still using it right now?
///
/// Cargo takes a lock on `<target-dir>/.cargo-lock` for the lifetime of a
/// build, released the moment the process exits — a live one is the one
/// signal available here that costs nothing to check and cannot itself be
/// stale (unlike an mtime, which a build that has been running for an hour
/// leaves looking old). Its mere *presence* only means a lock was taken at
/// some point, which is not proof of anything by itself, but between it and
/// silence the safer read is the one this function gives: never claim a
/// cache is free of it when the file is sitting right there.
pub fn has_lock_file(dir: &Path) -> bool {
    dir.join(".cargo-lock").is_file()
}

/// A directory found next to the configured build cache that also looks like
/// one, per [`looks_like_cargo_target`].
#[derive(Debug, Clone)]
pub struct NeighborCache {
    /// The directory itself.
    pub path: PathBuf,
    /// Its size, per [`dir_size`].
    pub bytes: u64,
    /// The most recent modification time found anywhere inside it, if any
    /// file could be stat'd at all — see [`dir_size_and_newest_mtime`] for
    /// why this rides along with `bytes` rather than being its own,
    /// separate walk.
    pub newest_mtime: Option<std::time::SystemTime>,
}

/// How many directory levels under `cache_dir`'s own parent
/// [`scan_neighbor_caches`] looks into.
///
/// `1` alone would cover a flat leftover like `Temp\magi-land6`, but the
/// incident this module answers also found per-seat scratch targets one
/// level further down, inside a container that is not itself Cargo-shaped
/// (`Temp\j26c7\A`, `Temp\j26c7\C` — `j26c7` is just a run id, `A` and `C`
/// are the actual targets). `2` catches both without opening the door to a
/// full recursive walk of `Temp`, which is the "delete things by guessing"
/// this module exists to avoid.
const NEIGHBOR_SCAN_DEPTH: u32 = 2;

/// List directories that look like Cargo build caches next to `cache_dir`,
/// other than `cache_dir` itself and anything under `skip`.
///
/// [`NEIGHBOR_SCAN_DEPTH`] levels under `cache_dir`'s own parent, never
/// wider: the leftovers this exists to surface sat near the configured cache
/// (typically rendered from the same `{{ vars.cache }}` template), and
/// walking an entire `Temp` tree — or any directory the operator did not
/// point magi at — is exactly the "delete things under `Temp` by guessing"
/// this module is built to avoid. `skip` is for the directories this scan
/// must never enter even if they happened to look like a match: the run
/// worktree bay and `<home>/runs`, neither of which this function has any
/// business touching.
///
/// A symlink or a junction is never followed and never counted, matching
/// [`dir_size`]'s own rule: a reparse point next to the cache could target
/// anywhere on the machine, and a Cargo-shaped directory reached only by
/// following one is not a directory this scan actually found next to the
/// cache.
pub fn scan_neighbor_caches(cache_dir: &Path, skip: &[PathBuf]) -> Vec<NeighborCache> {
    let Some(parent) = cache_dir.parent() else {
        return Vec::new();
    };
    let cache_canon = canonical_or(cache_dir);
    let skip_canon: Vec<PathBuf> = skip.iter().map(|p| canonical_or(p)).collect();
    let mut out = Vec::new();
    scan_neighbor_level(
        parent,
        &cache_canon,
        &skip_canon,
        NEIGHBOR_SCAN_DEPTH,
        &mut out,
    );
    out
}

/// One level of [`scan_neighbor_caches`]'s walk, recursing into anything
/// that is not itself Cargo-shaped until `depth` runs out. A directory that
/// *is* Cargo-shaped is reported and never descended into — the target
/// itself is what this scan looks for, not whatever cargo has written below
/// it.
fn scan_neighbor_level(
    dir: &Path,
    cache_canon: &Path,
    skip_canon: &[PathBuf],
    depth: u32,
    out: &mut Vec<NeighborCache>,
) {
    let Some(next_depth) = depth.checked_sub(1) else {
        return;
    };
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let canon = canonical_or(&path);
        // Not a bare equality check: `skip` promises "anything under skip",
        // and an operator's `{{ vars.cache }}` can legally nest the
        // configured cache (and so this scan's own `parent`) inside the
        // worktree bay or `<home>/runs` rather than only ever sitting beside
        // them. `starts_with` catches both directions — a found entry
        // sitting inside a skip root, and (should `dir` itself already be a
        // descendant of one) a skip root sitting inside a found entry, which
        // would otherwise still be walked into by `dir_size` and, worse,
        // deleted whole by a caller that reclaims it.
        if canon == *cache_canon
            || skip_canon
                .iter()
                .any(|s| canon.starts_with(s) || s.starts_with(&canon))
        {
            continue;
        }
        if looks_like_cargo_target(&path) {
            let (bytes, newest_mtime) = dir_size_and_newest_mtime(&path);
            out.push(NeighborCache {
                bytes,
                newest_mtime,
                path,
            });
        } else {
            scan_neighbor_level(&path, cache_canon, skip_canon, next_depth, out);
        }
    }
}

/// `path`, canonicalized when possible, falling back to the path as given —
/// so a comparison against it degrades to a plain (still useful, if less
/// robust against `..` or symlinks) path comparison rather than failing
/// outright when the path does not exist or is not readable.
///
/// `pub(crate)`: [`crate::clean`]'s neighbor-cache classifier compares a scan
/// result against paths read back out of run records the same way, and the
/// two must agree on what "the same directory" means.
pub(crate) fn canonical_or(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    for (_, size, path) in tree.files {
        if !over_limit(total, limit) {
            break;
        }
        // A file that is being read elsewhere (a concurrent build, a snapshot)
        // fails on Windows; skip it and continue — the next prune gets it.
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            freed += size;
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

/// Files and directories under one root, walked up-front.
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
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let path = entry.path();
                if meta.is_dir() {
                    dirs.push((depth + 1, path.clone()));
                    stack.push((depth + 1, path));
                } else if meta.is_file() {
                    let size = meta.len();
                    total += size;
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

    #[test]
    fn a_powershell_number_is_one_unsigned_integer() {
        assert_eq!(parse_u64("     82072211456\r\n"), Some(82_072_211_456));
        assert_eq!(parse_u64("nah"), None);
    }

    /// DriveInfo takes a volume, and the queue hands out verbatim paths.
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
        // No drive to name: a share has no DriveInfo, and a POSIX path has no
        // volume at all. The caller has to report that it cannot measure.
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
    fn a_cargo_target_is_recognized_by_its_tag_file_not_its_name() {
        let t = tempfile::TempDir::new().expect("temp");
        assert!(
            !looks_like_cargo_target(t.path()),
            "an empty directory is not a build cache just because it exists"
        );
        fs::write(t.path().join("CACHEDIR.TAG"), b"Signature: 8a477f...").expect("write");
        assert!(looks_like_cargo_target(t.path()));
        assert!(
            !looks_like_cargo_target(&t.path().join("nope")),
            "a missing directory is never mistaken for a cache"
        );
    }

    #[test]
    fn a_cargo_lock_file_is_the_only_thing_that_marks_use() {
        let t = tempfile::TempDir::new().expect("temp");
        assert!(!has_lock_file(t.path()));
        fs::write(t.path().join(".cargo-lock"), b"").expect("write");
        assert!(has_lock_file(t.path()));
    }

    #[test]
    fn neighbor_scan_finds_only_cargo_shaped_siblings_outside_the_skip_list() {
        let t = tempfile::TempDir::new().expect("temp");
        let cache = t.path().join("magi-target");
        fs::create_dir_all(&cache).expect("dir");
        fs::write(cache.join("CACHEDIR.TAG"), b"tag").expect("write");

        // A genuine leftover: cargo-shaped, sitting right beside the
        // configured cache.
        let orphan = t.path().join("magi-land6");
        fs::create_dir_all(&orphan).expect("dir");
        fs::write(orphan.join("CACHEDIR.TAG"), b"tag").expect("write");
        fs::write(orphan.join("junk"), vec![0u8; 5]).expect("write");

        // Same parent, but no `CACHEDIR.TAG`: an operator's own unrelated
        // directory, never a match by name alone.
        let unrelated = t.path().join("Downloads");
        fs::create_dir_all(&unrelated).expect("dir");

        // Explicitly excluded even though it is cargo-shaped: the worktree
        // bay or `<home>/runs` must never be swept by this scan.
        let excluded = t.path().join("wt");
        fs::create_dir_all(&excluded).expect("dir");
        fs::write(excluded.join("CACHEDIR.TAG"), b"tag").expect("write");

        let found = scan_neighbor_caches(&cache, std::slice::from_ref(&excluded));
        let paths: Vec<&Path> = found.iter().map(|n| n.path.as_path()).collect();
        assert_eq!(found.len(), 1, "found: {paths:?}");
        assert_eq!(found[0].path, orphan);
        assert_eq!(found[0].bytes, 8, "the tag file itself counts too");
    }

    /// The exact shape the incident behind this module found and a
    /// one-level scan would miss entirely: `Temp\j26c7` is just a run id, not
    /// a build cache in its own right, but `Temp\j26c7\A` and `\C` are — a
    /// seat's own scratch target one level further down than
    /// `magi-land6`-style flat leftovers sit.
    #[test]
    fn neighbor_scan_finds_a_seat_target_nested_one_level_inside_a_run_id_container() {
        let t = tempfile::TempDir::new().expect("temp");
        let cache = t.path().join("magi-target");
        fs::create_dir_all(&cache).expect("dir");
        fs::write(cache.join("CACHEDIR.TAG"), b"tag").expect("write");

        let container = t.path().join("j26c7");
        fs::create_dir_all(&container).expect("dir");
        let seat_a = container.join("A");
        fs::create_dir_all(&seat_a).expect("dir");
        fs::write(seat_a.join("CACHEDIR.TAG"), b"tag").expect("write");
        let seat_c = container.join("C");
        fs::create_dir_all(&seat_c).expect("dir");
        fs::write(seat_c.join("CACHEDIR.TAG"), b"tag").expect("write");

        let found = scan_neighbor_caches(&cache, &[]);
        let mut paths: Vec<&Path> = found.iter().map(|n| n.path.as_path()).collect();
        paths.sort();
        assert_eq!(paths, vec![seat_a.as_path(), seat_c.as_path()], "{paths:?}");
        // The container itself is not cargo-shaped, so it is never reported
        // as an entry of its own - only what is actually inside it.
        assert!(!found.iter().any(|n| n.path == container));
    }

    /// `skip` promises "anything under skip", not merely "exactly skip" — an
    /// operator's `{{ vars.cache }}` can render `CARGO_TARGET_DIR` to a path
    /// nested inside `<home>/runs` or the worktree bay rather than only ever
    /// beside them. Both directions of containment have to be caught: a
    /// found entry sitting inside a skip root, and a skip root sitting inside
    /// a found entry.
    #[test]
    fn neighbor_scan_excludes_anything_under_skip_not_only_an_exact_match() {
        let t = tempfile::TempDir::new().expect("temp");

        // Direction one: the scan's own parent is already inside a skip
        // root, so every entry it lists is too.
        let runs = t.path().join("runs");
        let parent = runs.join("subdir");
        fs::create_dir_all(&parent).expect("dir");
        let cache = parent.join("magi-target");
        fs::create_dir_all(&cache).expect("dir");
        let leftover = parent.join("leftover");
        fs::create_dir_all(&leftover).expect("dir");
        fs::write(leftover.join("CACHEDIR.TAG"), b"tag").expect("write");

        let found = scan_neighbor_caches(&cache, std::slice::from_ref(&runs));
        assert!(
            found.is_empty(),
            "an entry inside a skip root must never be reported: {found:?}"
        );

        // Direction two: a skip root sits inside one of the found entries -
        // reclaiming that whole entry would take the skip root down with it.
        let cache2 = t.path().join("magi-target2");
        fs::create_dir_all(&cache2).expect("dir");
        let big = t.path().join("big-cache");
        fs::create_dir_all(&big).expect("dir");
        fs::write(big.join("CACHEDIR.TAG"), b"tag").expect("write");
        let nested_runs = big.join("inner-runs");
        fs::create_dir_all(&nested_runs).expect("dir");

        let found2 = scan_neighbor_caches(&cache2, std::slice::from_ref(&nested_runs));
        assert!(
            found2.is_empty(),
            "an entry that would carry a skip root down with it must never be \
             reported: {found2:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn neighbor_scan_never_follows_a_symlink_into_a_cargo_shaped_target() {
        let t = tempfile::TempDir::new().expect("temp");
        let cache = t.path().join("magi-target");
        fs::create_dir_all(&cache).expect("dir");

        let real = t.path().join("elsewhere");
        fs::create_dir_all(&real).expect("dir");
        fs::write(real.join("CACHEDIR.TAG"), b"tag").expect("write");
        std::os::unix::fs::symlink(&real, t.path().join("link")).expect("symlink");

        let found = scan_neighbor_caches(&cache, &[]);
        assert!(
            found.is_empty(),
            "a symlink next to the cache is never treated as a cache of its own"
        );
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
