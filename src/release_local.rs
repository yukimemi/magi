//! Releasing without GitHub Actions (`[release] mode = "local"`).
//!
//! On a repository whose Actions never run, `auto-tag.yml` and `release.yml`
//! never fire, so nothing tags or publishes after the release pull request
//! merges. In local mode [`crate::release_watch`] merges that pull request on
//! the owner's approval and then drives the [`Job`] defined here, from a clean
//! detached checkout of the merge commit:
//!
//! 1. create and push the `vX.Y.Z` tag - **never over a tag that exists**
//!    ([`tag_step`] is the whole decision);
//! 2. run `[release] commands` one at a time, stopping at the first failure.
//!
//! - **Progress is saved before every step**, through the caller's `save`. A
//!   step that was started and never finished (a crash, a kill) is
//!   [`Job::interrupted`]: magi cannot know whether `gh release create` got as
//!   far as the forge, so it holds for the owner instead of repeating it.
//! - **A failure is a hold, not a retry.** [`run_job`] returns the reason; the
//!   watcher records it, raises a notice and asks. Resuming skips the tag and
//!   every command that already succeeded.
//! - **Environment, not templates.** Commands see `MAGI_RELEASE_VERSION`,
//!   `MAGI_RELEASE_TAG`, `MAGI_RELEASE_COMMIT` and `MAGI_RELEASE_PR`; teravars
//!   renders the whole config before any release exists, so `{{ version }}`
//!   cannot work.
//! - Only `git` and the commands the operator wrote run; there is no API path.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Release;
use crate::git;
use crate::proc::Quiet as _;

/// Bytes of a command's output kept in the record; the whole of it goes to
/// `release-<n>.out` beside the job.
const TAIL: usize = 4_000;

/// Head of a release branch name, before the version.
pub const BRANCH_PREFIX: &str = "chore/release-v";

/// What happened in one step, kept in the record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StepLog {
    /// `tag` or the command line.
    pub name: String,
    /// Exit code; `None` for a signal or a timeout.
    pub code: Option<i32>,
    /// Last bytes of the combined output.
    pub tail: String,
    /// Absolute path of the file holding the whole output, when one was kept.
    pub output: Option<String>,
}

/// One release of one merged pull request, resumable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    /// `X.Y.Z`, without the `v`.
    pub version: String,
    /// The merged release pull request.
    pub pr_url: String,
    /// The commit the pull request merged as; what the tag points at.
    pub commit: String,
    /// The tag exists on the remote at `commit`.
    pub tag_done: bool,
    /// Commands that finished successfully.
    pub done: usize,
    /// The step started and not yet finished (`tag` or `command N`).
    pub running: Option<String>,
    /// Why the job stopped. Held until the owner decides.
    pub failed: Option<String>,
    /// Everything ran.
    pub finished: bool,
    /// What each step that ran said, in order.
    pub log: Vec<StepLog>,
}

impl Job {
    /// A job that has not started.
    pub fn new(version: &str, pr_url: &str, commit: &str) -> Self {
        Self {
            version: version.to_owned(),
            pr_url: pr_url.to_owned(),
            commit: commit.to_owned(),
            ..Self::default()
        }
    }

    /// The tag name, `vX.Y.Z`.
    pub fn tag(&self) -> String {
        format!("v{}", self.version)
    }

    /// Stopped in the middle of a step with nothing recorded about how it
    /// ended: the outcome is unknown, so it is not retried on its own.
    pub fn interrupted(&self) -> bool {
        self.running.is_some() && self.failed.is_none() && !self.finished
    }

    /// Owner chose to retry: forget the stop and resume from the progress.
    pub fn resume(&mut self) {
        self.failed = None;
        self.running = None;
    }
}

/// `1.2.3` out of `chore/release-v1.2.3`.
pub fn version_from_branch(branch: &str) -> Option<String> {
    let v = branch.strip_prefix(BRANCH_PREFIX)?;
    (!v.is_empty()).then(|| v.to_owned())
}

