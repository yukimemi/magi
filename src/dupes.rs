//! Duplicate-work detection at the moment work is filed.
//!
//! Two tasks once chased the same commit without knowing about each other: an
//! open task and its open pull request already covered a change when a chat
//! filed a second task that named the same branch and commit. Nothing at
//! filing time said so. This module is that check, and it is deliberately
//! small: **concrete identifiers only** - a branch name, a commit SHA, a pull
//! request number - never text similarity. A false positive costs one
//! `--force`; a false negative is the behaviour before this module existed.
//!
//! A *claim* is a thing an unfinished piece of work owns: the branches of its
//! candidates and the pull request it opened. The claims come from
//!
//! - every task that is not `done`: its `review_branch`, and each run in
//!   `Task::runs`;
//! - every run that has not reached a terminal status;
//! - every run that is terminal but whose pull request is still open (the
//!   shape of the original accident: the run ended, the PR did not).
//!
//! Run records are read through a tolerant view rather than `RunState::load`:
//! a schema bump must not turn into a silent false negative (the same lesson
//! as `clean::fold_due`).
//!
//! Everything here is synchronous, read-only and takes its roots as
//! arguments, so tests need no process-global home.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::proc::Quiet as _;
use crate::queue::{Queue, Task, TaskStatus};
use crate::run::RunStatus;

/// What kind of identifier matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A branch name.
    Branch,
    /// A commit SHA.
    Sha,
    /// A pull request number or URL.
    Pr,
}

/// Who owns the thing that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A queued task.
    Task,
    /// A recorded run.
    Run,
    /// A pull request the forge reports open that no record here owns.
    Pr,
}

/// One reason a new piece of work looks like one already under way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Task or run.
    pub owner: Owner,
    /// Full id of the task or run.
    pub id: String,
    /// The owner's status word (`queued`, `reviewing`, ...).
    pub status: String,
    /// What kind of identifier matched.
    pub signal: Signal,
    /// The identifier as it matched: a branch, a SHA, `#48`.
    pub token: String,
    /// How the owner is tied to it, e.g. `produced by its run c9eb`.
    pub via: String,
}

impl fmt::Display for Hit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.owner {
            Owner::Task => "task",
            Owner::Run => "run",
            Owner::Pr => "pull request",
        };
        let what = match self.signal {
            Signal::Branch => "names branch",
            Signal::Sha => "names commit",
            Signal::Pr => "names pull request",
        };
        write!(
            f,
            "{kind} {} ({}): this work {what} {}, {}",
            crate::queue::short(&self.id),
            self.status,
            self.token,
            self.via
        )
    }
}

/// The refusal: one or more [`Hit`]s and nothing filed.
#[derive(Debug, Clone)]
pub struct Duplicate(pub Vec<Hit>);

impl Duplicate {
    /// The refusal text, ending in how to override it.
    pub fn render(&self, override_hint: &str) -> String {
        let mut out = String::from("this looks like work that is already in flight:");
        for h in &self.0 {
            out.push_str("\n  - ");
            out.push_str(&h.to_string());
        }
        out.push('\n');
        out.push_str(override_hint);
        out
    }
}

impl fmt::Display for Duplicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(
            "If it is not a duplicate, pass --force to file it anyway \
             (an agent should report this to the operator instead).",
        ))
    }
}

impl std::error::Error for Duplicate {}

/// The slice of `run.json` this module needs. Every field is optional so a
/// record from any schema still yields what it has.
#[derive(Debug, Default, Deserialize)]
struct RunView {
    #[serde(default)]
    id: String,
    #[serde(default)]
    repo: PathBuf,
    #[serde(default)]
    status: String,
    #[serde(default)]
    base_commit: String,
    #[serde(default)]
    candidates: Vec<CandView>,
    #[serde(default)]
    pr: Option<PrView>,
}

#[derive(Debug, Default, Deserialize)]
struct CandView {
    #[serde(default)]
    branch: String,
}

#[derive(Debug, Default, Deserialize)]
struct PrView {
    #[serde(default)]
    url: String,
    #[serde(default)]
    number: u64,
    #[serde(default)]
    state: String,
}

