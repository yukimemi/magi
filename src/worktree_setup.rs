//! `[worktree] setup`: what a fresh worktree needs beyond its tracked files.
//!
//! magi makes its worktrees with plain `git worktree add`, so a `.env` or a
//! one-off bootstrap that `renri add` would have produced is missing. Each
//! step runs in order, right after the worktree exists and before any agent
//! or verify command touches it. A failing step fails that worktree with the
//! step and its output; nothing here is ever swallowed.
//!
//! Three rules keep the competition honest:
//!
//! - **Blindness.** `MAGI_RUN` / `MAGI_NODE` are removed from a command's
//!   environment, and nothing seat- or agent-shaped is exported.
//! - **Products are not work.** Every path the steps leave untracked is
//!   recorded in the worktree's own git dir ([`RECORD`]) and hidden from `git
//!   add -A` by a per-worktree `core.excludesFile`; [`withheld_paths`] is the
//!   second line, used by the rescue commits.
//! - **Setup does not edit tracked files.** One that does fails the worktree,
//!   which also stops a `kata apply`-shaped step: every candidate would carry
//!   the same diff.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{Config, SetupStep};
use crate::git;
use crate::proc::Quiet as _;

/// File in the worktree's git dir listing the setup products, NUL-separated.
pub const RECORD: &str = "magi-setup-paths";
/// Per-worktree exclude file `core.excludesFile` points at.
const EXCLUDE: &str = "magi-setup-exclude";
/// Output kept in an error message.
const TAIL: usize = 4000;

/// Run `cfg.worktree.setup` in `wt`, a worktree of `repo` (the primary checkout).
///
/// A no-op for the default empty list. Idempotent, since a reviewer's worktree
/// is wiped and set up again every round.
pub async fn prepare(cfg: &Config, repo: &Path, wt: &Path) -> Result<()> {
    let steps = &cfg.worktree.setup;
    if steps.is_empty() {
        return Ok(());
    }
    let total = steps.len();
    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        let what = step.describe();
        match (&step.copy, &step.run) {
            (Some(spec), None) => copy_step(repo, wt, spec, step.optional)
                .await
                .with_context(|| format!("worktree setup step {n}/{total} ({what}) failed"))?,
            (None, Some(command)) => {
                let secs = step
                    .timeout_secs
                    .unwrap_or_else(|| cfg.graph.verify_timeout());
                run_step(&cfg.shell(), command, wt, Duration::from_secs(secs))
                    .await
                    .with_context(|| format!("worktree setup step {n}/{total} ({what}) failed"))?;
            }
            _ => bail!("worktree setup step {n}/{total} must set exactly one of copy / run"),
        }
    }
    seal(wt).await.context("worktree setup")
}

/// `src -> dst`, or just `path` for the same path on both sides.
fn split_copy(spec: &str) -> (&str, &str) {
    match spec.split_once("->") {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (spec.trim(), spec.trim()),
    }
}

/// Is `p` a path that stays inside the worktree, and out of `.git`?
pub fn valid_destination(p: &str) -> Result<()> {
    if p.is_empty() {
        bail!("destination is empty");
    }
    let path = Path::new(p);
    if path.is_absolute() || p.starts_with('/') || p.starts_with('\\') || p.contains(':') {
        bail!("destination `{p}` must be relative to the worktree");
    }
    let mut first = true;
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                if first && s.eq_ignore_ascii_case(".git") {
                    bail!("destination `{p}` is inside .git");
                }
                first = false;
            }
            Component::CurDir => {}
            _ => bail!("destination `{p}` must not contain `..`"),
        }
    }
    Ok(())
}

/// Validate a `copy` spec at load time.
pub fn validate_copy(spec: &str) -> Result<()> {
    let (src, dst) = split_copy(spec);
    if src.is_empty() {
        bail!("copy `{spec}` has no source");
    }
    valid_destination(dst)
}

