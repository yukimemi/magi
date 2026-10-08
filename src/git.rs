//! Git plumbing.
//!
//! magi drives the `git` CLI rather than linking a library: every operation it
//! needs is a one-liner, and shelling out keeps the behaviour identical to what
//! the operator sees when they inspect a run by hand.
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::proc::Quiet as _;
use anyhow::{Context as _, Result, bail};
use tokio::process::Command;

/// Output of a completed `git` invocation.
#[derive(Debug)]
pub struct GitOut {
    /// Exit status code, if the process was not killed by a signal.
    pub code: Option<i32>,
    /// Captured stdout, trailing newline trimmed.
    pub stdout: String,
    /// Captured stderr, trailing newline trimmed.
    pub stderr: String,
}

impl GitOut {
    /// Did the command succeed?
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `git` in `cwd` with `args`, returning the captured output regardless of
/// exit status.
pub async fn git_raw(cwd: &Path, args: &[&str]) -> Result<GitOut> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .quiet()
        // A hook that opens an editor or a credential prompt would hang a
        // headless run forever.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    Ok(GitOut {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
    })
}

/// Fetch `origin`'s branches into `refs/remotes/origin/*` in `cwd`, bounded by
/// `timeout`.
///
/// The destination is fixed on the command line and the remote is addressed by
/// its URL rather than its name. A plain `git fetch origin` follows the
/// configured `remote.origin.fetch`, which can map onto local branches
/// (`+refs/heads/main:refs/heads/main`) or a single branch, and even a fetch
/// with an explicit refspec updates remote-tracking refs from that config
/// when the remote is named. By URL, nothing but the refspec given here is
/// written: no local branch, HEAD, index or working tree. A child still
/// running at the deadline is killed on drop.
pub async fn fetch_origin(cwd: &Path, timeout: std::time::Duration) -> Result<()> {
    let url = git(cwd, &["remote", "get-url", "origin"]).await?;
    let fut = Command::new("git")
        .args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "--no-write-fetch-head",
            "--",
            url.as_str(),
            "+refs/heads/*:refs/remotes/origin/*",
        ])
        .current_dir(cwd)
        .quiet()
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow::anyhow!("git fetch timed out after {}s", timeout.as_secs()))?
        .context("spawn git fetch")?;
    if !out.status.success() {
        bail!(
            "git fetch failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim_end()
        );
    }
    Ok(())
}

/// Run `git`, failing on a non-zero exit status.
pub async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = git_raw(cwd, args).await?;
    if !out.ok() {
        bail!(
            "git {} failed in {} (exit {:?}): {}",
            args.join(" "),
            cwd.display(),
            out.code,
            if out.stderr.is_empty() {
                out.stdout.as_str()
            } else {
                out.stderr.as_str()
            }
        );
    }
    Ok(out.stdout)
}

/// Absolute path to the top level of the working tree containing `path`.
pub async fn toplevel(path: &Path) -> Result<PathBuf> {
    let out = git(path, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(out))
}

/// Resolve a revision to a full object id.
pub async fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    git(repo, &["rev-parse", rev]).await
}

/// Currently checked-out branch, or `None` when detached.
pub async fn current_branch(repo: &Path) -> Result<Option<String>> {
    let out = git_raw(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    Ok(if out.ok() && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    })
}

/// The branch a `refs/remotes/<remote>/HEAD` symref names, e.g.
/// `refs/remotes/origin/main` -> `main`. Pure, and shared with
/// `Config::discover` so the branch magi reads its config from and the branch
/// a run starts from can never come from two different readings.
pub fn remote_head_branch(remote: &str, symref: &str) -> Option<String> {
    symref
        .trim()
        .strip_prefix(&format!("refs/remotes/{remote}/"))
        .filter(|b| !b.is_empty() && *b != "HEAD")
        .map(str::to_owned)
}

/// Record `refs/remotes/<remote>/HEAD` -> `<remote>/<base>` when it is absent,
/// so a later read of the remote's default (`Config::discover`) agrees with
/// the branch just resolved even when git did not create the symref on fetch
/// (`followRemoteHEAD=never`, old git). Moves only `refs/remotes`; an existing
/// symref is left alone. Best effort.
pub async fn ensure_remote_head(repo: &Path, remote: &str, base: &str) {
    let head = format!("refs/remotes/{remote}/HEAD");
    if git_raw(repo, &["symbolic-ref", "--quiet", &head])
        .await
        .is_ok_and(|o| o.ok())
    {
        return;
    }
    let target = format!("refs/remotes/{remote}/{base}");
    let _ = git_raw(repo, &["symbolic-ref", &head, &target]).await;
}

/// The branch in `git ls-remote --symref <remote> HEAD` output
/// (`ref: refs/heads/main\tHEAD`).
fn symref_branch(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/"))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_owned)
}

/// Which branch of `remote` is the base: `explicit` (`[merge] base`) wins,
/// else the remote's default branch as recorded in `refs/remotes/<remote>/HEAD`.
/// When that ref is missing (a checkout that was never cloned from it), ask the
/// remote once with `git remote set-head <remote> -a`, which moves only
/// `refs/remotes`. Never the checked-out branch: a detached or stale primary
/// checkout must not decide what a run branches off.
pub async fn merge_base_branch(
    repo: &Path,
    remote: &str,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(b) = explicit.map(str::trim).filter(|b| !b.is_empty()) {
        return Ok(b.to_owned());
    }
    let head = format!("refs/remotes/{remote}/HEAD");
    let read = || async {
        let out = git_raw(repo, &["symbolic-ref", "--quiet", &head])
            .await
            .ok()?;
        if out.ok() {
            remote_head_branch(remote, &out.stdout)
        } else {
            None
        }
    };
    if let Some(b) = read().await {
        return Ok(b);
    }
    let set = git_raw(repo, &["remote", "set-head", remote, "-a"]).await;
    if let Some(b) = read().await {
        return Ok(b);
    }
    // `set-head -a` insists the tracking ref already exists, which it does not
    // for a remote that was never fetched (or a single-branch clone of another
    // branch). Ask the remote for its HEAD symref instead: that only names the
    // branch, and the fetch that follows (`resolve_base`) creates the ref.
    if let Ok(o) = git_raw(repo, &["ls-remote", "--symref", remote, "HEAD"]).await
        && o.ok()
        && let Some(b) = symref_branch(&o.stdout)
    {
        return Ok(b);
    }
    let why = match set {
        Ok(o) if !o.ok() => o.stderr.lines().next().unwrap_or("").trim().to_owned(),
        Ok(_) => "the remote reported no default branch".to_owned(),
        Err(e) => e.to_string(),
    };
    bail!(
        "cannot tell which branch of `{remote}` is the base ({why}): set [merge] base \
         (and remote) in magi.toml, or run `git remote set-head {remote} -a`. magi does \
         not fall back to the checked-out branch, which may be detached or stale"
    )
}

/// Is the working tree free of tracked modifications and untracked files?
pub async fn is_clean(repo: &Path) -> Result<bool> {
    Ok(git(repo, &["status", "--porcelain"]).await?.is_empty())
}

/// `git status --porcelain`, for reporting what is dirty.
pub async fn status_porcelain(repo: &Path) -> Result<String> {
    git(repo, &["status", "--porcelain"]).await
}

/// Create a worktree at `path` with a fresh branch `branch` starting at `base`.
pub async fn worktree_add_branch(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let path_s = path.to_string_lossy().to_string();
    git(repo, &["worktree", "add", "-b", branch, &path_s, base])
        .await
        .map(|_| ())
}

