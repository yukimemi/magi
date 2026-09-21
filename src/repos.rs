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
//! explicit refresh. [`discover_verified`] is the one exception: it spawns
//! `git` to confirm a candidate [`discover`] found by filesystem shape alone
//! actually works, because a caller substituting it for an unresolved
//! `--repo .` needs more than a plausible path before using it silently -
//! see its own doc.
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

/// How deep [`discover`] walks below each root before giving up on a branch.
/// Deep enough to reach both `<root>/<host>/<owner>/<repo>` (ghq's layout)
/// and `<wt root>/<repo>/<run>/<seat>` (magi's own worktree layout, three
/// levels below `wt` since a run id and a seat are directories in their own
/// right, not part of a checkout's name); shallow enough that an unrelated
/// tree sitting under a well-known root does not turn a fallback lookup into
/// an unbounded walk.
const DISCOVER_MAX_DEPTH: u32 = 4;

/// The directories under the operator's home worth searching when a
/// repository must be found without being named outright - see [`discover`].
/// Not the same list `[repos] roots` scans by default (that one starts
/// empty; nothing is scanned unless configured), because this fallback has
/// to work with no configuration at all.
fn well_known_roots(home: &Path) -> Vec<PathBuf> {
    ["src/github.com", "ghq", "dev", "repos", "projects", "wt"]
        .into_iter()
        .map(|rel| home.join(rel))
        .collect()
}

/// One checkout [`discover`] can offer as an answer.
struct Candidate {
    /// The main checkout's directory - never a linked worktree's own
    /// directory, see [`main_checkout`].
    path: PathBuf,
    /// The main checkout's own directory name, what an operator would type
    /// as a bare repo name.
    name: String,
    /// `<owner>/<name>`, when the checkout's parent directory looks like an
    /// owner (i.e. the checkout sits at least two levels below a root), so a
    /// hint naming `owner/repo` can match precisely instead of only the bare
    /// name.
    owner_name: Option<String>,
}

/// Walk `dir` for git checkouts up to `depth` levels down, resolving each to
/// its main checkout (see [`main_checkout`]) and recording one [`Candidate`]
/// per canonical path not already in `seen`.
///
/// A checkout found is not itself descended into - a repository nested
/// inside another (a submodule, a vendored copy) is one repository as far as
/// naming it goes, not several - and an unreadable directory contributes
/// nothing, the same as [`subdirs`] everywhere else in this module.
fn collect(dir: &Path, depth: u32, seen: &mut HashSet<PathBuf>, out: &mut Vec<Candidate>) {
    if depth == 0 {
        return;
    }
    for child in subdirs(dir) {
        match main_checkout(&child) {
            Some(main) => {
                let path = main.canonicalize().unwrap_or(main);
                if seen.insert(path.clone()) {
                    let name = file_name(&path).into_owned();
                    let owner_name = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|owner| format!("{}/{name}", owner.to_string_lossy()));
                    out.push(Candidate {
                        path,
                        name,
                        owner_name,
                    });
                }
            }
            None => collect(&child, depth - 1, seen, out),
        }
    }
}

/// The main checkout `dir` belongs to, when `dir` holds a `.git` at all - a
/// directory for a normal checkout, or the file git leaves in a linked
/// worktree, naming its main checkout's git dir as
/// `gitdir: <main>/.git/worktrees/<id>`.
///
/// Always the *main* checkout, never the worktree itself: magi's own
/// `<wt root>/<repo>/<run>/<seat>` is a disposable seat, and matching a hint,
/// or an operator's own repository, against a seat id instead of `<repo>`
/// would never succeed. This is also what makes a normal checkout and one of
/// its own linked worktrees collapse to a single [`Candidate`] in [`collect`]
/// rather than competing as if they were two different repositories.
fn main_checkout(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Some(dir.to_path_buf());
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = contents.strip_prefix("gitdir:")?.trim();
    // `<main>/.git/worktrees/<id>` -> `<main>/.git/worktrees` -> `<main>/.git`.
    let git_dir = PathBuf::from(gitdir).parent()?.parent()?.to_path_buf();
    if git_dir.file_name()?.to_str()? != ".git" {
        return None;
    }
    Some(git_dir.parent()?.to_path_buf())
}

/// Whether `hint` mentions `token` as a whole word. `/`, `-` and `_` count as
/// part of a word, so `owner/repo` and `my-repo` match as themselves rather
/// than splitting into several shorter words that could each match too
/// loosely.
fn mentions(hint_lower: &str, token: &str) -> bool {
    let token_lower = token.to_lowercase();
    hint_lower
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '-' | '_')))
        .any(|word| word == token_lower)
}

/// One tier of [`discover`]'s matching ladder: exactly one candidate settles
/// it, none moves on to the next tier, and more than one is refused outright
/// rather than loosened to a lower tier - a tie at the tier that was
/// supposed to decide it is exactly the ambiguity [`discover`] exists to not
/// guess through.
enum Tier {
    Settled(PathBuf),
    Ambiguous,
    Miss,
}