async fn copy_step(repo: &Path, wt: &Path, spec: &str, optional: bool) -> Result<()> {
    let (src, dst) = split_copy(spec);
    validate_copy(spec)?;
    let src_path = if Path::new(src).is_absolute() {
        PathBuf::from(src)
    } else {
        repo.join(src)
    };
    let dst_path = wt.join(dst);
    // A destination reached through a symlink could land outside the worktree.
    let mut probe = wt.to_path_buf();
    for c in Path::new(dst).components() {
        if let Component::Normal(s) = c {
            probe.push(s);
            if let Ok(meta) = tokio::fs::symlink_metadata(&probe).await
                && meta.file_type().is_symlink()
            {
                bail!("destination `{dst}` goes through a symlink");
            }
        }
    }
    let data = match tokio::fs::read(&src_path).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && optional => return Ok(()),
        Err(e) => bail!("cannot read source {}: {e}", src_path.display()),
    };
    if let Some(parent) = dst_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }
    // `create_new`: an existing file in the worktree wins, never overwritten.
    use tokio::io::AsyncWriteExt as _;
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dst_path)
        .await
    {
        Ok(mut f) => {
            f.write_all(&data).await?;
            f.flush().await?;
            #[cfg(unix)]
            if let Ok(meta) = tokio::fs::metadata(&src_path).await {
                tokio::fs::set_permissions(&dst_path, meta.permissions())
                    .await
                    .ok();
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => bail!("cannot create {}: {e}", dst_path.display()),
    }
}

fn tail(s: &str) -> &str {
    let s = s.trim();
    if s.len() <= TAIL {
        return s;
    }
    let mut at = s.len() - TAIL;
    while !s.is_char_boundary(at) {
        at += 1;
    }
    &s[at..]
}

async fn run_step(shell: &[String], command: &str, wt: &Path, timeout: Duration) -> Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let log = std::env::temp_dir().join(format!(
        "magi-setup-{}-{}.out",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let out = std::fs::File::create(&log).context("create output file")?;
    let err = out.try_clone()?;
    let mut cmd = tokio::process::Command::new(&shell[0]);
    cmd.quiet();
    cmd.args(&shell[1..])
        .arg(command)
        .current_dir(wt)
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn `{}` for `{command}`", shell[0]))?;
    let pid = child.id();
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let body = || tail(&std::fs::read_to_string(&log).unwrap_or_default()).to_owned();
    let result = match waited {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(anyhow::anyhow!(
            "`{command}` exited with {status}\n{}",
            body()
        )),
        Ok(Err(e)) => Err(anyhow::anyhow!("`{command}` could not be awaited: {e}")),
        Err(_) => {
            kill_tree(pid, &mut child).await;
            Err(anyhow::anyhow!(
                "`{command}` timed out after {}s\n{}",
                timeout.as_secs(),
                body()
            ))
        }
    };
    std::fs::remove_file(&log).ok();
    result
}

/// Stop the command and, on unix, the process group it leads.
async fn kill_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let mut kill = tokio::process::Command::new("kill");
        kill.args(["-KILL", "--", &format!("-{pid}")]);
        kill.status().await.ok();
    }
    #[cfg(not(unix))]
    let _ = pid;
    child.start_kill().ok();
    child.wait().await.ok();
}

async fn git_dir(wt: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git::git(wt, &["rev-parse", "--absolute-git-dir"])
            .await?
            .trim(),
    ))
}