/// Create a worktree at `path` with a detached HEAD at `rev`.
pub async fn worktree_add_detached(repo: &Path, path: &Path, rev: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let path_s = path.to_string_lossy().to_string();
    git(repo, &["worktree", "add", "--detach", &path_s, rev])
        .await
        .map(|_| ())
}

/// Move an existing detached worktree to `rev`, discarding local state.
pub async fn reset_detached(worktree: &Path, rev: &str) -> Result<()> {
    git(worktree, &["checkout", "--detach", rev]).await?;
    git(worktree, &["reset", "--hard", rev]).await?;
    git(worktree, &["clean", "-fdx"]).await?;
    Ok(())
}

/// Where `branch` is checked out, if some worktree holds it.
///
/// Read from `git worktree list --porcelain` rather than out of an error
/// message, which varies with git's version and locale.
pub async fn worktree_holding(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let listing = git(repo, &["worktree", "list", "--porcelain"]).await?;
    Ok(parse_worktree_holder(&listing, branch))
}

fn parse_worktree_holder(listing: &str, branch: &str) -> Option<PathBuf> {
    let want = format!("refs/heads/{branch}");
    let mut path: Option<PathBuf> = None;
    for line in listing.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if line.strip_prefix("branch ") == Some(want.as_str()) {
            return path;
        }
    }
    None
}

/// Remove a worktree. Returns `Ok(false)` when git refused (e.g. the path is
/// already gone), so callers can keep folding the rest of a run.
pub async fn worktree_remove(repo: &Path, path: &Path) -> Result<bool> {
    let path_s = path.to_string_lossy().to_string();
    let out = git_raw(repo, &["worktree", "remove", "--force", &path_s]).await?;
    if out.ok() {
        return Ok(true);
    }
    // A worktree whose directory was deleted by hand only needs pruning.
    git_raw(repo, &["worktree", "prune"]).await?;
    Ok(false)
}

/// Remove a worktree only if git finds it clean: no `--force`, so a change
/// made after the caller last looked is refused rather than thrown away.
/// Ignored files (`target/`) do not count as changes and go with it.
pub async fn worktree_remove_clean(repo: &Path, path: &Path) -> Result<bool> {
    let path_s = path.to_string_lossy().to_string();
    Ok(git_raw(repo, &["worktree", "remove", &path_s]).await?.ok())
}

/// Unregister a linked worktree whose directory is about to be deleted by
/// hand, so the path can be `worktree add`-ed again.
///
/// A linked worktree's `.git` is a file whose `gitdir:` line names the
/// bookkeeping entry inside its repository's admin directory; pruning from
/// there removes the registration without touching the directory. No-op when
/// `dir` is not a registered worktree (`.git` missing or not a `gitdir:`
/// link): nothing was registered, nothing survives removal.
pub async fn remove_worktree_from_linked(dir: &Path) {
    let Ok(link) = std::fs::read_to_string(dir.join(".git")) else {
        return;
    };
    let Some(admin) = link.strip_prefix("gitdir:").map(str::trim) else {
        return;
    };
    // `<repo>/.git/worktrees/<name>`, so the repository's git dir is two
    // levels up from here.
    let admin = Path::new(admin);
    let Some(common) = admin.parent().and_then(Path::parent) else {
        return;
    };
    let common_s = common.to_string_lossy();
    let _ = git_raw(dir, &["--git-dir", &common_s, "worktree", "prune"]).await;
}

/// Drop registrations for worktrees whose directory is already gone.
///
/// `git worktree remove` already does this for the path it just removed, but
/// a directory deleted by hand - [`crate::clean::fold_orphaned_worktrees`], or
/// an operator's own `rm -rf` - leaves the registration behind, and a
/// registered path refuses a fresh `worktree add` until something prunes it.
/// The operator's own machine had 31 such registrations sitting in one
/// repository, all of them for directories that no longer existed.
pub async fn worktree_prune(repo: &Path) -> Result<()> {
    git(repo, &["worktree", "prune"]).await.map(|_| ())
}

/// Delete a branch, ignoring "not found".
pub async fn branch_delete(repo: &Path, branch: &str) -> Result<bool> {
    Ok(git_raw(repo, &["branch", "-D", branch]).await?.ok())
}

/// Does `branch` exist?
pub async fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    let refname = format!("refs/heads/{branch}");
    Ok(
        git_raw(repo, &["show-ref", "--verify", "--quiet", &refname])
            .await?
            .ok(),
    )
}

/// Does `remote` have `branch`? `Err` when that cannot be told (unreachable
/// remote), which callers must not read as "no".
pub async fn remote_has_branch(repo: &Path, remote: &str, branch: &str) -> Result<bool> {
    let refname = format!("refs/heads/{branch}");
    let out = git_raw(repo, &["ls-remote", "--exit-code", remote, &refname]).await?;
    match out.code {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        code => bail!(
            "git ls-remote {remote} failed (exit {code:?}): {}",
            out.stderr
        ),
    }
}

/// Patch of `head` against the merge base with `base`.
pub async fn diff(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let range = format!("{base}...{head}");
    git(
        worktree,
        &["diff", "--no-color", "--no-ext-diff", "-M", &range],
    )
    .await
}

/// `--stat` summary of `base...head`.
pub async fn diff_stat(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let range = format!("{base}...{head}");
    git(worktree, &["diff", "--no-color", "--stat", &range]).await
}

/// Number of files touched by `base...head`.
pub async fn changed_files(worktree: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let range = format!("{base}...{head}");
    let out = git(worktree, &["diff", "--name-only", &range]).await?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// Subject and body of every commit in `base..head`, oldest first. Fields are
/// split on control characters git never prints in a message, so an empty
/// subject or a body full of separators cannot shift a record.
pub async fn commit_log(worktree: &Path, base: &str, head: &str) -> Result<Vec<(String, String)>> {
    let range = format!("{base}..{head}");
    let out = git(
        worktree,
        &[
            "log",
            "--reverse",
            "--no-color",
            "--format=%s%x1f%b%x00",
            &range,
        ],
    )
    .await?;
    Ok(out
        .split('\0')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            if rec.trim().is_empty() {
                return None;
            }
            let (subject, body) = rec.split_once('\x1f').unwrap_or((rec, ""));
            Some((subject.trim().to_owned(), body.trim().to_owned()))
        })
        .collect())
}

/// One-line log of `base..head`, oldest first.
pub async fn log_oneline(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let range = format!("{base}..{head}");
    git(
        worktree,
        &["log", "--reverse", "--format=%s%n%b%n--", &range],
    )
    .await
}

/// Subjects of the commits in `base..head`, oldest first. NUL-terminated so an
/// empty subject keeps its place instead of shifting the later ones forward.
pub async fn subjects(worktree: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let range = format!("{base}..{head}");
    let out = git(worktree, &["log", "--reverse", "--format=%s%x00", &range]).await?;
    let mut parts: Vec<String> = out.split('\0').map(|s| s.trim().to_owned()).collect();
    // Whatever follows the last terminator is just the trailing newline.
    parts.pop();
    Ok(parts)
}