fn tier<'a>(mut it: impl Iterator<Item = &'a Candidate>) -> Tier {
    match (it.next(), it.next()) {
        (None, _) => Tier::Miss,
        (Some(only), None) => Tier::Settled(only.path.clone()),
        (Some(_), Some(_)) => Tier::Ambiguous,
    }
}

/// What [`discover`] found, and in a sentence, why - so a caller that
/// substitutes it for `--repo .` can say so out loud instead of silently
/// swapping in a different repository from the one the operator's own
/// working directory suggested.
pub struct Found {
    /// The main checkout's absolute path.
    pub path: PathBuf,
    /// Which tier of the matching ladder settled it, in words fit to print
    /// straight after "using `<path>` instead - ".
    pub reason: &'static str,
}

/// Find the repository a `--repo .` most likely means when the operator's
/// own working directory is not a git checkout at all: search well-known
/// project directories under `home` (see [`well_known_roots`]), plus
/// `extra_roots` (ordinarily `[repos] roots`), for a checkout matching
/// `hint` - free-form text such as an instruction or a task body - or,
/// failing that, this binary's own checkout (`self_name`; see
/// `crate::updater::REPO`, the one place that name is defined).
///
/// A checkout is used only when it is the single best match at whichever
/// tier of the ladder settles it: an `owner/repo` mention in `hint`, then a
/// bare name mention, then `self_name` alone with nothing else sharing it.
/// `None` either means nothing at all was found, or that a tier which would
/// otherwise have decided it saw more than one candidate - both are left for
/// the caller to fall back to asking the operator, the same as a miss or an
/// ambiguous match through [`crate::main`]'s `resolve_repo_by_name` (see its
/// own doc for why silently guessing between several checkouts is worse than
/// the round trip this exists to save).
pub fn discover(
    home: &Path,
    extra_roots: &[PathBuf],
    hint: Option<&str>,
    self_name: &str,
) -> Option<Found> {
    let mut roots = well_known_roots(home);
    roots.extend(extra_roots.iter().cloned());

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for root in &roots {
        collect(root, DISCOVER_MAX_DEPTH, &mut seen, &mut candidates);
    }

    if let Some(hint) = hint {
        let hint_lower = hint.to_lowercase();
        match tier(candidates.iter().filter(|c| {
            c.owner_name
                .as_deref()
                .is_some_and(|on| mentions(&hint_lower, on))
        })) {
            Tier::Settled(path) => {
                return Some(Found {
                    path,
                    reason: "its owner/repo name is mentioned in the hint",
                });
            }
            Tier::Ambiguous => return None,
            Tier::Miss => {}
        }
        match tier(candidates.iter().filter(|c| mentions(&hint_lower, &c.name))) {
            Tier::Settled(path) => {
                return Some(Found {
                    path,
                    reason: "its name is mentioned in the hint",
                });
            }
            Tier::Ambiguous => return None,
            Tier::Miss => {}
        }
    }

    match tier(candidates.iter().filter(|c| c.name == self_name)) {
        Tier::Settled(path) => Some(Found {
            path,
            reason: "it is this binary's own repository, the only checkout found under the \
                     usual project directories",
        }),
        Tier::Ambiguous | Tier::Miss => None,
    }
}