impl RunView {
    fn read(runs_root: &Path, id: &str) -> Option<Self> {
        let raw = std::fs::read_to_string(runs_root.join(id).join("run.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// An unknown status word counts as unfinished: claiming too much is the
    /// cheap error here.
    fn terminal(&self) -> bool {
        serde_json::from_value::<RunStatus>(serde_json::Value::String(self.status.clone()))
            .map(RunStatus::done)
            .unwrap_or(false)
    }

    fn pr_open(&self) -> bool {
        self.pr.as_ref().is_some_and(|p| p.state == "open")
    }
}

/// Something an unfinished piece of work owns.
#[derive(Debug, Clone)]
struct Claim {
    owner: Owner,
    id: String,
    status: String,
    via: String,
    branch: Option<String>,
    /// Commit the branch forked from; without it a SHA cannot be judged.
    base: Option<String>,
    /// `(number, url)` of an open pull request.
    pr: Option<(u64, String)>,
}

/// Look for work already in flight that `text` (and `review_branch`, for a
/// review-only run) points at. `repo` is the repository the new work is for;
/// `ignore_task` is a task being edited, whose own claims never count.
pub fn check(
    queue: &Queue,
    runs_root: &Path,
    repo: &Path,
    text: &str,
    review_branch: Option<&str>,
    ignore_task: Option<&str>,
) -> Vec<Hit> {
    check_with(
        queue,
        runs_root,
        repo,
        text,
        review_branch,
        ignore_task,
        &gh_open_pr,
    )
}

/// [`check`] with the forge lookup supplied: `open_pr(repo, n)` says whether
/// pull request `n` of `repo` is open (`Some(url)`) or not / unknown (`None`).
/// It is asked only about numbers the text names that no local record already
/// explained, so a PR magi never produced (opened by hand) still collides.
pub fn check_with(
    queue: &Queue,
    runs_root: &Path,
    repo: &Path,
    text: &str,
    review_branch: Option<&str>,
    ignore_task: Option<&str>,
    open_pr: &dyn Fn(&Path, u64) -> Option<String>,
) -> Vec<Hit> {
    let mut idents = Idents::default();
    let here = idents.of(repo);
    let tasks = queue.list();
    let own_runs: BTreeSet<String> = tasks
        .iter()
        .filter(|t| Some(t.id.as_str()) == ignore_task)
        .flat_map(|t| t.runs.iter().cloned())
        .collect();
    // PRs the edited task's own runs opened: never a rival, forge or not.
    let own_prs: BTreeSet<u64> = own_runs
        .iter()
        .filter_map(|id| RunView::read(runs_root, id))
        .filter_map(|v| v.pr.map(|p| p.number))
        .collect();

    let mut claims: Vec<Claim> = Vec::new();
    let mut from_task: BTreeSet<String> = BTreeSet::new();
    for t in tasks
        .iter()
        .filter(|t| t.status != TaskStatus::Done && Some(t.id.as_str()) != ignore_task)
        .filter(|t| idents.of(&t.repo) == here)
    {
        claims.extend(task_claims(t, runs_root, &mut from_task));
    }
    for id in crate::run::list_ids_in(runs_root) {
        if own_runs.contains(&id) {
            continue;
        }
        let Some(view) = RunView::read(runs_root, &id) else {
            continue;
        };
        if (view.terminal() && !view.pr_open()) || idents.of(&view.repo) != here {
            continue;
        }
        claims.extend(run_claims(&view, Owner::Run, None, "its own run"));
    }

    let mut hits: Vec<Hit> = Vec::new();
    let mut push = |c: &Claim, signal: Signal, token: String| {
        let hit = Hit {
            owner: c.owner.clone(),
            id: c.id.clone(),
            status: c.status.clone(),
            signal,
            token,
            via: c.via.clone(),
        };
        if !hits.contains(&hit) {
            hits.push(hit);
        }
    };

    let prs = pr_numbers(text);
    let shas = sha_candidates(repo, text);
    for c in &claims {
        if let Some(b) = &c.branch {
            if names_branch(text, b) || review_branch == Some(b.as_str()) {
                push(c, Signal::Branch, b.clone());
            }
            if let Some(base) = &c.base {
                for sha in &shas {
                    if on_branch_only(repo, sha, b, base) {
                        push(c, Signal::Sha, short_sha(sha));
                    }
                }
            }
        }
        if let Some((n, url)) = &c.pr {
            if prs.contains(&Mention::Number(*n))
                || prs.iter().any(|p| matches!(p, Mention::Url(u) if u == url))
            {
                push(c, Signal::Pr, format!("#{n}"));
            }
        }
    }
    let mut asked = BTreeSet::new();
    for n in prs.iter().filter_map(|m| match m {
        Mention::Number(n) => Some(*n),
        Mention::Url(u) => u.rsplit('/').next().and_then(|d| d.parse().ok()),
    }) {
        let token = format!("#{n}");
        if hits
            .iter()
            .any(|h| h.signal == Signal::Pr && h.token == token)
            || own_prs.contains(&n)
            || !asked.insert(n)
        {
            continue;
        }
        if let Some(url) = open_pr(repo, n) {
            hits.push(Hit {
                owner: Owner::Pr,
                id: token.clone(),
                status: "open".into(),
                signal: Signal::Pr,
                token,
                via: format!("an open pull request with no run record here ({url})"),
            });
        }
    }
    hits
}

/// Ask the forge, via `gh`, whether PR `n` is open. Best effort and bounded:
/// any failure (no `gh`, no remote, offline, a slow answer) is `None`, i.e.
/// today's behaviour. `GH_REPO` is dropped so the PR is looked up in `repo`'s
/// own remote, not whatever the environment points at.
fn gh_open_pr(repo: &Path, n: u64) -> Option<String> {
    let mut child = Command::new("gh")
        .quiet()
        .args(["pr", "view", &n.to_string(), "--json", "state,url"])
        .current_dir(repo)
        .env_remove("GH_REPO")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut raw).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    (v["state"] == "OPEN")
        .then(|| v["url"].as_str().map(str::to_owned))
        .flatten()
}

fn task_claims(t: &Task, runs_root: &Path, seen: &mut BTreeSet<String>) -> Vec<Claim> {
    let status = t.status.as_str().to_owned();
    let mut out = Vec::new();
    if let Some(b) = &t.review_branch {
        out.push(Claim {
            owner: Owner::Task,
            id: t.id.clone(),
            status: status.clone(),
            via: "its review branch".into(),
            branch: Some(b.clone()),
            base: None,
            pr: None,
        });
    }
    for rid in &t.runs {
        if let Some(view) = RunView::read(runs_root, rid) {
            seen.insert(rid.clone());
            // A merged or closed pull request is not in flight, even when the
            // task that owns it is.
            out.extend(run_claims(
                &view,
                Owner::Task,
                Some((&t.id, &status)),
                &format!("produced by its run {}", crate::queue::short(rid)),
            ));
        }
    }
    out
}

/// The claims of one run, attributed to `owner` (the task, when there is one).
fn run_claims(view: &RunView, owner: Owner, task: Option<(&str, &str)>, via: &str) -> Vec<Claim> {
    let (id, status) = match task {
        Some((id, status)) => (id.to_owned(), status.to_owned()),
        None => (view.id.clone(), view.status.clone()),
    };
    let pr = view
        .pr
        .as_ref()
        .filter(|p| p.state == "open" && p.number > 0)
        .map(|p| (p.number, p.url.clone()));
    let via_pr = |extra: &str| match &pr {
        Some((n, _)) => format!("{via} (PR #{n} open){extra}"),
        None => format!("{via}{extra}"),
    };
    let mut out: Vec<Claim> = view
        .candidates
        .iter()
        .filter(|c| !c.branch.is_empty())
        .map(|c| Claim {
            owner: owner.clone(),
            id: id.clone(),
            status: status.clone(),
            via: via_pr(""),
            branch: Some(c.branch.clone()),
            base: (!view.base_commit.is_empty()).then(|| view.base_commit.clone()),
            pr: None,
        })
        .collect();
    if pr.is_some() {
        out.push(Claim {
            owner,
            id,
            status,
            via: via_pr(""),
            branch: None,
            base: None,
            pr,
        });
    }
    out
}

/// Repository identity: the canonical git common dir, so two worktrees of one
/// repository are the same repository and a path spelled two ways is too.
#[derive(Default)]
struct Idents(HashMap<PathBuf, PathBuf>);

impl Idents {
    fn of(&mut self, path: &Path) -> PathBuf {
        self.0
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                git(
                    path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )
                .map(PathBuf::from)
                .and_then(|p| p.canonicalize().ok())
                .or_else(|| path.canonicalize().ok())
                .unwrap_or_else(|| path.to_path_buf())
            })
            .clone()
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .quiet()
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    git(cwd, args).is_some()
}

fn is_ref_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-')
}