/// Paths a worktree's setup recorded, relative to it. Empty when none.
pub async fn withheld_paths(wt: &Path) -> Vec<String> {
    let Ok(dir) = git_dir(wt).await else {
        return Vec::new();
    };
    let Ok(raw) = tokio::fs::read(dir.join(RECORD)).await else {
        return Vec::new();
    };
    raw.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// Unstage whatever setup produced, so a rescue commit never carries it.
pub async fn unstage_products(wt: &Path) -> Result<()> {
    let paths = withheld_paths(wt).await;
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["reset", "-q", "--"];
    args.extend(paths.iter().map(String::as_str));
    git::git(wt, &args).await.map(|_| ())
}

/// After the steps: refuse tracked edits, record the untracked products and
/// hide them from `git add`.
async fn seal(wt: &Path) -> Result<()> {
    let tracked = git::git(wt, &["diff", "--name-only", "HEAD"]).await?;
    if !tracked.trim().is_empty() {
        bail!(
            "setup must not change tracked files (every seat would carry the same diff), \
             but it changed:\n{}",
            tail(&tracked)
        );
    }
    let out = git::git(wt, &["ls-files", "-z", "--others", "--exclude-standard"]).await?;
    let mut paths: Vec<String> = withheld_paths(wt).await;
    for p in out.split('\0').filter(|p| !p.is_empty()) {
        if !paths.iter().any(|q| q == p) {
            paths.push(p.to_owned());
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let dir = git_dir(wt).await?;
    let mut raw = Vec::new();
    for p in &paths {
        raw.extend_from_slice(p.as_bytes());
        raw.push(0);
    }
    tokio::fs::write(dir.join(RECORD), raw).await?;
    hide(wt, &dir, &paths).await;
    Ok(())
}

fn exclude_line(p: &str) -> String {
    let mut s = String::from("/");
    for c in p.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\' | '#' | '!' | ' ') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Point this worktree's `core.excludesFile` at its products, keeping the
/// user's own excludes. Best effort: needs `extensions.worktreeConfig`, and
/// the rescue commits' [`unstage_products`] holds when it is not on.
async fn hide(wt: &Path, dir: &Path, paths: &[String]) {
    let file = dir.join(EXCLUDE);
    let file_s = file.to_string_lossy().replace('\\', "/");
    let current = git::git_raw(wt, &["config", "--get", "core.excludesFile"])
        .await
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_owned())
        .filter(|s| !s.is_empty());
    let mut body = match current {
        Some(c) if c == file_s => std::fs::read_to_string(&file).unwrap_or_default(),
        Some(c) => {
            let c = match c.strip_prefix("~/") {
                Some(rest) => std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(rest))
                    .unwrap_or_else(|| PathBuf::from(&c)),
                None => PathBuf::from(&c),
            };
            std::fs::read_to_string(c).unwrap_or_default()
        }
        None => String::new(),
    };
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    for p in paths {
        let line = exclude_line(p);
        if !body.lines().any(|l| l == line) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    if std::fs::write(&file, body).is_err() {
        return;
    }
    if let Err(e) = git::git(wt, &["config", "--worktree", "core.excludesFile", &file_s]).await {
        tracing::debug!("worktree setup: products not hidden from git add: {e:#}");
    }
}

impl SetupStep {
    /// One-line description for errors.
    pub fn describe(&self) -> String {
        match (&self.copy, &self.run) {
            (Some(c), _) => format!("copy {c}"),
            (_, Some(r)) => format!("run {r}"),
            _ => "empty".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SetupStep;

    fn copy(spec: &str) -> SetupStep {
        SetupStep {
            copy: Some(spec.to_owned()),
            ..Default::default()
        }
    }

    fn run(cmd: &str) -> SetupStep {
        SetupStep {
            run: Some(cmd.to_owned()),
            ..Default::default()
        }
    }

    fn cfg(steps: Vec<SetupStep>) -> Config {
        let mut c = Config::default();
        c.worktree.setup = steps;
        c.verify.shell = Some(vec!["sh".to_owned(), "-c".to_owned()]);
        c
    }

    /// A repository with one commit and a detached linked worktree.
    async fn scratch() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "t"],
            vec!["config", "user.email", "t@example.com"],
        ] {
            git::git(&repo, &args).await.unwrap();
        }
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git::git(&repo, &["add", "-A"]).await.unwrap();
        git::git(&repo, &["commit", "-m", "init"]).await.unwrap();
        let wt = tmp.path().join("wt");
        git::worktree_add_detached(&repo, &wt, "HEAD")
            .await
            .unwrap();
        (tmp, repo, wt)
    }

    #[tokio::test]
    async fn default_is_a_no_op() {
        let (_t, repo, wt) = scratch().await;
        prepare(&Config::default(), &repo, &wt).await.unwrap();
        assert!(withheld_paths(&wt).await.is_empty());
    }

    #[tokio::test]
    async fn copy_then_run_in_order() {
        let (_t, repo, wt) = scratch().await;
        std::fs::write(repo.join(".env.example"), "KEY=1\n").unwrap();
        let c = cfg(vec![
            copy(".env.example -> .env"),
            run("cat .env > seen.txt && echo ran >> seen.txt"),
        ]);
        prepare(&c, &repo, &wt).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("seen.txt")).unwrap(),
            "KEY=1\nran\n"
        );
        let mut got = withheld_paths(&wt).await;
        got.sort();
        assert_eq!(got, [".env", "seen.txt"]);
    }

    #[tokio::test]
    async fn a_failing_step_stops_and_names_step_and_output() {
        let (_t, repo, wt) = scratch().await;
        let c = cfg(vec![run("echo boom >&2; exit 3"), run("touch after.txt")]);
        let err = format!("{:#}", prepare(&c, &repo, &wt).await.unwrap_err());
        assert!(err.contains("step 1/2"), "{err}");
        assert!(err.contains("boom"), "{err}");
        assert!(!wt.join("after.txt").exists());
    }

    #[tokio::test]
    async fn a_timeout_keeps_the_partial_output() {
        let (_t, repo, wt) = scratch().await;
        let mut step = run("echo partial; sleep 30");
        step.timeout_secs = Some(1);
        let err = format!(
            "{:#}",
            prepare(&cfg(vec![step]), &repo, &wt).await.unwrap_err()
        );
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("partial"), "{err}");
    }

    #[tokio::test]
    async fn copy_never_overwrites() {
        let (_t, repo, wt) = scratch().await;
        std::fs::write(repo.join("src.txt"), "new\n").unwrap();
        std::fs::write(wt.join("dst.txt"), "mine\n").unwrap();
        prepare(&cfg(vec![copy("src.txt -> dst.txt")]), &repo, &wt)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("dst.txt")).unwrap(),
            "mine\n"
        );
    }

    #[tokio::test]
    async fn a_missing_source_is_an_error_unless_optional() {
        let (_t, repo, wt) = scratch().await;
        let err = format!(
            "{:#}",
            prepare(&cfg(vec![copy("nope")]), &repo, &wt)
                .await
                .unwrap_err()
        );
        assert!(err.contains("step 1/1") && err.contains("nope"), "{err}");
        let mut opt = copy("nope");
        opt.optional = true;
        prepare(&cfg(vec![opt]), &repo, &wt).await.unwrap();
        assert!(!wt.join("nope").exists());
    }

    #[tokio::test]
    async fn setup_may_not_edit_tracked_files() {
        let (_t, repo, wt) = scratch().await;
        let err = format!(
            "{:#}",
            prepare(&cfg(vec![run("echo x >> a.txt")]), &repo, &wt)
                .await
                .unwrap_err()
        );
        assert!(err.contains("tracked") && err.contains("a.txt"), "{err}");
    }

    #[tokio::test]
    async fn the_command_does_not_see_the_run_identity() {
        let (_t, repo, wt) = scratch().await;
        // Safe in a unit test: only this process reads them, via the child.
        unsafe {
            std::env::set_var("MAGI_RUN", "r1");
            std::env::set_var("MAGI_NODE", "n1");
        }
        prepare(
            &cfg(vec![run("echo \"[$MAGI_RUN][$MAGI_NODE]\" > ids.txt")]),
            &repo,
            &wt,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("ids.txt")).unwrap(),
            "[][]\n"
        );
    }

    #[tokio::test]
    async fn products_are_kept_out_of_both_commit_paths() {
        let (_t, repo, wt) = scratch().await;
        git::acquire_worktree_config(&repo).await.unwrap();
        std::fs::write(repo.join("secret"), "s\n").unwrap();
        prepare(&cfg(vec![copy("secret")]), &repo, &wt)
            .await
            .unwrap();

        // Hidden from the agent's own `git add -A` ...
        git::git(&wt, &["add", "-A"]).await.unwrap();
        assert!(
            git::git(&wt, &["diff", "--cached", "--name-only"])
                .await
                .unwrap()
                .is_empty()
        );

        // ... and a rescue with only the product commits nothing.
        assert!(!git::commit_all(&wt, "rescue").await.unwrap());
        let r = git::rescue_commit(&wt, "rescue").await.unwrap();
        assert!(!r.committed);

        // Real work beside it is committed, the product is not.
        std::fs::write(wt.join("work.txt"), "w\n").unwrap();
        assert!(git::commit_all(&wt, "rescue").await.unwrap());
        let files = git::git(&wt, &["show", "--name-only", "--format=", "HEAD"])
            .await
            .unwrap();
        assert_eq!(files.trim(), "work.txt");
        git::release_worktree_config(&repo).await.unwrap();
    }

    #[tokio::test]
    async fn the_rescue_commits_hold_without_worktree_config() {
        let (_t, repo, wt) = scratch().await;
        std::fs::write(repo.join("secret"), "s\n").unwrap();
        prepare(&cfg(vec![copy("secret")]), &repo, &wt)
            .await
            .unwrap();
        std::fs::write(wt.join("work.txt"), "w\n").unwrap();
        let r = git::rescue_commit(&wt, "rescue").await.unwrap();
        assert!(r.committed);
        let files = git::git(&wt, &["show", "--name-only", "--format=", "HEAD"])
            .await
            .unwrap();
        assert_eq!(files.trim(), "work.txt");
    }

    #[test]
    fn destinations_must_stay_inside() {
        for bad in ["/etc/x", "../x", "a/../../x", ".git/hooks/x", "C:/x", ""] {
            assert!(valid_destination(bad).is_err(), "{bad}");
        }
        assert!(valid_destination("a/b.env").is_ok());
    }
}