/// How many commits `head` is ahead of `base`.
pub async fn commits_ahead(worktree: &Path, base: &str, head: &str) -> Result<usize> {
    let range = format!("{base}..{head}");
    let out = git(worktree, &["rev-list", "--count", &range]).await?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// Stage everything and commit under a neutral identity.
///
/// Used to rescue an agent that edited files but never committed: without this
/// its candidate would silently be empty. The neutral identity is part of the
/// blindness contract — a real `user.name` in a candidate's history would name
/// the operator, and an agent-configured one would name the vendor.
pub async fn commit_all(worktree: &Path, message: &str) -> Result<bool> {
    if git(worktree, &["status", "--porcelain"]).await?.is_empty() {
        return Ok(false);
    }
    git(worktree, &["add", "-A"]).await?;
    // What `[worktree] setup` produced is not the agent's work.
    if !crate::worktree_setup::withheld_paths(worktree)
        .await
        .is_empty()
    {
        crate::worktree_setup::unstage_products(worktree).await?;
        if git_raw(worktree, &["diff", "--cached", "--quiet"])
            .await?
            .ok()
        {
            return Ok(false);
        }
    }
    let out = git_raw(
        worktree,
        &[
            "-c",
            "user.name=magi candidate",
            "-c",
            "user.email=magi@localhost",
            "commit",
            "--no-verify",
            "-m",
            message,
        ],
    )
    .await?;
    if !out.ok() {
        bail!("rescue commit failed: {}", out.stderr);
    }
    Ok(true)
}

/// A freshly created lockfile that belongs to a package manager the directory
/// does not use, and was therefore left out of a rescue commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    /// Repo-relative path, forward slashes.
    pub path: String,
    /// The package manager the file belongs to (`pnpm`, `cargo`, ...).
    pub manager: String,
    /// What made it foreign: the tracked lockfile (or `Cargo.toml`'s absence)
    /// that says which manager the directory really uses.
    pub kept_by: String,
}

/// What [`rescue_commit`] did.
#[derive(Debug, Default)]
pub struct Rescue {
    /// Whether a commit was made.
    pub committed: bool,
    /// Files left untracked in the worktree instead of being committed.
    pub withheld: Vec<Stray>,
}

/// `(ecosystem, manager)` for a lockfile's file name.
fn lock_kind(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "package-lock.json" | "npm-shrinkwrap.json" => ("node", "npm"),
        "yarn.lock" => ("node", "yarn"),
        "pnpm-lock.yaml" => ("node", "pnpm"),
        "bun.lock" | "bun.lockb" => ("node", "bun"),
        "poetry.lock" => ("python", "poetry"),
        "uv.lock" => ("python", "uv"),
        "Pipfile.lock" => ("python", "pipenv"),
        "pdm.lock" => ("python", "pdm"),
        "Cargo.lock" => ("rust", "cargo"),
        _ => return None,
    })
}