/// What to do about the tag. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagStep {
    /// Neither the remote nor this checkout has it: create it and push.
    Create,
    /// A local tag already at the commit but not on the remote: push it.
    PushExisting,
    /// The remote already has it at the commit: nothing to do.
    Present,
    /// A tag of that name points elsewhere. Never touched.
    Foreign(String),
    /// The remote could not be read, so "does not exist" is not known.
    Unreadable(String),
}

/// Decide the tag step from what was read. `remote` is the commit the remote's
/// tag peels to (`Ok(None)`: absent), or the reason it could not be read;
/// `local` is the commit of a local tag of that name.
pub fn tag_step(
    tag: &str,
    commit: &str,
    remote: std::result::Result<Option<&str>, &str>,
    local: Option<&str>,
) -> TagStep {
    match remote {
        Err(e) => TagStep::Unreadable(format!("could not read the remote's {tag}: {e}")),
        Ok(Some(r)) if r == commit => TagStep::Present,
        Ok(Some(r)) => TagStep::Foreign(format!(
            "{tag} already exists on the remote at {r}, not at the merge commit {commit}"
        )),
        Ok(None) => match local {
            Some(l) if l == commit => TagStep::PushExisting,
            Some(l) => TagStep::Foreign(format!(
                "{tag} already exists locally at {l}, not at the merge commit {commit}"
            )),
            None => TagStep::Create,
        },
    }
}

/// The commit `tag` peels to, out of `git ls-remote --tags <remote>
/// refs/tags/<tag> refs/tags/<tag>^{}`. The peeled line wins over the tag
/// object's own.
pub fn parse_ls_remote(out: &str, tag: &str) -> Option<String> {
    let direct = format!("refs/tags/{tag}");
    let peeled = format!("{direct}^{{}}");
    let mut found = None;
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let (Some(oid), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        if name == peeled {
            return Some(oid.to_owned());
        }
        if name == direct {
            found = Some(oid.to_owned());
        }
    }
    found
}

/// Last `max` bytes of `s`, on a character boundary.
pub fn tail(s: &str, max: usize) -> String {
    let s = s.trim_end();
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("...{}", &s[cut..])
}

/// Where a job keeps its worktree and full command output.
pub fn job_dir(home: &Path, key: &str) -> PathBuf {
    home.join("release-local").join(crate::notices::id_of(key))
}

fn env_of(job: &Job) -> Vec<(&'static str, String)> {
    vec![
        ("MAGI_RELEASE_VERSION", job.version.clone()),
        ("MAGI_RELEASE_TAG", job.tag()),
        ("MAGI_RELEASE_COMMIT", job.commit.clone()),
        ("MAGI_RELEASE_PR", job.pr_url.clone()),
    ]
}

/// Persists the job; `false` means it could not be, and nothing may run.
pub type Save<'a> = &'a mut (dyn FnMut(&Job) -> bool + Send);

/// Where and how a job runs.
pub struct Env<'a> {
    /// Primary checkout; only used to add and remove the detached worktree.
    pub repo: &'a Path,
    /// magi home, for the job's directory.
    pub home: &'a Path,
    /// Names the job's directory (the pull request key).
    pub key: &'a str,
    /// Remote the tag is pushed to (`[merge] remote`).
    pub remote: &'a str,
    /// [`crate::config::Config::shell`].
    pub shell: &'a [String],
    /// The `[release]` table.
    pub release: &'a Release,
}

/// Drive `job` to the end or to the first failure. `Err` carries the reason it
/// stopped; the job itself already says how far it got. Never retries.
pub async fn run_job(env: &Env<'_>, job: &mut Job, save: Save<'_>) -> Result<()> {
    let dir = job_dir(env.home, env.key);
    let wt = dir.join("wt");
    let result = drive(env, &dir, &wt, job, save).await;
    // A held job keeps its checkout: a resumed command (say `gh release
    // create`) needs what an earlier one built (`target/release/...`), and a
    // fresh checkout would skip the build and attach nothing. Only a finished
    // release cleans up.
    if job.finished {
        let _ = git::worktree_remove(env.repo, &wt).await;
    }
    result
}