/// Does `text` contain `branch` as a whole word? `.` and `/` before it are
/// fine (`origin/magi/x/A`), a name character on either side is not, so
/// `magi/27b2/A` does not match inside `magi/27b2/AB`.
fn names_branch(text: &str, branch: &str) -> bool {
    if branch.len() < 3 {
        return false;
    }
    text.match_indices(branch).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + branch.len()..].chars().next();
        let after_ok = match after {
            None => true,
            Some('/') => false,
            Some('.') => !text[i + branch.len() + 1..]
                .chars()
                .next()
                .is_some_and(is_ref_char),
            Some(c) => !is_ref_char(c),
        };
        before.is_none_or(|c| !is_ref_char(c) && c != '.') && after_ok
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Mention {
    Number(u64),
    Url(String),
}

/// `#48`, `PR 48`, `PR #48`, `pull request 48`, and `.../pull/48` URLs.
fn pr_numbers(text: &str) -> Vec<Mention> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let digits = |from: usize| -> Option<(u64, usize)> {
        let n = text[from..].bytes().take_while(u8::is_ascii_digit).count();
        (n > 0 && n < 10)
            .then(|| text[from..from + n].parse().ok().map(|v| (v, from + n)))
            .flatten()
    };
    for (i, _) in text.match_indices('#') {
        if let Some((n, _)) = digits(i + 1) {
            let word_before = i > 0 && is_ref_char(bytes[i - 1] as char);
            if !word_before {
                out.push(Mention::Number(n));
            }
        }
    }
    let lower = text.to_ascii_lowercase();
    for key in ["pull request ", "pr "] {
        for (i, _) in lower.match_indices(key) {
            if i > 0 && is_ref_char(bytes[i - 1] as char) {
                continue;
            }
            let from = i + key.len();
            let from = if text[from..].starts_with('#') {
                from + 1
            } else {
                from
            };
            if let Some((n, _)) = digits(from) {
                out.push(Mention::Number(n));
            }
        }
    }
    for (i, _) in text.match_indices("/pull/") {
        if let Some((_, end)) = digits(i + 6) {
            let start = text[..i]
                .rfind(|c: char| c.is_whitespace() || matches!(c, '(' | '<' | '"' | '\''))
                .map_or(0, |p| p + 1);
            out.push(Mention::Url(text[start..end].to_owned()));
        }
    }
    out
}

