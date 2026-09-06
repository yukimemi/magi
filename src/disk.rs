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
    // the Windows PowerShell that ships with the OS does not. The constructor
    // takes any rooted path and derives the volume, so an absolute path is
    // passed straight in.
    let abs = std::path::absolute(path)
        .with_context(|| format!("absolute path for {}", path.display()))?;
    let quoted = abs.to_string_lossy().replace('\'', "''");
    let script = format!("[System.IO.DriveInfo]::new('{quoted}').AvailableFreeSpace");
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
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
        files.sort_unstable_by_key(|(mtime, _, _)| *mtime);
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