async fn drive(env: &Env<'_>, dir: &Path, wt: &Path, job: &mut Job, save: Save<'_>) -> Result<()> {
    let (repo, remote) = (env.repo, env.remote);
    if job.commit.is_empty() {
        bail!("the merge commit of {} is unknown", job.pr_url);
    }
    // Resuming after progress reuses the checkout the finished steps ran in
    // (it is no longer "clean": their output is there, which is the point).
    let progressed = job.tag_done || job.done > 0;
    let reusable = progressed
        && wt.exists()
        && git::rev_parse(wt, "HEAD").await.ok().as_deref() == Some(job.commit.as_str());
    if !reusable {
        if wt.exists() {
            let _ = git::worktree_remove(repo, wt).await;
            let _ = std::fs::remove_dir_all(wt);
        }
        // The merge commit exists on the remote; this checkout may not have it
        // yet. A failed fetch is only fatal if the commit is still missing.
        let fetched = git::git_raw(repo, &["fetch", "--quiet", remote]).await?;
        if !git::rev_exists(repo, &format!("{}^{{commit}}", job.commit)).await {
            bail!(
                "the merge commit {} is not available locally (git fetch {remote}: {})",
                job.commit,
                fetched.stderr
            );
        }
        git::worktree_add_detached(repo, wt, &job.commit)
            .await
            .context("check out the merge commit")?;
        let head = git::rev_parse(wt, "HEAD").await?;
        if head != job.commit {
            bail!(
                "the release checkout is at {head}, not the merge commit {}",
                job.commit
            );
        }
        if !git::is_clean(wt).await? {
            bail!("the release checkout is not clean");
        }
    }
    if let Ok(toml) = std::fs::read_to_string(wt.join("Cargo.toml")) {
        match crate::bump::current_version(&toml) {
            Ok(v) if v == job.version => {}
            Ok(v) => bail!(
                "Cargo.toml at the merge commit says {v}, but the release is {}; not tagging",
                job.version
            ),
            Err(e) => bail!("cannot read the version at the merge commit: {e:#}"),
        }
    }

    if !job.tag_done {
        job.running = Some("tag".to_owned());
        if !save(job) {
            bail!("could not record progress; nothing was run");
        }
        let note = tag(wt, remote, job).await?;
        job.log.push(StepLog {
            name: "tag".to_owned(),
            code: Some(0),
            tail: note,
            output: None,
        });
        job.tag_done = true;
        job.running = None;
        if !save(job) {
            bail!("the tag was pushed but progress could not be recorded");
        }
    }

    let release = env.release;
    let timeout = Duration::from_secs(release.timeout_minutes.max(1) * 60);
    while job.done < release.commands.len() {
        let n = job.done;
        let command = &release.commands[n];
        job.running = Some(format!("command {}", n + 1));
        if !save(job) {
            bail!("could not record progress; nothing was run");
        }
        let (code, output) = run_command(wt, env.shell, command, job, timeout).await;
        let _ = std::fs::create_dir_all(dir);
        let out_path = dir.join(format!("release-{}.out", n + 1));
        let kept = std::fs::write(&out_path, &output).is_ok();
        job.log.push(StepLog {
            name: command.clone(),
            code,
            tail: tail(&output, TAIL),
            output: kept.then(|| out_path.display().to_string()),
        });
        if code != Some(0) {
            let why = match code {
                Some(c) => format!("command {} `{command}` exited {c}", n + 1),
                None => format!(
                    "command {} `{command}` did not finish (timeout is {} minute(s))",
                    n + 1,
                    release.timeout_minutes
                ),
            };
            // `failed` is saved together with the cleared step: a stop between
            // the two must never read as an unmarked, retryable job.
            job.failed = Some(why.clone());
            job.running = None;
            let _ = save(job);
            bail!("{why}");
        }
        job.running = None;
        job.done += 1;
        if !save(job) {
            bail!(
                "command {} succeeded but progress could not be recorded",
                n + 1
            );
        }
    }
    job.finished = true;
    let _ = save(job);
    Ok(())
}