fn split_dir(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Which of the newly created `untracked` files are lockfiles of a manager the
/// repo does not use in that directory.
///
/// Foreign means: a lockfile of the same ecosystem but another manager is
/// already tracked *in the same directory* (no recursion — a workspace root and
/// a sub-package may legitimately differ), or, for `Cargo.lock`, there is no
/// `Cargo.toml` beside it. A first lockfile in a directory with none is normal.
pub fn stray_lockfiles(untracked: &[String], tracked: &[String]) -> Vec<Stray> {
    let mut out = Vec::new();
    for path in untracked {
        let (dir, name) = split_dir(path);
        let Some((eco, manager)) = lock_kind(name) else {
            continue;
        };
        let beside = |other: &String| split_dir(other).0 == dir;
        let kept_by = if manager == "cargo" {
            let has_manifest = tracked
                .iter()
                .chain(untracked)
                .any(|p| beside(p) && split_dir(p).1 == "Cargo.toml");
            if has_manifest {
                continue;
            }
            "no Cargo.toml in the directory".to_owned()
        } else {
            let Some(other) = tracked.iter().find(|p| {
                beside(p)
                    && lock_kind(split_dir(p).1).is_some_and(|(e, m)| e == eco && m != manager)
            }) else {
                continue;
            };
            other.clone()
        };
        out.push(Stray {
            path: path.clone(),
            manager: manager.to_owned(),
            kept_by,
        });
    }
    out
}

async fn nul_list(worktree: &Path, args: &[&str]) -> Result<Vec<String>> {
    let out = git(worktree, args).await?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// [`commit_all`] for an agent's leftover work, minus stray foreign lockfiles.
///
/// The withheld files stay untracked in the worktree (nothing is deleted) and
/// are returned so the caller can record them: silently dropping them could
/// lose a file the task really asked for.
pub async fn rescue_commit(worktree: &Path, message: &str) -> Result<Rescue> {
    if git(worktree, &["status", "--porcelain"]).await?.is_empty() {
        return Ok(Rescue::default());
    }
    let untracked = nul_list(
        worktree,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )
    .await?;
    let tracked = nul_list(worktree, &["ls-files", "-z"]).await?;
    let withheld = stray_lockfiles(&untracked, &tracked);

    git(worktree, &["add", "-A"]).await?;
    if !withheld.is_empty() {
        let mut args = vec!["reset", "-q", "--"];
        args.extend(withheld.iter().map(|s| s.path.as_str()));
        git(worktree, &args).await?;
    }
    crate::worktree_setup::unstage_products(worktree).await?;
    if git_raw(worktree, &["diff", "--cached", "--quiet"])
        .await?
        .ok()
    {
        return Ok(Rescue {
            committed: false,
            withheld,
        });
    }
    let out = git_raw(
        worktree,
        &[
            "-c",
            "user.name=magi candidate",
            "-c",
            "user.email=magi@localhost",
            "commit",
            "--no-verify",
            "-m",
            message,
        ],
    )
    .await?;
    if !out.ok() {
        bail!("rescue commit failed: {}", out.stderr);
    }
    Ok(Rescue {
        committed: true,
        withheld,
    })
}

/// Enable `extensions.worktreeConfig` if it is not already on.
///
/// Returns `true` when magi turned it on, so the caller can turn it back off
/// during cleanup and leave the repo exactly as it found it.
pub async fn enable_worktree_config(repo: &Path) -> Result<bool> {
    let out = git_raw(repo, &["config", "--get", "extensions.worktreeConfig"]).await?;
    if out.ok() && out.stdout.trim() == "true" {
        return Ok(false);
    }
    git(repo, &["config", "extensions.worktreeConfig", "true"]).await?;
    Ok(true)
}

/// Undo [`enable_worktree_config`].
pub async fn disable_worktree_config(repo: &Path) -> Result<()> {
    git_raw(repo, &["config", "--unset", "extensions.worktreeConfig"]).await?;
    Ok(())
}

/// How many runs currently want `extensions.worktreeConfig` on for one
/// repository, and whether magi is the one that turned it on.
struct WorktreeConfigRef {
    /// Runs holding a reference, via [`acquire_worktree_config`].
    count: usize,
    /// Did *this process* flip the setting from off to on? If not - it was
    /// already `true` when the first run in this process asked - nothing
    /// here ever turns it off either; that is what [`enable_worktree_config`]
    /// already decided for the single-run case, and the ref-counted version
    /// must not second-guess it.
    we_enabled: bool,
}

/// One entry per repository, each guarded by its own `tokio::sync::Mutex` so
/// that two repositories' acquisitions never wait on each other - only two
/// runs in the *same* repository do, which is the point.
///
/// A `std::sync::Mutex` guards the map itself, held only long enough to find
/// or insert an entry and clone its `Arc`, never across an `.await`.
static WORKTREE_CONFIG: std::sync::LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<PathBuf, std::sync::Arc<tokio::sync::Mutex<WorktreeConfigRef>>>,
    >,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// The per-repository slot, creating it if this is the first run to ask.
fn worktree_config_slot(repo: &Path) -> std::sync::Arc<tokio::sync::Mutex<WorktreeConfigRef>> {
    let mut map = WORKTREE_CONFIG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(repo.to_path_buf())
        .or_insert_with(|| {
            std::sync::Arc::new(tokio::sync::Mutex::new(WorktreeConfigRef {
                count: 0,
                we_enabled: false,
            }))
        })
        .clone()
}

/// Take a reference on `extensions.worktreeConfig` being on for `repo`.
///
/// [`enable_worktree_config`] alone is only safe for one run in a repository
/// at a time: it is a plain get-then-set, so a second run's "already true?"
/// check can see the first run's write and conclude it owns nothing to turn
/// back off, while the first run's own cleanup turns the setting off under
/// the second run's feet the moment *it* finishes - the exact race that let a
/// finished run's fold disable the hook a still-running sibling in the same
/// repository depended on. This ref-counts instead: the setting is turned on
/// once, by whichever caller is first, and turned off only once every caller
/// has released it via [`release_worktree_config`].
///
/// The per-repository lock is held across the `git config` call for the
/// first acquire, so a second, concurrent acquire for the same repository
/// waits for it rather than racing it - without that, both could observe
/// "not yet counted" and both try to flip the setting on.
pub async fn acquire_worktree_config(repo: &Path) -> Result<()> {
    let slot = worktree_config_slot(repo);
    let mut entry = slot.lock().await;
    entry.count += 1;
    if entry.count == 1 {
        entry.we_enabled = enable_worktree_config(repo).await?;
    }
    Ok(())
}

/// Release a reference taken by [`acquire_worktree_config`].
///
/// Only the last release for a repository actually calls
/// [`disable_worktree_config`], and only when this process was the one that
/// turned the setting on in the first place.
pub async fn release_worktree_config(repo: &Path) -> Result<()> {
    let slot = worktree_config_slot(repo);
    let mut entry = slot.lock().await;
    entry.count = entry.count.saturating_sub(1);
    if entry.count == 0 && entry.we_enabled {
        disable_worktree_config(repo).await?;
        entry.we_enabled = false;
    }
    Ok(())
}

/// Point a single worktree at its own hooks directory.
///
/// `core.hooksPath` is normally repo-wide; scoping it with `--worktree` keeps
/// the operator's own hooks untouched in the primary worktree, and the setting
/// disappears together with the worktree.
pub async fn set_worktree_hooks_path(worktree: &Path, hooks_dir: &Path) -> Result<()> {
    let dir = hooks_dir.to_string_lossy().replace('\\', "/");
    git(worktree, &["config", "--worktree", "core.hooksPath", &dir])
        .await
        .map(|_| ())
}

/// Exclude a path from a worktree's status without touching `.gitignore`.
pub async fn local_exclude(worktree: &Path, pattern: &str) -> Result<()> {
    let git_dir = git(worktree, &["rev-parse", "--git-path", "info/exclude"]).await?;
    let path = worktree.join(git_dir);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut body = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    if body.lines().any(|l| l.trim() == pattern) {
        return Ok(());
    }
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(pattern);
    body.push('\n');
    tokio::fs::write(&path, body)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// `git merge --no-ff` of `branch` into the currently checked-out branch.
///
/// One of three ways to land a branch driven by [`crate::config::MergeStyle`]
/// — see [`merge_squash`] and [`merge_ff_only`] for the other two, and that
/// enum's own doc for why the choice between them lives in configuration.
pub async fn merge_no_ff(repo: &Path, branch: &str, message: &str) -> Result<GitOut> {
    git_raw(
        repo,
        &["merge", "--no-ff", "--no-edit", "-m", message, branch],
    )
    .await
}

/// `git merge --squash` of `branch`, followed by a commit under `message`.
///
/// Two `git` calls because `--squash` only stages the result — unlike
/// [`merge_no_ff`] there is no merge commit for `--no-edit` to write, and
/// skipping the second call is exactly the trap `land`'s module doc warns
/// about: a squash that inherits `branch`'s own single-commit subject
/// (`magi: candidate A (uncommitted work)`) instead of `message`. Returns the
/// `--squash` step's own output, unrun `commit` included, when staging itself
/// fails (a conflict), so a caller sees what actually went wrong rather than
/// a `git commit` complaint about nothing being staged.
pub async fn merge_squash(repo: &Path, branch: &str, message: &str) -> Result<GitOut> {
    let staged = git_raw(repo, &["merge", "--squash", branch]).await?;
    if !staged.ok() {
        return Ok(staged);
    }
    git_raw(repo, &["commit", "-m", message]).await
}

/// Fast-forward `branch` into the currently checked-out branch, refusing to
/// create a merge commit.
///
/// Only ever fast-forwards because the winner was already rebased onto the
/// tracked base tip before this runs (`Runner::sync_to_base`); at that point
/// `--ff-only` is indistinguishable from GitHub's "rebase and merge" button.
/// If the base moved again in the meantime this fails rather than falling
/// back to a real rebase, the same way `merge_no_ff` fails rather than
/// resolving a conflict — landing is not the place to improvise.
pub async fn merge_ff_only(repo: &Path, branch: &str) -> Result<GitOut> {
    git_raw(repo, &["merge", "--ff-only", branch]).await
}

/// Push a branch to `remote`.
pub async fn push(repo: &Path, remote: &str, branch: &str) -> Result<GitOut> {
    git_raw(repo, &["push", "-u", remote, branch]).await
}

/// Force-push a branch that has been rewritten, refusing to clobber work
/// pushed since this side last looked.
///
/// `--force-with-lease` rather than `--force`: a rebase replaces the branch's
/// commits, so a plain push is rejected, but a blind force would also throw
/// away anything a person pushed to the same branch meanwhile. The lease
/// turns that case into a failure instead of a loss.
pub async fn push_rewritten(repo: &Path, remote: &str, branch: &str) -> Result<GitOut> {
    git_raw(repo, &["push", "--force-with-lease", remote, branch]).await
}

/// Force-push `branch` only if `remote` still has it at `expected`.
///
/// The lease is pinned to a sha the caller read itself, not to whatever the
/// remote-tracking ref says at push time: a `fetch` in between moves the
/// tracking ref, and a bare lease would then be measured against the very
/// commit it was meant to protect a person's push from. A remote that has
/// moved on refuses the push, so a concurrent push fails the step instead of
/// being lost.
pub async fn push_pinned(
    repo: &Path,
    remote: &str,
    branch: &str,
    expected: &str,
) -> Result<GitOut> {
    let lease = format!("--force-with-lease=refs/heads/{branch}:{expected}");
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    git_raw(repo, &["push", &lease, remote, &refspec]).await
}

/// `git cherry <upstream> <head>`, split into the commits of `head` that have
/// no patch-id twin in `upstream` (`+`) and those that do (`-`).
pub async fn cherry(repo: &Path, upstream: &str, head: &str) -> Result<(Vec<String>, Vec<String>)> {
    let out = git(repo, &["cherry", upstream, head]).await?;
    let (mut unmatched, mut matched) = (Vec::new(), Vec::new());
    for line in out.lines() {
        if let Some(sha) = line.strip_prefix("+ ") {
            unmatched.push(sha.trim().to_owned());
        } else if let Some(sha) = line.strip_prefix("- ") {
            matched.push(sha.trim().to_owned());
        }
    }
    Ok((unmatched, matched))
}

/// One commit as a rebase preserves it: the sha plus the attributes git keeps
/// when it replays a commit (author name, email, author date, subject). A
/// replayed commit changes sha and patch-id, but not these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitKey {
    /// The commit id.
    pub sha: String,
    /// Author name, email, author date and subject, joined.
    pub key: String,
}

/// The non-merge commits in `range`, oldest first, with their [`CommitKey`].
pub async fn commit_keys(repo: &Path, range: &str) -> Result<Vec<CommitKey>> {
    let out = git(
        repo,
        &[
            "log",
            "--no-merges",
            "--reverse",
            "--date=raw",
            "--format=%H%x1f%an%x1f%ae%x1f%ad%x1f%s",
            range,
        ],
    )
    .await?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\u{1f}'))
        .map(|(sha, key)| CommitKey {
            sha: sha.to_owned(),
            key: key.to_owned(),
        })
        .collect())
}

/// How [`rebase_start`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseStart {
    /// It applied; the branch points at the rebased commits and the throwaway
    /// worktree is gone.
    Applied,
    /// It stopped on a conflict and the throwaway worktree was **kept**
    /// mid-rebase, for someone to resolve. Carries what git said.
    Conflicted(String),
    /// It failed without leaving a rebase in progress; the worktree is gone
    /// and the branch is untouched. Carries what git said.
    Failed(String),
}