/// Hex words of 7..=40 chars in `text` that resolve to a commit in `repo`.
fn sha_candidates(repo: &Path, text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if !(7..=40).contains(&word.len()) || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if seen.len() >= 16 || !seen.insert(word.to_ascii_lowercase()) {
            continue;
        }
        if let Some(full) = git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{word}^{{commit}}"),
            ],
        ) {
            out.push(full);
        }
    }
    out
}

/// Is `sha` reachable from `branch` but not from `base`? A SHA already in the
/// base would match every branch there is.
fn on_branch_only(repo: &Path, sha: &str, branch: &str, base: &str) -> bool {
    let tip = format!("{branch}^{{commit}}");
    git_ok(repo, &["rev-parse", "--verify", "--quiet", &tip])
        && git_ok(repo, &["merge-base", "--is-ancestor", sha, branch])
        && !git_ok(repo, &["merge-base", "--is-ancestor", sha, base])
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Source, Task};

    struct Fx {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        runs: PathBuf,
        q: Queue,
    }

    fn sh(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .quiet()
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A repo with `main` and a branch `magi/aaaa/A` one commit ahead.
    /// Returns the fixture plus the (main, branch) tip SHAs.
    fn fx() -> (Fx, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "1").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "base"]);
        let base = sh(&repo, &["rev-parse", "HEAD"]);
        sh(&repo, &["checkout", "-q", "-b", "magi/aaaa/A"]);
        std::fs::write(repo.join("a"), "2").unwrap();
        sh(&repo, &["commit", "-q", "-am", "work"]);
        let tip = sh(&repo, &["rev-parse", "HEAD"]);
        sh(&repo, &["checkout", "-q", "main"]);
        let runs = tmp.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let q = Queue::at(tmp.path().join("queue"));
        (
            Fx {
                _tmp: tmp,
                repo,
                runs,
                q,
            },
            base,
            tip,
        )
    }

    fn write_run(f: &Fx, id: &str, status: &str, base: &str, pr: Option<(u64, &str)>) {
        let dir = f.runs.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let pr = pr.map(|(n, s)| {
            serde_json::json!({"url": format!("https://github.com/o/r/pull/{n}"), "number": n, "state": s})
        });
        let v = serde_json::json!({
            "schema": 999, "id": id, "repo": f.repo, "status": status,
            "base_commit": base, "candidates": [{"branch": "magi/aaaa/A"}], "pr": pr,
        });
        std::fs::write(dir.join("run.json"), v.to_string()).unwrap();
    }

    fn file_task(f: &Fx, status: TaskStatus, runs: &[&str]) -> Task {
        let mut t = Task::new("t".into(), "x".into(), f.repo.clone(), Source::Human);
        t.status = status;
        t.runs = runs.iter().map(|s| (*s).to_owned()).collect();
        f.q.put(&mut t).unwrap();
        t
    }

    const RID: &str = "20260901-100000-aaaa";

    fn run(f: &Fx, text: &str, review: Option<&str>) -> Vec<Hit> {
        check_with(&f.q, &f.runs, &f.repo, text, review, None, &|_, _| None)
    }

    #[test]
    fn branch_matches_a_live_run_and_its_task() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let t = file_task(&f, TaskStatus::Running, &[RID]);
        let hits = run(&f, "land magi/aaaa/A onto a fresh branch.", None);
        assert!(hits.iter().any(|h| h.owner == Owner::Task
            && h.id == t.id
            && h.signal == Signal::Branch
            && h.token == "magi/aaaa/A"));
        assert!(hits.iter().any(|h| h.owner == Owner::Run && h.id == RID));
        let msg = Duplicate(hits).to_string();
        assert!(
            msg.contains("--force") && msg.contains("magi/aaaa/A"),
            "{msg}"
        );
    }

    #[test]
    fn branch_must_match_whole_word() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        assert!(run(&f, "see magi/aaaa/AB and magi/aaaa/A/x", None).is_empty());
    }

    #[test]
    fn sha_on_the_branch_matches_but_one_in_base_does_not() {
        let (f, base, tip) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let hits = run(&f, &format!("land commit {} please", &tip[..8]), None);
        assert!(
            hits.iter().any(|h| h.signal == Signal::Sha && h.id == RID),
            "{hits:?}"
        );
        assert!(run(&f, &format!("see {}", &base[..9]), None).is_empty());
        // Hex that resolves to nothing is ignored.
        assert!(run(&f, "deadbeef and 1234567", None).is_empty());
    }

    #[test]
    fn pr_number_matches_in_every_spelling() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        for text in [
            "finish #48",
            "PR 48 is stale",
            "pr #48",
            "pull request 48",
            "https://github.com/o/r/pull/48",
        ] {
            let hits = run(&f, text, None);
            assert!(
                hits.iter().any(|h| h.signal == Signal::Pr),
                "{text}: {hits:?}"
            );
        }
        assert!(run(&f, "see #480 and PR 4 and issue48", None).is_empty());
    }

    #[test]
    fn terminal_runs_and_done_tasks_do_not_match() {
        let (f, base, tip) = fx();
        write_run(&f, RID, "merged", &base, Some((48, "merged")));
        file_task(&f, TaskStatus::Done, &[RID]);
        let text = format!("magi/aaaa/A {} #48", &tip[..8]);
        assert!(run(&f, &text, None).is_empty());
    }

    #[test]
    fn terminal_run_with_open_pr_or_open_task_still_claims() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        assert!(!run(&f, "magi/aaaa/A", None).is_empty());
        let (g, base, _) = fx();
        write_run(&g, RID, "ready", &base, None);
        assert!(run(&g, "magi/aaaa/A", None).is_empty());
        let t = file_task(&g, TaskStatus::Held, &[RID]);
        let hits = run(&g, "magi/aaaa/A", None);
        assert!(hits.iter().any(|h| h.id == t.id), "{hits:?}");
    }

    #[test]
    fn review_only_matches_a_branch_a_live_task_owns() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, None);
        assert!(run(&f, "", Some("magi/aaaa/A")).is_empty());
        let mut t = file_task(&f, TaskStatus::Queued, &[]);
        t.review_branch = Some("magi/aaaa/A".into());
        f.q.put(&mut t).unwrap();
        let hits = run(&f, "", Some("magi/aaaa/A"));
        assert!(
            hits.iter()
                .any(|h| h.id == t.id && h.signal == Signal::Branch)
        );
    }

    #[test]
    fn other_repository_and_edited_task_do_not_match() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let t = file_task(&f, TaskStatus::Running, &[RID]);
        let other = f._tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["init", "-q"]);
        assert!(
            check_with(&f.q, &f.runs, &other, "magi/aaaa/A", None, None, &|_, _| {
                None
            })
            .is_empty()
        );
        // Editing the task that owns the run never collides with itself.
        assert!(
            check_with(
                &f.q,
                &f.runs,
                &f.repo,
                "magi/aaaa/A",
                None,
                Some(&t.id),
                &|_, _| None
            )
            .is_empty()
        );
    }

    #[test]
    fn a_worktree_is_the_same_repository() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let wt = f._tmp.path().join("wt");
        sh(
            &f.repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"],
        );
        assert!(
            !check_with(&f.q, &f.runs, &wt, "magi/aaaa/A", None, None, &|_, _| None).is_empty()
        );
    }

    #[test]
    fn an_open_pr_without_a_run_record_matches_through_the_forge() {
        let (f, _, _) = fx();
        let open = |_: &Path, n: u64| (n == 48).then(|| "https://example.test/pull/48".to_owned());
        let hit = |text: &str| check_with(&f.q, &f.runs, &f.repo, text, None, None, &open);
        let hits = hit("finish PR #48");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].owner, Owner::Pr);
        assert!(hits[0].to_string().contains("#48"));
        // Closed / unknown / unnamed PRs never match.
        assert!(hit("finish PR #49").is_empty());
        assert!(hit("finish the work").is_empty());
    }

    #[test]
    fn a_forge_hit_does_not_repeat_a_pr_a_run_already_explains() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        let open = |_: &Path, _: u64| Some("u".to_owned());
        let hits = check_with(&f.q, &f.runs, &f.repo, "#48", None, None, &open);
        assert!(hits.iter().all(|h| h.owner != Owner::Pr), "{hits:?}");
        assert!(!hits.is_empty());
    }

    #[test]
    fn an_edited_tasks_own_open_pr_is_not_a_forge_hit() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        let t = file_task(&f, TaskStatus::Running, &[RID]);
        let open = |_: &Path, _: u64| Some("u".to_owned());
        let hits = check_with(&f.q, &f.runs, &f.repo, "#48", None, Some(&t.id), &open);
        assert!(hits.is_empty(), "{hits:?}");
    }
}