/// Read the tag's state and act on [`tag_step`]. Returns a note for the log.
async fn tag(wt: &Path, remote: &str, job: &Job) -> Result<String> {
    let name = job.tag();
    let listed = git::git_raw(
        wt,
        &[
            "ls-remote",
            "--tags",
            remote,
            &format!("refs/tags/{name}"),
            &format!("refs/tags/{name}^{{}}"),
        ],
    )
    .await?;
    let remote_tag = if listed.ok() {
        Ok(parse_ls_remote(&listed.stdout, &name))
    } else {
        Err(listed.stderr.clone())
    };
    let local = git::git_raw(
        wt,
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/tags/{name}^{{commit}}"),
        ],
    )
    .await?;
    let local = local.ok().then(|| local.stdout.clone());
    let step = tag_step(
        &name,
        &job.commit,
        remote_tag
            .as_ref()
            .map(|o| o.as_deref())
            .map_err(String::as_str),
        local.as_deref(),
    );
    match step {
        TagStep::Present => Ok(format!("{name} already on the remote at the merge commit")),
        TagStep::Foreign(why) | TagStep::Unreadable(why) => bail!("{why}"),
        TagStep::Create | TagStep::PushExisting => {
            if step == TagStep::Create {
                // An annotated tag needs a committer. Use the operator's own
                // identity; only when git has none (a bare service account) fall
                // back to a neutral one rather than failing the release.
                let has_ident = git::git_raw(wt, &["var", "GIT_COMMITTER_IDENT"])
                    .await?
                    .ok();
                let mut args = vec![];
                if !has_ident {
                    args.extend(["-c", "user.name=magi", "-c", "user.email=magi@localhost"]);
                }
                args.extend(["tag", "-a", name.as_str(), "-m", name.as_str(), &job.commit]);
                let out = git::git_raw(wt, &args).await?;
                if !out.ok() {
                    bail!("git tag {name}: {}", out.stderr);
                }
            }
            // No `+`, no `--force`: an existing remote tag is refused by git.
            let refspec = format!("refs/tags/{name}:refs/tags/{name}");
            let out = git::git_raw(wt, &["push", remote, &refspec]).await?;
            if !out.ok() {
                bail!("git push {remote} {refspec}: {}", out.stderr);
            }
            Ok(format!("pushed {name} at {}", job.commit))
        }
    }
}