/// Start rebasing `branch` onto `onto` inside a throwaway worktree, and leave
/// a conflict standing.
///
/// A worktree of its own for two reasons. The repository magi runs in may be
/// jj-colocated, where git `HEAD` is detached and a rebase in the primary
/// tree would move it under the operator; and a rebase that hits a conflict
/// leaves state behind, which is far easier to discard with the whole
/// directory than to unpick in a tree somebody is using.
///
/// Unlike [`rebase_branch_in_temp`] this does not abort on a conflict: the
/// caller decides whether to hand the conflicted tree to a fixer or to call
/// [`rebase_abort`]. Any leftover at `scratch` from an earlier attempt is
/// removed first, so callers re-entering a rebase already in progress must
/// check [`rebase_in_progress`] before calling this.
pub async fn rebase_start(
    repo: &Path,
    scratch: &Path,
    branch: &str,
    onto: &str,
) -> Result<RebaseStart> {
    // Removed first so a leftover from an interrupted attempt cannot make
    // `worktree add` fail on a path that already exists.
    worktree_remove(repo, scratch).await.ok();
    git_raw(
        repo,
        &[
            "worktree",
            "add",
            "--force",
            &scratch.to_string_lossy(),
            branch,
        ],
    )
    .await?;

    let out = git_raw(scratch, &["rebase", onto]).await?;
    if out.ok() {
        worktree_remove(repo, scratch).await.ok();
        return Ok(RebaseStart::Applied);
    }
    let why = if out.stderr.trim().is_empty() {
        out.stdout.trim().to_owned()
    } else {
        out.stderr.trim().to_owned()
    };
    if rebase_in_progress(scratch).await {
        return Ok(RebaseStart::Conflicted(why));
    }
    worktree_remove(repo, scratch).await.ok();
    Ok(RebaseStart::Failed(why))
}

/// Is a rebase (merge or apply backend) in progress in `worktree`?
pub async fn rebase_in_progress(worktree: &Path) -> bool {
    for name in ["rebase-merge", "rebase-apply"] {
        let Ok(p) = git(worktree, &["rev-parse", "--git-path", name]).await else {
            continue;
        };
        let p = Path::new(p.trim());
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            worktree.join(p)
        };
        if full.exists() {
            return true;
        }
    }
    false
}

/// Paths git reports as unmerged in `worktree`.
pub async fn unmerged_paths(worktree: &Path) -> Result<Vec<String>> {
    let out = git(worktree, &["diff", "--name-only", "--diff-filter=U"]).await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Abandon a rebase left standing by [`rebase_start`] and drop its worktree.
/// The branch is exactly where it was before the rebase began.
pub async fn rebase_abort(repo: &Path, scratch: &Path) {
    git_raw(scratch, &["rebase", "--abort"]).await.ok();
    worktree_remove(repo, scratch).await.ok();
}

/// Rebase a branch onto `onto`, inside a throwaway worktree.
///
/// `Ok(None)` means it applied and the branch now points at the rebased
/// commits. `Ok(Some(why))` means it did not: the branch is untouched, and
/// the string is what git said - a person has to decide. Nothing half-rebased
/// is left behind; see [`rebase_start`] for the variant that keeps a conflict
/// standing.
pub async fn rebase_branch_in_temp(
    repo: &Path,
    scratch: &Path,
    branch: &str,
    onto: &str,
) -> Result<Option<String>> {
    match rebase_start(repo, scratch, branch, onto).await? {
        RebaseStart::Applied => Ok(None),
        RebaseStart::Failed(why) => Ok(Some(why)),
        RebaseStart::Conflicted(why) => {
            rebase_abort(repo, scratch).await;
            Ok(Some(why))
        }
    }
}

/// Bring an *attached* worktree's index and files in line with wherever its
/// branch now points.
///
/// [`rebase_branch_in_temp`] moves a branch from a throwaway worktree on
/// purpose - the whole point is never touching the tree someone else has
/// checked out. But a worktree that already had that branch checked out
/// shares the same ref: its `HEAD` resolves to the new commit the moment the
/// rebase lands elsewhere, while its index and working directory keep
/// whatever the old commit put there until something says otherwise. Left
/// alone, the next `git status` there reads as the whole rebase turning up
/// as an unstaged diff, and the next commit would be staged against stale
/// content.
pub async fn sync_to_head(worktree: &Path) -> Result<()> {
    git(worktree, &["reset", "--hard", "HEAD"]).await?;
    git(worktree, &["clean", "-fdx"]).await?;
    Ok(())
}

/// Fetch one branch from `remote`, updating its remote-tracking ref.
///
/// The refspec is spelled out rather than left to `git fetch <remote>
/// <branch>`, which writes `FETCH_HEAD` and updates
/// `refs/remotes/<remote>/<branch>` only as a side effect of the remote's
/// configured refspec. Naming the destination makes the thing this function
/// exists for - a tracking ref that moved - the operation rather than a
/// consequence of configuration magi does not own.
///
/// Honest note: a CI failure was first read as proof that some git versions do
/// not update the tracking ref here. That was wrong - the fetch had nothing to
/// update because the test had pushed to the wrong branch - so this is
/// determinism, not a fix for a demonstrated portability bug.
///
/// Refs, not the working copy: nothing is checked out and no local branch
/// moves, so this is safe to run while the operator has uncommitted work.
/// Returned as a [`GitOut`] rather than an error so the caller can decide - a
/// machine with no network must still be able to start a run.
pub async fn fetch(repo: &Path, remote: &str, branch: &str) -> Result<GitOut> {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    git_raw(repo, &["fetch", "--quiet", remote, &refspec]).await
}

/// The best common ancestor of `a` and `b`, or an error when there is none.
pub async fn merge_base(repo: &Path, a: &str, b: &str) -> Result<String> {
    Ok(git(repo, &["merge-base", a, b]).await?.trim().to_owned())
}

/// Is `ancestor` an ancestor of (or equal to) `of`?
pub async fn is_ancestor(repo: &Path, ancestor: &str, of: &str) -> bool {
    git_raw(repo, &["merge-base", "--is-ancestor", ancestor, of])
        .await
        .is_ok_and(|o| o.ok())
}

/// [`is_ancestor`] that keeps "no" apart from "could not tell": `merge-base
/// --is-ancestor` exits 0 for yes, 1 for no and anything else for an error
/// (an unknown object, a shallow clone), and reading an error as "no" would
/// report a merged change as unmerged.
pub async fn ancestry(repo: &Path, ancestor: &str, of: &str) -> Result<bool> {
    let out = git_raw(repo, &["merge-base", "--is-ancestor", ancestor, of]).await?;
    match out.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor {ancestor} {of} failed (exit {:?}): {}",
            out.code,
            out.stderr
        ),
    }
}