/// [`discover`], but verified: a checkout is only returned once
/// `crate::git::toplevel` confirms it actually works there. A directory that
/// merely *has* a `.git` - a stale entry, a `git init` interrupted before it
/// wrote anything past the directory itself, a linked worktree whose main
/// checkout was since deleted - is not a confident match, and neither is a
/// checkout found while the local git installation itself is broken: either
/// way the caller must fall back to asking the operator exactly like a miss,
/// not silently accept a path this module only ever inspected as bytes on
/// disk.
///
/// The one function in this module that spawns anything - everything else
/// here is the filesystem walk described in the module doc - because a
/// plausible path is not the same claim as a working repository, and the
/// callers this exists for (a default `--repo .` that already failed its
/// own `git::toplevel` check) exist precisely because that distinction
/// matters.
pub async fn discover_verified(
    home: &Path,
    extra_roots: &[PathBuf],
    hint: Option<&str>,
    self_name: &str,
) -> Option<Found> {
    let found = discover(home, extra_roots, hint, self_name)?;
    crate::git::toplevel(&found.path).await.ok()?;
    Some(found)
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

    #[test]
    fn main_checkout_resolves_a_linked_worktree_to_its_main_checkout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("main-repo");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("seat"))
            .expect("create main .git/worktrees/seat");
        let worktree = tmp.path().join("wt-repo");
        std::fs::create_dir_all(&worktree).expect("create worktree dir");
        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git").join("worktrees").join("seat").display()
            ),
        )
        .expect("write .git file");

        assert_eq!(main_checkout(&worktree), Some(main));
    }

    #[test]
    fn main_checkout_is_none_without_a_git_dir_or_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(main_checkout(tmp.path()), None);
    }

    #[test]
    fn discover_finds_this_binarys_own_repository_under_a_well_known_root_with_no_hint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let repo = home
            .join("src")
            .join("github.com")
            .join("yukimemi")
            .join("magi");
        std::fs::create_dir_all(repo.join(".git")).expect("create repo .git");

        let found = discover(home, &[], None, "magi").expect("self-name match");
        assert_eq!(found.path, repo.canonicalize().expect("canonicalize repo"));
        assert!(
            found.reason.contains("own repository"),
            "got: {}",
            found.reason
        );
    }

    #[test]
    fn discover_finds_nothing_when_no_checkout_matches_self_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        std::fs::create_dir_all(home.join("dev").join("yukimemi").join("other").join(".git"))
            .expect("create unrelated repo");

        assert!(discover(home, &[], None, "magi").is_none());
    }

    #[test]
    fn discover_prefers_an_owner_repo_hint_over_a_bare_name_collision() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let mine = home.join("dev").join("yukimemi").join("widget");
        let theirs = home.join("dev").join("someoneelse").join("widget");
        std::fs::create_dir_all(mine.join(".git")).expect("create mine");
        std::fs::create_dir_all(theirs.join(".git")).expect("create theirs");

        let found = discover(
            home,
            &[],
            Some("please fix a bug in yukimemi/widget"),
            "magi",
        )
        .expect("owner/repo hint resolves the tie");
        assert_eq!(found.path, mine.canonicalize().expect("canonicalize mine"));
        assert!(found.reason.contains("owner/repo"), "got: {}", found.reason);
    }

    #[test]
    fn discover_matches_a_unique_bare_name_mentioned_in_the_hint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let repo = home.join("repos").join("gizmo");
        std::fs::create_dir_all(repo.join(".git")).expect("create repo");

        let found = discover(home, &[], Some("look at gizmo please"), "magi")
            .expect("bare name hint resolves");
        assert_eq!(found.path, repo.canonicalize().expect("canonicalize repo"));
        assert!(
            found.reason.contains("name is mentioned"),
            "got: {}",
            found.reason
        );
    }

    #[test]
    fn discover_refuses_a_bare_name_hint_shared_by_two_checkouts_rather_than_guessing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        std::fs::create_dir_all(
            home.join("dev")
                .join("yukimemi")
                .join("widget")
                .join(".git"),
        )
        .expect("create first widget");
        std::fs::create_dir_all(
            home.join("dev")
                .join("someoneelse")
                .join("widget")
                .join(".git"),
        )
        .expect("create second widget");

        // A tie at the bare-name tier is refused outright, not loosened to
        // the self-name tier even though "magi" matches neither.
        assert!(discover(home, &[], Some("please fix widget"), "magi").is_none());
    }

    #[test]
    fn discover_resolves_a_worktree_under_wt_to_its_main_checkout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        // The main checkout lives outside every well-known root; only the
        // worktree pointer under `wt` leads back to it, the same as magi's
        // own `<wt root>/<repo>/<run>/<seat>` layout for its own checkout.
        let main = tmp.path().join("elsewhere").join("magi");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("cand-A"))
            .expect("create main .git/worktrees/cand-A");

        let seat = home.join("wt").join("magi").join("b21f").join("cand-A");
        std::fs::create_dir_all(&seat).expect("create seat dir");
        std::fs::write(
            seat.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git").join("worktrees").join("cand-A").display()
            ),
        )
        .expect("write worktree .git file");

        let found = discover(home, &[], None, "magi").expect("self-name match via worktree");
        assert_eq!(found.path, main.canonicalize().expect("canonicalize main"));
    }

    #[tokio::test]
    async fn discover_verified_refuses_a_directory_whose_git_dir_is_not_a_real_checkout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        // `discover` alone is satisfied by a directory that merely has a
        // `.git` - a stale entry, or a `git init` that never got past making
        // the directory. `discover_verified` must catch what the bare
        // filesystem shape cannot: `git` itself refuses to treat this as a
        // working tree.
        std::fs::create_dir_all(home.join("repos").join("widget").join(".git"))
            .expect("create a .git directory with nothing real inside it");

        assert!(
            discover_verified(home, &[], None, "widget").await.is_none(),
            "a `.git` directory that is not an actual checkout must not be returned"
        );
    }

    #[tokio::test]
    async fn discover_verified_accepts_a_real_checkout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let repo = home.join("repos").join("widget");
        tokio::fs::create_dir_all(&repo)
            .await
            .expect("create repo dir");
        crate::git::git(&repo, &["init", "-b", "main"])
            .await
            .expect("git init");

        let found = discover_verified(home, &[], None, "widget")
            .await
            .expect("a real checkout resolves");
        assert_eq!(found.path, repo.canonicalize().expect("canonicalize repo"));
    }
}