/// Run one command in `wt`; `(None, ..)` when it could not run or timed out.
async fn run_command(
    wt: &Path,
    shell: &[String],
    command: &str,
    job: &Job,
    timeout: Duration,
) -> (Option<i32>, String) {
    let Some((prog, args)) = shell.split_first() else {
        return (None, "no shell is configured".to_owned());
    };
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args)
        .arg(command)
        .current_dir(wt)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    for (k, v) in env_of(job) {
        cmd.env(k, v);
    }
    cmd.quiet();
    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => (None, "timed out".to_owned()),
        Ok(Err(e)) => (None, format!("could not run: {e}")),
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.code(), text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: &str = "aaaa";

    #[test]
    fn the_tag_decision_never_touches_an_existing_tag() {
        let t = "v1.0.0";
        assert_eq!(tag_step(t, C, Ok(None), None), TagStep::Create);
        assert_eq!(tag_step(t, C, Ok(None), Some(C)), TagStep::PushExisting);
        assert_eq!(tag_step(t, C, Ok(Some(C)), None), TagStep::Present);
        assert_eq!(tag_step(t, C, Ok(Some(C)), Some("bbbb")), TagStep::Present);
        assert!(matches!(
            tag_step(t, C, Ok(Some("bbbb")), Some(C)),
            TagStep::Foreign(_)
        ));
        assert!(matches!(
            tag_step(t, C, Ok(None), Some("bbbb")),
            TagStep::Foreign(_)
        ));
        // Unreadable is not absent.
        assert!(matches!(
            tag_step(t, C, Err("boom"), None),
            TagStep::Unreadable(_)
        ));
    }

    #[test]
    fn ls_remote_prefers_the_peeled_commit() {
        let out = "1111\trefs/tags/v1.0.0\n2222\trefs/tags/v1.0.0^{}\n3333\trefs/tags/v1.0.0-rc\n";
        assert_eq!(parse_ls_remote(out, "v1.0.0").as_deref(), Some("2222"));
        assert_eq!(
            parse_ls_remote("1111\trefs/tags/v1.0.0\n", "v1.0.0").as_deref(),
            Some("1111")
        );
        assert_eq!(parse_ls_remote("", "v1.0.0"), None);
    }

    #[test]
    fn version_comes_from_the_release_branch() {
        assert_eq!(
            version_from_branch("chore/release-v1.2.3").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(version_from_branch("chore/release-v"), None);
        assert_eq!(version_from_branch("feat/x"), None);
    }

    #[test]
    fn a_started_and_unfinished_step_is_interrupted_not_retried() {
        let mut j = Job::new("1.0.0", "u", C);
        assert!(!j.interrupted());
        j.running = Some("command 1".to_owned());
        assert!(j.interrupted());
        j.failed = Some("x".to_owned());
        assert!(!j.interrupted());
        j.resume();
        assert!(!j.interrupted() && j.failed.is_none());
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("abcdef", 3), "...def");
        // `é` is two bytes: cutting inside it moves forward to the boundary.
        assert_eq!(tail("aéb", 2), "...b");
    }

    /// Run git in `dir`, panicking on failure.
    fn g(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .quiet()
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        remote: PathBuf,
        home: PathBuf,
        commit: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        let repo = dir.path().join("repo");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        g(&remote, &["init", "--bare", "-q"]);
        g(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-q", "-m", "init"]);
        g(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        g(&repo, &["push", "-q", "origin", "main"]);
        let commit = g(&repo, &["rev-parse", "HEAD"]);
        Fixture {
            _dir: dir,
            repo,
            remote,
            home,
            commit,
        }
    }

    fn release(commands: &[&str]) -> Release {
        Release {
            mode: crate::config::ReleaseMode::Local,
            commands: commands.iter().map(|s| (*s).to_owned()).collect(),
            timeout_minutes: 1,
        }
    }

    async fn go(f: &Fixture, rel: &Release, job: &mut Job) -> Result<()> {
        let shell = vec!["sh".to_owned(), "-c".to_owned()];
        let env = Env {
            repo: &f.repo,
            home: &f.home,
            key: "o/r#1",
            remote: "origin",
            shell: &shell,
            release: rel,
        };
        run_job(&env, job, &mut |_: &Job| true).await
    }

    #[tokio::test]
    async fn a_release_tags_once_runs_every_command_and_keeps_the_output() {
        let f = fixture();
        let mut job = Job::new("1.0.0", "https://github.com/o/r/pull/1", &f.commit);
        let rel = release(&["echo tag=$MAGI_RELEASE_TAG", "true"]);
        go(&f, &rel, &mut job).await.unwrap();
        assert!(job.finished && job.tag_done);
        assert_eq!(job.done, 2);
        assert!(job.log[1].tail.contains("tag=v1.0.0"), "{:?}", job.log);
        assert_eq!(
            g(&f.remote, &["rev-parse", "refs/tags/v1.0.0^{commit}"]),
            f.commit
        );
        let out = job_dir(&f.home, "o/r#1").join("release-1.out");
        assert!(std::fs::read_to_string(out).unwrap().contains("tag=v1.0.0"));
    }

    #[tokio::test]
    async fn the_first_failure_stops_the_list_and_a_resume_skips_what_succeeded() {
        let f = fixture();
        let mut job = Job::new("1.0.0", "u", &f.commit);
        let rel = release(&["true", "exit 3", "echo never"]);
        let err = go(&f, &rel, &mut job).await.unwrap_err().to_string();
        assert!(err.contains("exited 3"), "{err}");
        assert_eq!(job.done, 1);
        assert!(job.tag_done && !job.finished);
        assert!(!job.log.iter().any(|l| l.tail.contains("never")));

        // The owner fixed it and retried: the tag is not redone, the first
        // command is not repeated.
        job.resume();
        let rel = release(&["true", "true", "echo now"]);
        let tags_before = g(&f.remote, &["tag", "-l"]);
        go(&f, &rel, &mut job).await.unwrap();
        assert!(job.finished);
        assert_eq!(job.done, 3);
        assert_eq!(tags_before, g(&f.remote, &["tag", "-l"]));
        assert_eq!(job.log.iter().filter(|l| l.name == "true").count(), 2);
    }

    #[tokio::test]
    async fn a_failure_is_never_saved_as_a_plain_unmarked_job_and_the_checkout_survives() {
        let f = fixture();
        let mut job = Job::new("1.0.0", "u", &f.commit);
        let rel = release(&["touch built.txt", "exit 1"]);
        let shell = vec!["sh".to_owned(), "-c".to_owned()];
        let env = Env {
            repo: &f.repo,
            home: &f.home,
            key: "o/r#1",
            remote: "origin",
            shell: &shell,
            release: &rel,
        };
        let mut seen: Vec<Job> = Vec::new();
        run_job(&env, &mut job, &mut |j: &Job| {
            seen.push(j.clone());
            true
        })
        .await
        .unwrap_err();
        // Every saved state either is mid-step or carries the failure.
        assert!(
            seen.iter()
                .all(|j| j.running.is_some() || j.failed.is_some() || j.done > 0 || j.tag_done)
        );
        assert!(seen.last().unwrap().failed.is_some());
        // The first command's output is still there for the resumed one.
        let wt = job_dir(&f.home, "o/r#1").join("wt");
        assert!(wt.join("built.txt").exists());
        job.resume();
        let rel = release(&["touch built.txt", "test -f built.txt"]);
        go(&f, &rel, &mut job).await.unwrap();
        assert!(job.finished);
        assert!(!wt.exists(), "a finished release removes its checkout");
    }

    #[tokio::test]
    async fn an_existing_tag_elsewhere_is_never_pushed_over_and_runs_nothing() {
        let f = fixture();
        // A foreign v1.0.0 on the remote, at a different commit.
        std::fs::write(f.repo.join("x"), "x").unwrap();
        g(&f.repo, &["add", "."]);
        g(&f.repo, &["commit", "-q", "-m", "other"]);
        let other = g(&f.repo, &["rev-parse", "HEAD"]);
        g(&f.repo, &["tag", "v1.0.0", &other]);
        g(&f.repo, &["push", "-q", "origin", "refs/tags/v1.0.0"]);

        let mut job = Job::new("1.0.0", "u", &f.commit);
        let rel = release(&["echo ran > ran.txt"]);
        let err = go(&f, &rel, &mut job).await.unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(!job.tag_done && job.done == 0);
        assert_eq!(
            g(&f.remote, &["rev-parse", "refs/tags/v1.0.0^{commit}"]),
            other
        );
    }

    #[tokio::test]
    async fn a_remote_tag_at_the_merge_commit_is_not_pushed_again() {
        let f = fixture();
        g(&f.repo, &["tag", "v1.0.0", &f.commit]);
        g(&f.repo, &["push", "-q", "origin", "refs/tags/v1.0.0"]);
        let mut job = Job::new("1.0.0", "u", &f.commit);
        go(&f, &release(&["true"]), &mut job).await.unwrap();
        assert!(job.finished);
        assert!(job.log[0].tail.contains("already on the remote"));
    }

    #[tokio::test]
    async fn a_version_that_disagrees_with_the_merge_commit_is_not_tagged() {
        let f = fixture();
        let mut job = Job::new("2.0.0", "u", &f.commit);
        let err = go(&f, &release(&[]), &mut job)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not tagging"), "{err}");
        assert_eq!(g(&f.remote, &["tag", "-l"]), "");
    }
}