/// The commit `rev` names, or `None` when it names no commit here.
pub async fn commit_of(repo: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    let out = git_raw(repo, &["rev-parse", "--verify", "--quiet", &spec])
        .await
        .ok()?;
    (out.ok() && !out.stdout.is_empty()).then_some(out.stdout)
}

/// Local and remote-tracking branches that contain `commit`.
pub async fn branches_containing(repo: &Path, commit: &str) -> Result<Vec<String>> {
    let out = git(
        repo,
        &[
            "branch",
            "-a",
            "--contains",
            commit,
            "--format=%(refname:short)",
        ],
    )
    .await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD") && !l.contains("HEAD detached"))
        .map(str::to_owned)
        .collect())
}

/// Cherry-pick `commit` onto the worktree's HEAD under a neutral committer.
/// On a conflict the pick is aborted, so the worktree is left as it was, and
/// git's own words come back as the error.
pub async fn cherry_pick(worktree: &Path, commit: &str) -> Result<()> {
    let out = git_raw(
        worktree,
        &[
            "-c",
            "user.name=magi candidate",
            "-c",
            "user.email=magi@localhost",
            "cherry-pick",
            "-x",
            commit,
        ],
    )
    .await?;
    if out.ok() {
        return Ok(());
    }
    let _ = git_raw(worktree, &["cherry-pick", "--abort"]).await;
    bail!(
        "cherry-pick of {commit} failed: {}",
        if out.stderr.is_empty() {
            out.stdout
        } else {
            out.stderr
        }
    )
}

/// The tree object id of `rev`.
pub async fn tree_of(repo: &Path, rev: &str) -> Result<String> {
    git(repo, &["rev-parse", &format!("{rev}^{{tree}}")]).await
}

