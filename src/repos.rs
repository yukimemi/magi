//! Local repository discovery: `magi repos` and `GET /api/repos`.
//!
//! [`scan`] walks the roots named in `[repos] roots` for a ghq-layout
//! checkout (`<root>/<host>/<owner>/<repo>`).
//!
//! # Read-only, and cheap enough to repeat
//!
//! Nothing here creates, deletes or writes anything, and no root is ever
//! reached over the network - the whole point is that this is a filesystem
//! fact about the operator's own machine. [`Cache`] exists only because a scan
//! still means walking however many roots the operator configured on every
//! request, and the web server should not repeat that walk on every poll. It
//! is trusted for `[repos] scan_ttl` seconds and can always be forced with an
//! explicit refresh.
//!
//! # One implementation, two callers
//!
//! [`scan`] is the whole surface, and both `magi repos` and `GET /api/repos`
//! (see [`crate::web`]) call it rather than each walking the filesystem in
//! its own way. [`Cache`] wraps [`scan`] for the web server, which asks on
//! every request; the CLI, invoked once per command, has no cache to keep.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

/// One repository found under a configured root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repo {
    /// `<owner>/<repo>`, the short name an operator types or picks from a
    /// list.
    pub name: String,
    /// Absolute path to the checkout.
    pub path: PathBuf,
}

/// Scan every root for a ghq-layout checkout: `<root>/<host>/<owner>/<repo>`
/// holding a `.git` directory.
///
/// A root that does not exist or cannot be read contributes nothing rather
/// than failing the whole scan - a stale entry left in `[repos] roots` must
/// not empty the picker for every other root. The same goes for a host or
/// owner directory partway down: [`subdirs`] turns an unreadable directory
/// into no children instead of an error.
///
/// Results are deduplicated by canonical path and sorted by name, so two
/// roots that reach the same checkout - a symlink, or one root nested inside
/// another - list it once.
pub fn scan(roots: &[PathBuf]) -> Vec<Repo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        for host in subdirs(root) {
            for owner in subdirs(&host) {
                for dir in subdirs(&owner) {
                    if !dir.join(".git").exists() {
                        continue;
                    }
                    let path = dir.canonicalize().unwrap_or_else(|_| dir.clone());
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    let name = format!("{}/{}", file_name(&owner), file_name(&dir));
                    out.push(Repo { name, path });
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Immediate subdirectories of `dir`, or none when it cannot be read.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn file_name(path: &Path) -> std::borrow::Cow<'_, str> {
    path.file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
}

/// In-process cache of the last scan.
///
/// `Arc<Mutex<..>>` inside rather than deriving over a bare `Mutex`, so
/// `Cache` itself is cheap to clone - [`crate::web::Ui`] clones its shared
/// state the same way for its loop and turn-guard bookkeeping.
#[derive(Debug, Clone, Default)]
pub struct Cache {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    repos: Vec<Repo>,
    scanned_at: Option<Instant>,
}

impl Cache {
    /// An empty cache. The first [`Cache::list`] always scans.
    pub fn new() -> Self {
        Self::default()
    }

    /// The repositories under `roots`, rescanning when `refresh` is set, the
    /// cache has never been filled, `ttl` has elapsed, or `ttl` is zero -
    /// which means "never trust the cache".
    pub fn list(&self, roots: &[PathBuf], ttl: Duration, refresh: bool) -> Vec<Repo> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let stale =
            refresh || ttl.is_zero() || state.scanned_at.is_none_or(|at| at.elapsed() >= ttl);
        if stale {
            state.repos = scan(roots);
            state.scanned_at = Some(Instant::now());
        }
        state.repos.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds `<root>/<host>/<owner>/<repo>`, with a `.git` directory only
    /// when `git` is true - the one thing that makes a directory count.
    fn make(root: &Path, host: &str, owner: &str, repo: &str, git: bool) -> PathBuf {
        let dir = root.join(host).join(owner).join(repo);
        std::fs::create_dir_all(&dir).expect("create repo dir");
        if git {
            std::fs::create_dir_all(dir.join(".git")).expect("create .git");
        }
        dir
    }

    #[test]
    fn scan_finds_only_git_checkouts_deduplicated_and_sorted_by_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_owned();
        make(&root, "github.com", "yukimemi", "rvpm", true);
        make(&root, "github.com", "yukimemi", "magi", true);
        // No `.git`: a checkout that has not been cloned, or any other
        // directory that happens to sit at the right depth.
        make(&root, "github.com", "yukimemi", "not-a-checkout", false);

        let repos = scan(&[root]);
        let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["yukimemi/magi", "yukimemi/rvpm"]);
        assert!(repos.iter().all(|r| r.path.is_absolute()));
    }

    #[test]
    fn a_missing_root_does_not_empty_the_results_of_the_others() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let good = tmp.path().join("good");
        std::fs::create_dir_all(&good).expect("good root");
        make(&good, "github.com", "yukimemi", "magi", true);
        let missing = tmp.path().join("does-not-exist");

        let repos = scan(&[missing, good]);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "yukimemi/magi");
    }

    #[test]
    fn duplicate_paths_across_roots_are_counted_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_owned();
        make(&root, "github.com", "yukimemi", "magi", true);

        // The same root named twice is the simplest way to exercise the
        // dedup path without touching symlinks, which are not portable to
        // set up in a test.
        let repos = scan(&[root.clone(), root]);
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn the_cache_does_not_rescan_within_the_ttl_but_refresh_forces_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_owned();
        make(&root, "github.com", "yukimemi", "magi", true);
        let roots = [root.clone()];
        let cache = Cache::new();

        let first = cache.list(&roots, Duration::from_secs(3600), false);
        assert_eq!(first.len(), 1);

        // A repository appears after the first scan; within the TTL the
        // cached answer must not notice it.
        make(&root, "github.com", "yukimemi", "rvpm", true);
        let second = cache.list(&roots, Duration::from_secs(3600), false);
        assert_eq!(second.len(), 1, "a fresh cache must not rescan");

        let refreshed = cache.list(&roots, Duration::from_secs(3600), true);
        assert_eq!(refreshed.len(), 2, "an explicit refresh must rescan");

        // The TTL now has to be honoured again against the refreshed scan.
        make(&root, "github.com", "yukimemi", "third", true);
        let still_cached = cache.list(&roots, Duration::from_secs(3600), false);
        assert_eq!(still_cached.len(), 2);
    }

    #[test]
    fn a_zero_ttl_always_rescans() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_owned();
        make(&root, "github.com", "yukimemi", "magi", true);
        let roots = [root.clone()];
        let cache = Cache::new();

        assert_eq!(cache.list(&roots, Duration::from_secs(0), false).len(), 1);
        make(&root, "github.com", "yukimemi", "rvpm", true);
        assert_eq!(cache.list(&roots, Duration::from_secs(0), false).len(), 2);
    }
}