/// Does this ref resolve?
pub async fn rev_exists(repo: &Path, rev: &str) -> bool {
    git_raw(repo, &["rev-parse", "--verify", "--quiet", rev])
        .await
        .is_ok_and(|o| o.ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_holder_is_read_from_the_porcelain_listing() {
        let listing = "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
                       worktree /wt/cand-A\nHEAD bbb\nbranch refs/heads/magi/f82f/A\n\n\
                       worktree /wt/detached\nHEAD ccc\ndetached\n";
        assert_eq!(
            parse_worktree_holder(listing, "magi/f82f/A"),
            Some(PathBuf::from("/wt/cand-A"))
        );
        assert_eq!(parse_worktree_holder(listing, "magi/f82f"), None);
        assert_eq!(parse_worktree_holder(listing, "other"), None);
    }

    async fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        tokio::fs::create_dir_all(&repo).await.unwrap();
        git(&repo, &["init", "-b", "main"]).await.unwrap();
        git(&repo, &["config", "user.name", "test"]).await.unwrap();
        git(&repo, &["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        tokio::fs::write(repo.join("a.txt"), "one\n").await.unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "init"]).await.unwrap();
        (dir, repo)
    }

    #[tokio::test]
    async fn a_branch_rebases_onto_a_moved_base_and_says_when_it_cannot() {
        let (_g, repo) = scratch().await;

        // A side branch touching a different file: rebases cleanly.
        git(&repo, &["checkout", "-b", "side"]).await.unwrap();
        tokio::fs::write(repo.join("b.txt"), "side\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "side work"]).await.unwrap();

        // main moves under it, which is what a repository merging other
        // pull requests does to a competition that took two hours.
        git(&repo, &["checkout", "main"]).await.unwrap();
        tokio::fs::write(repo.join("c.txt"), "main\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "main moved"]).await.unwrap();

        let scratch_tree = repo.parent().unwrap().join("rebase-scratch");
        let clean = rebase_branch_in_temp(&repo, &scratch_tree, "side", "main")
            .await
            .unwrap();
        assert!(clean.is_none(), "a disjoint change rebases: {clean:?}");
        assert_eq!(
            commits_ahead(&repo, "main", "side").await.unwrap(),
            1,
            "one commit, replayed onto the new base"
        );
        assert!(
            !scratch_tree.exists(),
            "the throwaway worktree is not left behind"
        );

        // A real conflict: both sides edit the same line.
        git(&repo, &["checkout", "-b", "clash"]).await.unwrap();
        tokio::fs::write(repo.join("a.txt"), "clash\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "clash"]).await.unwrap();
        git(&repo, &["checkout", "main"]).await.unwrap();
        tokio::fs::write(repo.join("a.txt"), "main edit\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "main edit"]).await.unwrap();

        let before = rev_parse(&repo, "clash").await.unwrap();
        let why = rebase_branch_in_temp(&repo, &scratch_tree, "clash", "main")
            .await
            .unwrap()
            .expect("a same-line clash cannot be rebased silently");
        assert!(
            why.to_lowercase().contains("conflict"),
            "the reason is what git said, which is what a person needs: {why}"
        );
        assert_eq!(
            rev_parse(&repo, "clash").await.unwrap(),
            before,
            "a failed rebase leaves the branch exactly where it was"
        );
        assert!(!scratch_tree.exists(), "and cleans up after itself");
    }

    #[tokio::test]
    async fn merge_squash_folds_the_branch_into_one_commit_under_the_given_message() {
        let (_g, repo) = scratch().await;
        git(&repo, &["checkout", "-b", "side"]).await.unwrap();
        for name in ["b.txt", "c.txt"] {
            tokio::fs::write(repo.join(name), "side\n").await.unwrap();
            git(&repo, &["add", "-A"]).await.unwrap();
            git(
                &repo,
                &["commit", "-m", "magi: candidate A (uncommitted work)"],
            )
            .await
            .unwrap();
        }
        git(&repo, &["checkout", "main"]).await.unwrap();
        let before = rev_parse(&repo, "main").await.unwrap();

        let out = merge_squash(&repo, "side", "an explicit subject")
            .await
            .unwrap();
        assert!(out.ok(), "{}", out.stderr);
        assert_eq!(
            commits_ahead(&repo, &before, "main").await.unwrap(),
            1,
            "squash adds exactly one commit onto the tip, not one per candidate commit"
        );
        let subject = git(&repo, &["log", "-1", "--format=%s"]).await.unwrap();
        assert_eq!(
            subject, "an explicit subject",
            "the candidate's own placeholder subject must not survive: {subject}"
        );
    }

    #[tokio::test]
    async fn merge_ff_only_fast_forwards_a_branch_already_rebased_onto_the_tip() {
        let (_g, repo) = scratch().await;
        git(&repo, &["checkout", "-b", "side"]).await.unwrap();
        tokio::fs::write(repo.join("b.txt"), "side\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "side work"]).await.unwrap();
        git(&repo, &["checkout", "main"]).await.unwrap();

        let before = rev_parse(&repo, "side").await.unwrap();
        let out = merge_ff_only(&repo, "side").await.unwrap();
        assert!(out.ok(), "{}", out.stderr);
        assert_eq!(
            rev_parse(&repo, "main").await.unwrap(),
            before,
            "a fast-forward moves the base tip to the branch, no merge commit"
        );
    }

    #[tokio::test]
    async fn merge_ff_only_refuses_to_write_a_merge_commit() {
        let (_g, repo) = scratch().await;
        git(&repo, &["checkout", "-b", "side"]).await.unwrap();
        tokio::fs::write(repo.join("b.txt"), "side\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "side work"]).await.unwrap();

        // main diverges, so a fast-forward is no longer possible.
        git(&repo, &["checkout", "main"]).await.unwrap();
        tokio::fs::write(repo.join("c.txt"), "main\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "main moved"]).await.unwrap();

        let before = rev_parse(&repo, "main").await.unwrap();
        let out = merge_ff_only(&repo, "side").await.unwrap();
        assert!(!out.ok(), "a divergent branch cannot fast-forward");
        assert_eq!(
            rev_parse(&repo, "main").await.unwrap(),
            before,
            "a refused fast-forward must not touch main"
        );
    }

    #[tokio::test]
    async fn a_sibling_worktree_stays_stale_after_a_rebase_until_synced() {
        let (guard, repo) = scratch().await;

        // An attached worktree of an existing branch - the shape a winner's
        // worktree keeps in `graph::Runner`, not the detached checkouts used
        // for judges and reviewers.
        git(&repo, &["branch", "side"]).await.unwrap();
        let side_wt = guard.path().join("side-wt");
        git(
            &repo,
            &["worktree", "add", &side_wt.to_string_lossy(), "side"],
        )
        .await
        .unwrap();
        tokio::fs::write(side_wt.join("b.txt"), "candidate\n")
            .await
            .unwrap();
        git(&side_wt, &["add", "-A"]).await.unwrap();
        git(&side_wt, &["commit", "-m", "side work"]).await.unwrap();

        // main moves under it.
        git(&repo, &["checkout", "main"]).await.unwrap();
        tokio::fs::write(repo.join("c.txt"), "main\n")
            .await
            .unwrap();
        git(&repo, &["add", "-A"]).await.unwrap();
        git(&repo, &["commit", "-m", "main moved"]).await.unwrap();

        // Rebase from a throwaway worktree, never from `side_wt` itself.
        let scratch_tree = guard.path().join("rebase-scratch");
        let clean = rebase_branch_in_temp(&repo, &scratch_tree, "side", "main")
            .await
            .unwrap();
        assert!(clean.is_none());

        // `HEAD` in the sibling worktree already resolves to the rebased
        // commit - the ref is shared - but nothing has told its index or its
        // files, which still hold the pre-rebase checkout.
        assert_eq!(
            rev_parse(&side_wt, "HEAD").await.unwrap(),
            rev_parse(&repo, "side").await.unwrap(),
            "HEAD follows the moved ref"
        );
        assert!(
            !side_wt.join("c.txt").exists(),
            "stale until synced: main's new file has not reached this worktree's disk"
        );

        sync_to_head(&side_wt).await.unwrap();
        assert!(side_wt.join("c.txt").is_file(), "synced now");
        assert!(
            side_wt.join("b.txt").is_file(),
            "the worktree's own committed work survives the sync"
        );
        assert!(is_clean(&side_wt).await.unwrap());
    }

    #[tokio::test]
    async fn clean_repo_reports_clean_then_dirty() {
        let (_g, repo) = scratch().await;
        assert!(is_clean(&repo).await.unwrap());
        tokio::fs::write(repo.join("a.txt"), "two\n").await.unwrap();
        assert!(!is_clean(&repo).await.unwrap());
    }

    async fn track(repo: &Path, name: &str, body: &str) {
        let p = repo.join(name);
        if let Some(d) = p.parent() {
            tokio::fs::create_dir_all(d).await.unwrap();
        }
        tokio::fs::write(&p, body).await.unwrap();
        git(repo, &["add", name]).await.unwrap();
        git(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@localhost",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rescue_withholds_a_foreign_lockfile() {
        let (_g, repo) = scratch().await;
        track(&repo, "web/bun.lock", "a\n").await;
        tokio::fs::write(repo.join("web/pnpm-lock.yaml"), "x\n")
            .await
            .unwrap();
        tokio::fs::write(repo.join("web/app.ts"), "real\n")
            .await
            .unwrap();

        let r = rescue_commit(&repo, "rescue").await.unwrap();
        assert!(r.committed);
        assert_eq!(
            r.withheld,
            [Stray {
                path: "web/pnpm-lock.yaml".to_owned(),
                manager: "pnpm".to_owned(),
                kept_by: "web/bun.lock".to_owned(),
            }]
        );
        let files = git(&repo, &["show", "--name-only", "--format=", "HEAD"])
            .await
            .unwrap();
        assert!(files.contains("web/app.ts"), "{files}");
        assert!(!files.contains("pnpm-lock"), "{files}");
        assert!(repo.join("web/pnpm-lock.yaml").is_file(), "not deleted");
    }

    #[tokio::test]
    async fn rescue_with_only_a_stray_commits_nothing() {
        let (_g, repo) = scratch().await;
        track(&repo, "bun.lock", "a\n").await;
        tokio::fs::write(repo.join("yarn.lock"), "x\n")
            .await
            .unwrap();
        let r = rescue_commit(&repo, "rescue").await.unwrap();
        assert!(!r.committed);
        assert_eq!(r.withheld.len(), 1);
    }

    #[tokio::test]
    async fn rescue_keeps_a_same_manager_lockfile_update() {
        let (_g, repo) = scratch().await;
        track(&repo, "bun.lock", "a\n").await;
        tokio::fs::write(repo.join("bun.lock"), "b\n")
            .await
            .unwrap();
        let r = rescue_commit(&repo, "rescue").await.unwrap();
        assert!(r.committed);
        assert!(r.withheld.is_empty());
        let files = git(&repo, &["show", "--name-only", "--format=", "HEAD"])
            .await
            .unwrap();
        assert_eq!(files, "bun.lock");
    }

    #[tokio::test]
    async fn rescue_keeps_the_first_lockfile_in_a_bare_directory() {
        let (_g, repo) = scratch().await;
        track(&repo, "other/bun.lock", "a\n").await;
        tokio::fs::create_dir_all(repo.join("web")).await.unwrap();
        tokio::fs::write(repo.join("web/package-lock.json"), "{}\n")
            .await
            .unwrap();
        let r = rescue_commit(&repo, "rescue").await.unwrap();
        assert!(r.committed);
        assert!(r.withheld.is_empty());
    }

    #[test]
    fn a_cargo_lock_is_foreign_only_without_a_cargo_toml() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert_eq!(stray_lockfiles(&s(&["a/Cargo.lock"]), &s(&[])).len(), 1);
        assert!(stray_lockfiles(&s(&["a/Cargo.lock"]), &s(&["a/Cargo.toml"])).is_empty());
        assert!(stray_lockfiles(&s(&["a/Cargo.lock", "a/Cargo.toml"]), &s(&[])).is_empty());
        // A manifest in another directory does not count.
        assert_eq!(
            stray_lockfiles(&s(&["a/Cargo.lock"]), &s(&["Cargo.toml"])).len(),
            1
        );
    }

    #[tokio::test]
    async fn worktree_lifecycle_and_diff() {
        let (guard, repo) = scratch().await;
        let base = rev_parse(&repo, "HEAD").await.unwrap();
        let wt = guard.path().join("wt-a");
        worktree_add_branch(&repo, &wt, "magi/test/a", &base)
            .await
            .unwrap();
        tokio::fs::write(wt.join("b.txt"), "candidate\n")
            .await
            .unwrap();

        assert!(commit_all(&wt, "candidate work").await.unwrap());
        assert!(!commit_all(&wt, "nothing left").await.unwrap());

        assert_eq!(commits_ahead(&wt, &base, "HEAD").await.unwrap(), 1);
        let patch = diff(&wt, &base, "HEAD").await.unwrap();
        assert!(patch.contains("b.txt"), "patch was: {patch}");
        assert_eq!(
            changed_files(&wt, &base, "HEAD").await.unwrap(),
            ["b.txt".to_owned()]
        );

        // The rescue commit must not carry the operator's identity.
        let author = git(&wt, &["log", "-1", "--format=%an <%ae>"])
            .await
            .unwrap();
        assert_eq!(author, "magi candidate <magi@localhost>");

        assert!(worktree_remove(&repo, &wt).await.unwrap());
        assert!(branch_exists(&repo, "magi/test/a").await.unwrap());
        assert!(branch_delete(&repo, "magi/test/a").await.unwrap());
        assert!(!branch_exists(&repo, "magi/test/a").await.unwrap());
    }

    #[tokio::test]
    async fn worktree_scoped_hooks_path_does_not_leak_to_primary() {
        let (guard, repo) = scratch().await;
        let base = rev_parse(&repo, "HEAD").await.unwrap();
        let wt = guard.path().join("wt-h");
        worktree_add_branch(&repo, &wt, "magi/test/h", &base)
            .await
            .unwrap();
        let hooks = guard.path().join("hooks");
        tokio::fs::create_dir_all(&hooks).await.unwrap();

        assert!(enable_worktree_config(&repo).await.unwrap());
        set_worktree_hooks_path(&wt, &hooks).await.unwrap();

        let in_wt = git(&wt, &["config", "--get", "core.hooksPath"])
            .await
            .unwrap();
        assert!(!in_wt.is_empty());
        let in_primary = git_raw(&repo, &["config", "--get", "core.hooksPath"])
            .await
            .unwrap();
        assert!(
            !in_primary.ok(),
            "primary worktree must keep its own hooks: {in_primary:?}"
        );

        disable_worktree_config(&repo).await.unwrap();
    }

    #[tokio::test]
    async fn worktree_config_stays_on_while_a_sibling_run_still_holds_it() {
        let (_g, repo) = scratch().await;

        // Two runs in the same repository, as `Config::daemon.max_concurrent_runs`
        // now allows: both acquire before either is done.
        acquire_worktree_config(&repo).await.unwrap();
        acquire_worktree_config(&repo).await.unwrap();

        let on = git(&repo, &["config", "--get", "extensions.worktreeConfig"])
            .await
            .unwrap();
        assert_eq!(on, "true");

        // The first run to finish releases its own reference. A plain
        // `disable_worktree_config` here is exactly the bug: it would turn
        // the setting off while the second run still depends on it.
        release_worktree_config(&repo).await.unwrap();
        let still_on = git(&repo, &["config", "--get", "extensions.worktreeConfig"])
            .await
            .unwrap();
        assert_eq!(
            still_on, "true",
            "a sibling run's release must not disable the setting for the one still working"
        );

        // Only the last release actually turns it back off.
        release_worktree_config(&repo).await.unwrap();
        let after = git_raw(&repo, &["config", "--get", "extensions.worktreeConfig"])
            .await
            .unwrap();
        assert!(
            !after.ok(),
            "the last release must turn the setting back off: {after:?}"
        );
    }

    #[tokio::test]
    async fn worktree_config_already_on_before_magi_touched_it_is_left_alone() {
        let (_g, repo) = scratch().await;
        git(&repo, &["config", "extensions.worktreeConfig", "true"])
            .await
            .unwrap();

        // magi did not turn this on, so even after every acquire is released,
        // it must not turn it off - that is what a bare `enable_worktree_config`
        // already promised for the single-run case, and the ref-counted
        // version must keep that promise.
        acquire_worktree_config(&repo).await.unwrap();
        release_worktree_config(&repo).await.unwrap();

        let still_on = git(&repo, &["config", "--get", "extensions.worktreeConfig"])
            .await
            .unwrap();
        assert_eq!(still_on, "true");
    }

    #[tokio::test]
    async fn local_exclude_is_idempotent() {
        let (_g, repo) = scratch().await;
        local_exclude(&repo, "/.magi/").await.unwrap();
        local_exclude(&repo, "/.magi/").await.unwrap();
        let path = repo.join(".git/info/exclude");
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(body.matches("/.magi/").count(), 1);
    }

    fn sh(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .quiet()
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A checkout whose `origin` is a local bare repository holding `trunk`,
    /// with `origin/HEAD` unset (as after `git remote add` + push).
    async fn with_remote() -> (tempfile::TempDir, PathBuf) {
        let (g, repo) = scratch().await;
        let bare = g.path().join("bare.git");
        sh(g.path(), &["init", "--bare", "-b", "trunk", "bare.git"]);
        sh(&repo, &["remote", "add", "origin", &bare.to_string_lossy()]);
        sh(&repo, &["push", "origin", "HEAD:refs/heads/trunk"]);
        sh(&repo, &["fetch", "origin"]);
        (g, repo)
    }

    #[test]
    fn remote_head_branch_strips_only_the_matching_remote() {
        assert_eq!(
            remote_head_branch("origin", "refs/remotes/origin/main\n").as_deref(),
            Some("main")
        );
        assert_eq!(remote_head_branch("up", "refs/remotes/origin/main"), None);
        assert_eq!(remote_head_branch("origin", "refs/remotes/origin/"), None);
    }

    #[tokio::test]
    async fn merge_base_branch_reads_origin_head_when_present() {
        let (_g, repo) = with_remote().await;
        sh(&repo, &["remote", "set-head", "origin", "trunk"]);
        assert_eq!(
            merge_base_branch(&repo, "origin", None).await.unwrap(),
            "trunk"
        );
    }

    #[tokio::test]
    async fn merge_base_branch_recovers_a_missing_origin_head() {
        let (_g, repo) = with_remote().await;
        // Newer git sets it on fetch; make the precondition true everywhere.
        sh(&repo, &["remote", "set-head", "origin", "-d"]);
        let head = git_raw(
            &repo,
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        )
        .await
        .unwrap();
        assert!(!head.ok(), "precondition: no origin/HEAD");
        assert_eq!(
            merge_base_branch(&repo, "origin", None).await.unwrap(),
            "trunk"
        );
    }

    #[tokio::test]
    async fn merge_base_branch_errors_without_a_remote_and_never_uses_head() {
        let (_g, repo) = scratch().await;
        let err = merge_base_branch(&repo, "origin", None).await.unwrap_err();
        assert!(format!("{err:#}").contains("[merge] base"), "{err:#}");
    }

    #[tokio::test]
    async fn merge_base_branch_prefers_the_explicit_base() {
        let (_g, repo) = with_remote().await;
        assert_eq!(
            merge_base_branch(&repo, "origin", Some("release"))
                .await
                .unwrap(),
            "release"
        );
    }

    #[tokio::test]
    async fn merge_base_branch_names_an_unfetched_default_branch() {
        // A remote added after the fact, never fetched: no tracking refs, so
        // `set-head -a` cannot work, but the remote's own HEAD still names it.
        let (g, repo) = scratch().await;
        let bare = g.path().join("bare.git");
        sh(g.path(), &["init", "--bare", "-b", "trunk", "bare.git"]);
        sh(
            &repo,
            &["push", &bare.to_string_lossy(), "HEAD:refs/heads/trunk"],
        );
        sh(&repo, &["remote", "add", "origin", &bare.to_string_lossy()]);
        assert!(!rev_exists(&repo, "refs/remotes/origin/trunk").await);
        assert_eq!(
            merge_base_branch(&repo, "origin", None).await.unwrap(),
            "trunk"
        );
    }

    #[test]
    fn symref_branch_reads_ls_remote_output() {
        assert_eq!(
            symref_branch("ref: refs/heads/main\tHEAD\nabc\tHEAD\n").as_deref(),
            Some("main")
        );
        assert_eq!(symref_branch("abc\tHEAD\n"), None);
    }
}
