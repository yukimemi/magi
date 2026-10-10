//! Watches every open release-bump pull request until it lands.
//!
//! [`crate::bump`] opens a `chore/release-vX.Y.Z` pull request with auto-merge
//! armed and then stops looking. A required check that goes red (a flaky
//! windows or browser job) leaves auto-merge waiting forever, and nothing
//! pages anybody because the notices there only fire when the pull request
//! cannot be *opened*. This module is the part that keeps looking.
//!
//! It runs inside `magi serve` as its own task, like [`crate::waiter`], and is
//! tied to no run: a pending bump is coalesced across runs and outlives the
//! one that opened it. The forge is the source of truth (every open pull
//! request whose head branch starts with `chore/release-v`, in every checkout
//! magi knows); the pending marker is not consulted, so a missing one changes
//! nothing.
//!
//! - **The decision is pure.** [`decide`] maps a checks snapshot and the
//!   recorded [`WatchState`] to one [`Step`]. A forge answer that cannot be
//!   read is `Unknown`: nothing moves, not even the stall clock.
//! - **A failed run is rerun once, durably.** The run id is written to the
//!   state file *before* `gh run rerun <id> --failed` is called, so a restart
//!   never reruns twice; if the call then fails, the cost is one escalation to
//!   a human, never a rerun loop.
//! - **Escalation is a notice plus a question** ([`crate::bump::NOTICE_NODE`],
//!   no `cwd`, no run). Nothing is ever merged, closed or pushed: the watcher
//!   only reruns jobs. Silence is a hold. The owner's choice is applied by the
//!   watcher itself on its next lap, recorded in `WatchState::applied` so an
//!   answer is applied once.
//! - **`[release] mode = "local"` replaces all of the above for that
//!   repository's release pull requests.** No check is awaited, reran or
//!   stalled on. The watcher asks the owner (`merge` / `hold`, bound to the
//!   head it observed), merges with `--match-head-commit`, and after the merge
//!   drives [`crate::release_local`] - tag, then the configured commands - with
//!   its progress in the watch record. A failure is a notice and a question
//!   (`retry` / `leave it`), never a blind retry. See [`decide_local`].
//! - **Notice wording is fixed per stage** and carries only the pull request,
//!   so a repeated poll never relights it; everything that varies (job names,
//!   links) rides in the question.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::ask::{Answer, Question, Questions};
use crate::land::{self, PrLifecycle, RollupView, Verdict};
use crate::notices::{self, Notice, Notices};
use crate::release_local::{self, Job};
use crate::run::RunState;

/// Pause between laps.
const LAP: Duration = Duration::from_secs(300);

/// How long after a rerun the forge may still report the old failure before
/// that is read as "still red". `gh run rerun` returns before the checks flip
/// to pending.
const RERUN_GRACE: i64 = 180;

/// Choice: rerun the failed jobs once more. The three choices are matched
/// verbatim when applied.
pub const RERUN_AGAIN: &str = "rerun again";
/// Choice: stay quiet until a check or the head changes.
pub const HOLD: &str = "hold";
/// Choice: stop watching this pull request.
pub const LEAVE_IT: &str = "leave it";
/// Choice (local mode): run the failed release again from where it stopped.
pub const RETRY: &str = "retry";

/// What one look at one pull request concludes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Merged or closed: stop watching.
    Done,
    /// Nothing to do yet.
    Wait,
    /// The forge answer could not be read; change nothing.
    Unknown,
    /// Rerun the failed jobs of these workflow runs, once each.
    Rerun(Vec<String>),
    /// A human is needed.
    Escalate(Why),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// Still red after the one rerun, or red with nothing to rerun.
    StillRed,
    /// No progress for longer than `[daemon] release_stall_minutes`.
    Stalled,
}

/// Everything remembered about one watched pull request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WatchState {
    /// Checkout the pull request belongs to.
    pub repo: String,
    pub url: String,
    /// Head commit the rest of this record is about.
    pub head: String,
    /// Workflow runs already rerun for this head, with when (unix seconds).
    pub reruns: BTreeMap<String, i64>,
    /// Digest of the head and every check's verdict, and when it last changed.
    pub fingerprint: String,
    pub progress_at: i64,
    /// The open question asked about this pull request.
    pub question: Option<String>,
    /// Fingerprint the owner chose to hold: no new question until it changes.
    pub held: Option<String>,
    /// The owner said to leave it.
    pub ignored: bool,
    /// Question ids whose answer was already applied.
    pub applied: Vec<String>,
    /// Local mode: the head the open (or last) approval question was about.
    pub asked_head: Option<String>,
    /// Local mode: the head the owner held or whose merge was refused; no new
    /// approval question until the head moves.
    pub held_head: Option<String>,
    /// Local mode: the run that opened this release PR, so a failed release can
    /// hold the task that run finished (the daemon marks it Done at the merge).
    pub run: Option<String>,
    /// Local mode: the task this watcher put on hold for a failed release, so a
    /// successful retry can give it back (and a restart does not hold it twice).
    pub held_task: Option<String>,
    /// Local mode: the release after the merge.
    pub job: Option<Job>,
}

fn is_failed(v: Verdict) -> bool {
    v == Verdict::Fail
}

/// Digest of what "progress" means: the head and every check's verdict.
pub(crate) fn fingerprint(snap: &RollupView) -> String {
    let mut parts: Vec<String> = snap
        .checks
        .iter()
        .map(|c| format!("{}={:?}", c.name, c.verdict))
        .collect();
    parts.sort();
    format!("{}|{}", snap.head, parts.join(","))
}

/// Fold a fresh snapshot into the record: a new head forgets the old head's
/// reruns, and a changed fingerprint restarts the stall clock.
pub(crate) fn observe(st: &mut WatchState, snap: &RollupView, now: i64) {
    if st.head != snap.head {
        st.reruns.clear();
        st.head = snap.head.clone();
    }
    let fp = fingerprint(snap);
    if st.fingerprint != fp {
        st.fingerprint = fp;
        st.progress_at = now;
    }
}

/// Decide what to do about a pull request. `snap` is `None` when the forge
/// could not be read. `stall_secs == 0` disables the stall rule.
pub(crate) fn decide(
    snap: Option<&RollupView>,
    st: &WatchState,
    now: i64,
    stall_secs: i64,
) -> Step {
    let Some(snap) = snap else {
        return Step::Unknown;
    };
    if snap.state != PrLifecycle::Open {
        return Step::Done;
    }
    if st.ignored || st.held.as_deref() == Some(fingerprint(snap).as_str()) {
        return Step::Wait;
    }

    let any_pending = snap.checks.iter().any(|c| c.verdict == Verdict::Pending);
    // A run is complete once none of its own checks is still pending; another
    // run's pending job says nothing about it.
    let complete = |run: &str| {
        !snap
            .checks
            .iter()
            .any(|c| c.verdict == Verdict::Pending && c.run.as_deref() == Some(run))
    };
    let mut fresh = Vec::new();
    let mut spent_recently = false;
    let mut spent = false;
    let mut external = false;
    for c in snap.checks.iter().filter(|c| is_failed(c.verdict)) {
        match c.run.as_deref() {
            None => external = true,
            Some(run) if !complete(run) => {}
            Some(run) => match st.reruns.get(run) {
                None => {
                    if !fresh.iter().any(|r: &String| r == run) {
                        fresh.push(run.to_owned());
                    }
                }
                Some(&at) if now - at < RERUN_GRACE => spent_recently = true,
                Some(_) => spent = true,
            },
        }
    }
    if !fresh.is_empty() {
        return Step::Rerun(fresh);
    }
    if spent_recently {
        return Step::Wait;
    }
    if spent || (external && !any_pending) {
        return Step::Escalate(Why::StillRed);
    }
    if stall_secs > 0 && now - st.progress_at >= stall_secs {
        return Step::Escalate(Why::Stalled);
    }
    Step::Wait
}

/// `owner/repo#N` out of a pull request url, the stable name for notices.
pub(crate) fn pr_key(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let mut it = rest.split('/');
    let _host = it.next()?;
    let owner = it.next()?;
    let repo = it.next()?;
    if it.next()? != "pull" {
        return None;
    }
    let n: String = it
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!n.is_empty()).then(|| format!("{owner}/{repo}#{n}"))
}

fn notice_key(pr: &str) -> String {
    format!("release-pr:{pr}")
}

/// Fixed per stage: no counts, times or job names.
fn rerun_message(pr: &str) -> String {
    format!("Release PR {pr}: failed checks were rerun once")
}

fn human_message(pr: &str) -> String {
    format!("Release PR {pr} is stuck and needs a human")
}

fn question_detail(snap: &RollupView, why: Why, st: &WatchState) -> String {
    let mut s = String::new();
    s.push_str(&format!("Pull request: {}\n\n", snap.url));
    s.push_str(match why {
        Why::StillRed => "Checks are still red after the failed jobs were rerun once.\n\n",
        Why::Stalled => "The pull request has made no progress for too long.\n\n",
    });
    let failed: Vec<_> = snap
        .checks
        .iter()
        .filter(|c| is_failed(c.verdict))
        .collect();
    if failed.is_empty() {
        s.push_str("No check is failing.\n");
    } else {
        s.push_str("Failed jobs:\n");
        for c in failed {
            match &c.url {
                Some(u) => s.push_str(&format!("- {} ({u})\n", c.name)),
                None => s.push_str(&format!("- {}\n", c.name)),
            }
        }
    }
    s.push_str(&format!(
        "\nWorkflow runs rerun so far: {}.\n\n\
         - `{RERUN_AGAIN}`: rerun the failed jobs once more.\n\
         - `{HOLD}`: stay quiet until a check or the head changes.\n\
         - `{LEAVE_IT}`: stop watching this pull request.\n\n\
         Nothing is merged or pushed by magi; silence is a hold.\n",
        st.reruns.len()
    ));
    s
}

/// Where the owner's approval stands, as far as [`decide_local`] cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Approved {
    /// No question was filed (or it is gone).
    NoQuestion,
    /// Filed, not answered.
    Open,
    /// The owner said `merge`.
    Merge,
    /// The owner said anything else, or it was abandoned: silence is a hold.
    Hold,
}

/// What one look at an open local-mode release pull request concludes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalStep {
    /// Nothing to do now.
    Wait,
    /// Ask the owner whether to merge this head.
    Ask,
    /// Merge, pinned to this head.
    Merge(String),
    /// Remember the owner's no for this head and stay quiet until it moves.
    Hold(String),
}

/// Decide an *open* local-mode pull request. No I/O, and no CI: nothing is
/// awaited, rerun or stalled on, because nothing will ever report. The head is
/// what the owner approved: an answer about an older head asks again.
pub(crate) fn decide_local(head: &str, st: &WatchState, approved: Approved) -> LocalStep {
    if st.ignored || st.held_head.as_deref() == Some(head) {
        return LocalStep::Wait;
    }
    let about_this_head = st.asked_head.as_deref() == Some(head);
    match approved {
        Approved::Open => LocalStep::Wait,
        Approved::NoQuestion => LocalStep::Ask,
        Approved::Merge if about_this_head => LocalStep::Merge(head.to_owned()),
        Approved::Hold if about_this_head => LocalStep::Hold(head.to_owned()),
        // The head moved after the question was filed.
        Approved::Merge | Approved::Hold => LocalStep::Ask,
    }
}

/// Start watching a release pull request before the first lap sees it, so one
/// merged within a lap of being opened still gets released. Never overwrites.
pub(crate) fn register(home: &Path, repo: &Path, url: &str, run: &str) {
    let Some(pr) = pr_key(url) else {
        return;
    };
    let w = Watcher::new(Box::new(GhForge), home.to_path_buf());
    if w.state_path(&pr).exists() {
        return;
    }
    let st = WatchState {
        repo: repo.to_string_lossy().into_owned(),
        url: url.to_owned(),
        run: Some(run.to_owned()),
        ..WatchState::default()
    };
    w.save(&pr, &st);
}

/// The watch record whose open question is `question_id`, read from the magi
/// `home`. `None` when no record names it (never guessed from the text).
pub(crate) fn state_for_question(home: &Path, question_id: &str) -> Option<WatchState> {
    Watcher::new(Box::new(GhForge), home.to_path_buf())
        .stored()
        .into_iter()
        .find(|s| s.question.as_deref() == Some(question_id))
}

/// The task a release-watch question's answer could touch: the one that
/// finished the run which opened the release pull request (`WatchState::run`).
/// `None` when the record or the run is not known - then there is nothing to
/// check, and the settle goes on (an answer here never releases a task).
pub fn task_of_question(
    home: &Path,
    q: &Question,
    queue: &crate::queue::Queue,
) -> Option<crate::queue::Task> {
    let run = state_for_question(home, &q.id)?.run?;
    queue.list().into_iter().find(|t| t.runs.contains(&run))
}

/// What a deputy is told about a release-watch question: which pull request,
/// and what each offered choice really does - matched to [`Watcher::apply_answer`]
/// and [`Watcher::settle_job_question`]. A missing watch record is said to be
/// missing, never reconstructed.
pub(crate) fn deputy_brief(q: &Question, home: &Path) -> String {
    let mut s = String::from(
        "This question was filed by magi's release watcher about a release pull \
         request. Whatever the owner picks, only the watcher acts on it, on its \
         next lap; you apply nothing and never close, merge, rerun or push \
         anything. Silence is a hold.",
    );
    match state_for_question(home, &q.id) {
        Some(st) => {
            s.push_str(&format!(
                "\n\nPull request: {}\nCheckout: {}",
                st.url, st.repo
            ));
        }
        None => s.push_str(
            "\n\nThe watcher's record for this question could not be found, so the \
             pull request is known to you only through the question's own text. \
             Say so to the owner rather than guessing.",
        ),
    }
    s.push_str("\n\nWhat each option does:");
    for c in &q.choices {
        let what = match c.as_str() {
            RERUN_AGAIN => "forget the reruns already made and rerun the failed jobs once more",
            HOLD if q.choices.iter().any(|c| c == land::APPROVE) => {
                "leave the pull request open and unmerged; ask again only when the head moves"
            }
            HOLD => "stay quiet until a check or the head changes",
            LEAVE_IT => {
                "stop watching this pull request and dismiss its notice. It does NOT \
                 close the pull request"
            }
            RETRY => "run the failed release again from where it stopped",
            land::APPROVE => {
                "squash-merge exactly the head the question names - irreversible - \
                 then tag and release"
            }
            _ => "recorded as the answer",
        };
        s.push_str(&format!("\n- `{c}`: {what}"));
    }
    s.push_str(
        "\n\nIf the owner's words ask for the pull request itself to be closed, that \
         is something only they can do by hand; do not read it as a choice unless \
         it plainly means stop watching.",
    );
    s
}

/// Starts the hold reason of a task held for a failed release, so only that
/// hold is undone when the release later succeeds.
const HOLD_PREFIX: &str = "[release] ";

fn approval_message(pr: &str) -> String {
    format!("Release PR {pr} is waiting for your approval to merge")
}

fn failed_message(pr: &str) -> String {
    format!("Release PR {pr} merged but the release is on hold")
}

fn released_key(pr: &str) -> String {
    format!("release-done:{pr}")
}

type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Every forge access, so tests inject a fake instead of `gh`.
pub(crate) trait ReleaseForge: Send + Sync {
    /// `(branch, url)` of every open release-bump pull request in `repo`.
    fn list<'a>(&'a self, repo: &'a Path) -> Fut<'a, Result<Vec<(String, String)>>>;
    fn snapshot<'a>(&'a self, repo: &'a Path, url: &'a str) -> Fut<'a, Result<RollupView>>;
    fn rerun<'a>(&'a self, repo: &'a Path, run: &'a str) -> Fut<'a, Result<()>>;
    /// The repository's config, to read `[release]` and `[merge] remote`.
    fn config<'a>(&'a self, _repo: &'a Path) -> Fut<'a, Result<crate::config::Config>> {
        Box::pin(async { Ok(crate::config::Config::default()) })
    }
    /// Head branch and merge commit of a pull request (local mode).
    fn info<'a>(&'a self, _repo: &'a Path, url: &'a str) -> Fut<'a, Result<PrInfo>> {
        Box::pin(async move { bail!("no pull request info for {url}") })
    }
    /// Merge `url` (squash, delete branch) pinned to `head`. `Ok(true)` once
    /// the forge says it merged, `Ok(false)` when it did not.
    fn merge<'a>(&'a self, _repo: &'a Path, url: &'a str, _head: &'a str) -> Fut<'a, Result<bool>> {
        Box::pin(async move { bail!("cannot merge {url}") })
    }
}

/// What the forge says about a pull request's branch and merge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PrInfo {
    /// Head branch name, e.g. `chore/release-v1.2.3`.
    pub branch: String,
    /// The commit the pull request merged as, once merged.
    pub merge_commit: Option<String>,
    /// Pull request title.
    pub title: String,
}

/// Parse `gh pr view --json headRefName,mergeCommit,title`. No I/O.
pub(crate) fn parse_info(json: &str) -> Result<PrInfo> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let text = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    Ok(PrInfo {
        branch: text("headRefName"),
        title: text("title"),
        merge_commit: v
            .get("mergeCommit")
            .and_then(|m| m.get("oid"))
            .and_then(|o| o.as_str())
            .filter(|o| !o.is_empty())
            .map(str::to_owned),
    })
}

/// The real forge: `gh`.
pub(crate) struct GhForge;

impl ReleaseForge for GhForge {
    fn list<'a>(&'a self, repo: &'a Path) -> Fut<'a, Result<Vec<(String, String)>>> {
        Box::pin(crate::bump::list_open_release_prs(repo))
    }

    fn snapshot<'a>(&'a self, repo: &'a Path, url: &'a str) -> Fut<'a, Result<RollupView>> {
        Box::pin(async move {
            let args = [
                "pr".to_owned(),
                "view".to_owned(),
                url.to_owned(),
                "--json".to_owned(),
                "url,number,state,headRefOid,statusCheckRollup".to_owned(),
            ];
            let (ok, out) = land::gh(repo, &args).await?;
            if !ok {
                bail!("gh pr view {url}: {out}");
            }
            land::parse_rollup(&out)
        })
    }

    fn rerun<'a>(&'a self, repo: &'a Path, run: &'a str) -> Fut<'a, Result<()>> {
        Box::pin(async move {
            let args = [
                "run".to_owned(),
                "rerun".to_owned(),
                run.to_owned(),
                "--failed".to_owned(),
            ];
            let (ok, out) = land::gh(repo, &args).await?;
            if !ok {
                bail!("gh run rerun {run}: {out}");
            }
            Ok(())
        })
    }

    fn config<'a>(&'a self, repo: &'a Path) -> Fut<'a, Result<crate::config::Config>> {
        Box::pin(async move { Ok(crate::config::Config::discover(repo, None)?.0) })
    }

    fn info<'a>(&'a self, repo: &'a Path, url: &'a str) -> Fut<'a, Result<PrInfo>> {
        Box::pin(async move {
            let args = [
                "pr".to_owned(),
                "view".to_owned(),
                url.to_owned(),
                "--json".to_owned(),
                "headRefName,mergeCommit,title".to_owned(),
            ];
            let (ok, out) = land::gh(repo, &args).await?;
            if !ok {
                bail!("gh pr view {url}: {out}");
            }
            parse_info(&out)
        })
    }

    fn merge<'a>(&'a self, repo: &'a Path, url: &'a str, head: &'a str) -> Fut<'a, Result<bool>> {
        Box::pin(async move {
            let title = self
                .info(repo, url)
                .await
                .map(|i| i.title)
                .unwrap_or_default();
            let title = if title.trim().is_empty() {
                "chore: release".to_owned()
            } else {
                title
            };
            // Same shape as the direct merge in `bump`, pinned to the head the
            // owner approved: a push in between is refused by the forge.
            let mut argv = crate::bump::bump_merge_argv(url, &title);
            argv.push("--match-head-commit".to_owned());
            argv.push(head.to_owned());
            let (ok, out) = land::gh(repo, &argv).await?;
            if ok {
                return Ok(true);
            }
            // jj keeps HEAD detached, so `--delete-branch` exits non-zero after
            // the merge happened: the forge decides.
            let after = land::lifecycle(repo, url).await.ok();
            if land::merged_after_all(&argv, &out, after).is_some() {
                return Ok(true);
            }
            bail!("gh pr merge {url}: {out}")
        })
    }
}

/// The watcher: a forge, a magi home, and nothing else.
pub(crate) struct Watcher {
    forge: Box<dyn ReleaseForge>,
    home: PathBuf,
}

impl Watcher {
    pub(crate) fn new(forge: Box<dyn ReleaseForge>, home: PathBuf) -> Self {
        Self { forge, home }
    }

    fn dir(&self) -> PathBuf {
        self.home.join("release-watch")
    }

    fn state_path(&self, pr: &str) -> PathBuf {
        self.dir().join(format!("{}.json", notices::id_of(pr)))
    }

    fn load(&self, pr: &str) -> WatchState {
        std::fs::read_to_string(self.state_path(pr))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, pr: &str, st: &WatchState) -> bool {
        let write = || -> Result<()> {
            std::fs::create_dir_all(self.dir())?;
            let path = self.state_path(pr);
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(st)?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        };
        match write() {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("could not save the release watch for {pr}: {e:#}");
                false
            }
        }
    }

    /// Stored records, for pull requests that left the open list.
    fn stored(&self) -> Vec<WatchState> {
        let Ok(rd) = std::fs::read_dir(self.dir()) else {
            return Vec::new();
        };
        rd.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str(&s).ok())
            .collect()
    }

    fn questions(&self) -> Questions {
        Questions::at(self.home.join("questions"))
    }

    fn raise(&self, pr: &str, message: String) {
        notices::raise_in(&self.home, Notice::warn(&notice_key(pr), message));
    }

    /// One pass over every repo. `stall_secs` is `[daemon] release_stall_minutes`
    /// in seconds. Best-effort throughout.
    pub(crate) async fn lap(
        &self,
        repos: &[PathBuf],
        stall_secs: i64,
        now: i64,
        halt: &(dyn Fn() -> bool + Sync),
    ) {
        let stored = self.stored();
        // A repo with a watch record is covered even when nothing else names it.
        let mut repos = repos.to_vec();
        for s in &stored {
            let p = PathBuf::from(&s.repo);
            if !s.repo.is_empty() && !repos.contains(&p) {
                repos.push(p);
            }
        }
        for repo in &repos {
            if halt() {
                return;
            }
            let mut urls: Vec<String> = match self.forge.list(repo).await {
                Ok(v) => v.into_iter().map(|(_, u)| u).collect(),
                Err(e) => {
                    tracing::warn!(
                        "could not list release pull requests in {}: {e:#}",
                        repo.display()
                    );
                    Vec::new()
                }
            };
            // A pull request that left the open list is kept until its end is
            // confirmed, never assumed.
            let here = repo.to_string_lossy();
            for s in stored.iter().filter(|s| s.repo == here.as_ref()) {
                if !urls.contains(&s.url) {
                    urls.push(s.url.clone());
                }
            }
            for url in urls {
                if halt() {
                    return;
                }
                self.watch(repo, &url, stall_secs, now, halt).await;
            }
        }
    }

    /// Look at one pull request and act once.
    pub(crate) async fn watch(
        &self,
        repo: &Path,
        url: &str,
        stall_secs: i64,
        now: i64,
        halt: &(dyn Fn() -> bool + Sync),
    ) {
        let Some(pr) = pr_key(url) else {
            tracing::warn!("not a pull request url: {url}");
            return;
        };
        let mut st = self.load(&pr);
        st.repo = repo.to_string_lossy().into_owned();
        st.url = url.to_owned();

        let snap = match self.forge.snapshot(repo, url).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("could not read {url}: {e:#}");
                None
            }
        };
        match self.forge.config(repo).await {
            Ok(cfg) if cfg.release.is_local() => {
                self.watch_local(repo, &pr, st, snap, &cfg).await;
                return;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                "could not read the config of {}: {e:#}; watching {url} as an Actions release",
                repo.display()
            ),
        }
        if decide(snap.as_ref(), &st, now, stall_secs) == Step::Done {
            self.finish(&pr, &st);
            return;
        }
        let Some(snap) = snap else {
            return;
        };

        // The owner's word comes first.
        if let Some(id) = st.question.clone() {
            match self.questions().get(&id) {
                Ok(q) if q.status.open() => return,
                Ok(q) => self.apply_answer(&pr, &mut st, &snap, &q),
                Err(_) => st.question = None,
            }
            // Whatever was decided, the record is saved before anything else.
            observe(&mut st, &snap, now);
            self.save(&pr, &st);
        } else {
            observe(&mut st, &snap, now);
        }

        match decide(Some(&snap), &st, now, stall_secs) {
            Step::Done | Step::Unknown | Step::Wait => {}
            Step::Rerun(runs) => {
                // Durable before the call: at most once per failing run.
                for r in &runs {
                    st.reruns.insert(r.clone(), now);
                }
                // Without a durable record the call must not happen: a restart
                // would rerun the same run again.
                if !self.save(&pr, &st) || halt() {
                    return;
                }
                let mut ok = false;
                for r in &runs {
                    match self.forge.rerun(repo, r).await {
                        Ok(()) => ok = true,
                        Err(e) => tracing::warn!("could not rerun run {r} of {url}: {e:#}"),
                    }
                }
                if ok {
                    self.raise(&pr, rerun_message(&pr));
                }
            }
            Step::Escalate(why) => {
                self.raise(&pr, human_message(&pr));
                let mut q = Question::new(
                    String::new(),
                    crate::bump::NOTICE_NODE.to_owned(),
                    "release-watch".to_owned(),
                    format!("Release PR {pr} is stuck: what now?"),
                    question_detail(&snap, why, &st),
                    vec![RERUN_AGAIN.to_owned(), HOLD.to_owned(), LEAVE_IT.to_owned()],
                );
                match self.questions().put(&mut q) {
                    Ok(()) => st.question = Some(q.id.clone()),
                    Err(e) => tracing::warn!("could not file the question for {pr}: {e:#}"),
                }
            }
        }
        self.save(&pr, &st);
    }

    /// The local-mode path of [`Watcher::watch`]: approval, merge, release.
    async fn watch_local(
        &self,
        repo: &Path,
        pr: &str,
        mut st: WatchState,
        snap: Option<RollupView>,
        cfg: &crate::config::Config,
    ) {
        let Some(snap) = snap else {
            return;
        };
        match snap.state {
            PrLifecycle::Closed => self.finish(pr, &st),
            PrLifecycle::Merged => self.release_merged(repo, pr, st, cfg).await,
            PrLifecycle::Open => {
                let approved = match st.question.clone() {
                    None => Approved::NoQuestion,
                    Some(id) => match self.questions().get(&id) {
                        Ok(q) if q.status.open() => Approved::Open,
                        Ok(q) => match &q.answer {
                            Some(Answer::Choice(c))
                                if land::approval(Some(c)) == land::Approval::Merge =>
                            {
                                Approved::Merge
                            }
                            _ => Approved::Hold,
                        },
                        Err(_) => Approved::NoQuestion,
                    },
                };
                let step = decide_local(&snap.head, &st, approved);
                if matches!(
                    step,
                    LocalStep::Merge(_) | LocalStep::Hold(_) | LocalStep::Ask
                ) {
                    // A settled (or stale) question is consumed exactly once.
                    st.question = None;
                }
                match step {
                    LocalStep::Wait => {}
                    LocalStep::Hold(head) => st.held_head = Some(head),
                    LocalStep::Ask => {
                        st.held_head = None;
                        st.asked_head = Some(snap.head.clone());
                        self.raise(pr, approval_message(pr));
                        let mut q = Question::new(
                            String::new(),
                            crate::bump::NOTICE_NODE.to_owned(),
                            "release-watch".to_owned(),
                            format!("Release PR {pr}: merge it and release?"),
                            format!(
                                "Pull request: {}\nHead: {}\n\n\
                                 `[release] mode = \"local\"`: no CI is awaited. `{}` merges \
                                 exactly this head, then magi tags the merge commit and runs the \
                                 configured release commands. `{}` leaves the pull request \
                                 open. Silence is a hold.\n",
                                snap.url,
                                snap.head,
                                land::APPROVE,
                                land::HOLD
                            ),
                            vec![land::APPROVE.to_owned(), land::HOLD.to_owned()],
                        );
                        match self.questions().put(&mut q) {
                            Ok(()) => st.question = Some(q.id.clone()),
                            Err(e) => tracing::warn!("could not file the question for {pr}: {e:#}"),
                        }
                    }
                    LocalStep::Merge(head) => {
                        match self.forge.merge(repo, &snap.url, &head).await {
                            Ok(_) => {
                                // Release at once rather than a lap later.
                                self.save(pr, &st);
                                self.release_merged(repo, pr, st, cfg).await;
                                return;
                            }
                            Err(e) => {
                                tracing::warn!("could not merge {}: {e:#}", snap.url);
                                st.held_head = Some(head);
                                self.raise(
                                    pr,
                                    format!(
                                        "Release PR {pr} could not be merged; merge it by hand"
                                    ),
                                );
                            }
                        }
                    }
                }
                self.save(pr, &st);
            }
        }
    }

    /// The pull request is merged: create the job once, then drive it.
    async fn release_merged(
        &self,
        repo: &Path,
        pr: &str,
        mut st: WatchState,
        cfg: &crate::config::Config,
    ) {
        if st.ignored {
            return self.finish(pr, &st);
        }
        if st.job.is_none() {
            let info = match self.forge.info(repo, &st.url).await {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!("could not read the merged {}: {e:#}", st.url);
                    return;
                }
            };
            let (Some(version), Some(commit)) = (
                release_local::version_from_branch(&info.branch),
                info.merge_commit,
            ) else {
                // Not a release branch, or the forge has not recorded the merge
                // commit yet: nothing to release (yet).
                if release_local::version_from_branch(&info.branch).is_none() {
                    self.finish(pr, &st);
                }
                return;
            };
            let mut job = Job::new(&version, &st.url, &commit);
            // An escalated pull request keeps its first branch name but gets a
            // new title; the manifest at the merge commit picks between them.
            job.accepted = release_local::version_from_title(&info.title)
                .into_iter()
                .filter(|t| *t != version)
                .collect();
            st.job = Some(job);
            if !self.save(pr, &st) {
                return;
            }
        }
        let Some(mut job) = st.job.take() else {
            return;
        };
        if job.finished {
            // Saved as finished, but a stop may have come before the task was
            // given back: do that now, and keep the record until it is.
            st.job = Some(job);
            if !self.record_on_run(&st) {
                self.save(pr, &st);
                return;
            }
            return self.complete(pr, &mut st);
        }
        if job.interrupted() {
            job.failed = Some(format!(
                "magi stopped while `{}` was running; it may have partly run, so it is not repeated on its own",
                job.running.clone().unwrap_or_default()
            ));
        }
        if let Some(why) = job.failed.clone() {
            // Reconcile first: a stop between saving the failure and holding
            // the task (or an interrupted step) must not leave the task Done.
            self.hold_task(&mut st, &format!("release {} is on hold: {why}", job.tag()));
            // Held: only the owner's `retry` runs it again.
            let retry = self.settle_job_question(pr, &mut st, &mut job);
            if !retry {
                st.job = Some(job);
                self.save(pr, &st);
                return;
            }
        }
        let shell = cfg.shell();
        let env = release_local::Env {
            repo,
            home: &self.home,
            key: pr,
            remote: &cfg.merge.remote,
            shell: &shell,
            release: &cfg.release,
        };
        let result = {
            let mut save = |j: &Job| {
                let mut copy = st.clone();
                copy.job = Some(j.clone());
                self.save(pr, &copy)
            };
            release_local::run_job(&env, &mut job, &mut save).await
        };
        match result {
            Ok(()) => {
                self.raise_released(pr, &job);
                st.job = Some(job);
                if self.record_on_run(&st) {
                    self.complete(pr, &mut st);
                } else {
                    // Finished and saved as such: the next lap retries only
                    // the run record, never the commands.
                    self.save(pr, &st);
                }
            }
            Err(e) => {
                job.failed = Some(format!("{e:#}"));
                self.hold_task(&mut st, &format!("release {} failed: {e:#}", job.tag()));
                self.raise(pr, failed_message(pr));
                self.file_job_question(pr, &mut st, &job);
                st.job = Some(job);
                self.save(pr, &st);
                self.record_on_run(&st);
            }
        }
    }

    /// Copy the job onto the run that opened the release PR, so what the
    /// release did outlives the watcher's record. Returns whether the record
    /// may go: `false` only when a readable run could not be saved. No run, or
    /// a run that is gone, is warned about and counts as done.
    fn record_on_run(&self, st: &WatchState) -> bool {
        let (Some(run), Some(job)) = (st.run.as_deref(), st.job.as_ref()) else {
            return true;
        };
        let mut state = match RunState::load_under(run, &self.home) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("could not record the release on run {run}: {e:#}");
                return true;
            }
        };
        let bump = state.release_bump.get_or_insert_with(Default::default);
        if bump.release.as_ref() == Some(job) {
            return true;
        }
        bump.release = Some(job.clone());
        state.event(
            "release",
            format!(
                "release {} {}",
                job.tag(),
                if job.finished {
                    "finished"
                } else if let Some(why) = &job.failed {
                    why
                } else {
                    "stopped"
                }
            ),
        );
        match state.save_under(&self.home) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("could not save the release on run {run}: {e:#}");
                false
            }
        }
    }

    /// Hold the task whose run opened this release PR: the daemon marked it
    /// Done when the change merged, but the release is not done. Best-effort;
    /// a task the owner already holds or that cannot be found is left alone.
    fn hold_task(&self, st: &mut WatchState, reason: &str) {
        if st.held_task.is_some() {
            return;
        }
        let Some(run) = st.run.clone() else {
            return;
        };
        let q = crate::queue::Queue::at(self.home.join("queue"));
        for mut t in q.list() {
            if t.runs.contains(&run) {
                if t.status != crate::queue::TaskStatus::Held {
                    t.hold_machine(Some(format!("{HOLD_PREFIX}{reason}")));
                    match q.put(&mut t) {
                        Ok(()) => st.held_task = Some(t.id.clone()),
                        Err(e) => tracing::warn!("could not hold task {} for {run}: {e:#}", t.id),
                    }
                }
                return;
            }
        }
    }

    /// Give back the hold [`Watcher::hold_task`] put on a task, once the
    /// release succeeded. Only a machine hold carrying our marker is undone: a
    /// task the owner has since re-held or changed is left alone.
    ///
    /// Returns whether nothing is left to give back: `false` only when the
    /// write failed, in which case `held_task` is kept so a later lap retries.
    #[must_use]
    fn unhold_task(&self, st: &mut WatchState) -> bool {
        let Some(id) = st.held_task.take() else {
            return true;
        };
        let q = crate::queue::Queue::at(self.home.join("queue"));
        let mut t = match q.get(&id) {
            Ok(t) => t,
            // Only a task file confirmed absent is "nothing to restore";
            // any other failure may be transient, so keep the record.
            Err(_) if matches!(q.path_of(&id).try_exists(), Ok(false)) => return true,
            Err(e) => {
                tracing::warn!("could not read task {id} to restore it after the release: {e:#}");
                st.held_task = Some(id);
                return false;
            }
        };
        let ours = t.status == crate::queue::TaskStatus::Held
            && t.hold_source == Some(crate::queue::HoldSource::Machine)
            && t.hold_reason
                .as_deref()
                .is_some_and(|r| r.starts_with(HOLD_PREFIX));
        if ours {
            t.succeed();
            if let Err(e) = q.put(&mut t) {
                tracing::warn!("could not restore task {id} after the release: {e:#}");
                st.held_task = Some(id);
                return false;
            }
        }
        true
    }

    /// The release is done (`st.job` is finished): give the task back, then
    /// drop the record. If the task cannot be restored the record stays, so
    /// the next lap tries again instead of leaving the task Held for good.
    fn complete(&self, pr: &str, st: &mut WatchState) {
        if self.unhold_task(st) {
            self.finish(pr, st);
        } else {
            self.save(pr, st);
        }
    }

    fn raise_released(&self, pr: &str, job: &Job) {
        notices::raise_in(
            &self.home,
            Notice::info(&released_key(pr), format!("Released {} ({pr})", job.tag())),
        );
    }

    /// Ask what to do about a held job. One question per failure.
    fn file_job_question(&self, pr: &str, st: &mut WatchState, job: &Job) {
        let last = job
            .log
            .last()
            .map(|l| format!("\nLast step: {}\n\n{}\n", l.name, l.tail))
            .unwrap_or_default();
        let mut q = Question::new(
            String::new(),
            crate::bump::NOTICE_NODE.to_owned(),
            "release-watch".to_owned(),
            format!("Release {} of {pr} is on hold: retry?", job.tag()),
            format!(
                "Pull request: {}\nWhy it stopped: {}\n{last}\n\
                 Full output is under the magi home's release-local directory.\n\n\
                 - `{RETRY}`: run it again from where it stopped (the tag is not \
                 recreated and finished commands are skipped).\n\
                 - `{LEAVE_IT}`: stop watching; release by hand.\n\n\
                 Nothing is retried on its own; silence is a hold.\n",
                job.pr_url,
                job.failed.clone().unwrap_or_default()
            ),
            vec![RETRY.to_owned(), LEAVE_IT.to_owned()],
        );
        match self.questions().put(&mut q) {
            Ok(()) => st.question = Some(q.id.clone()),
            Err(e) => tracing::warn!("could not file the question for {pr}: {e:#}"),
        }
    }

    /// Read the owner's word on a held job. `true` means `retry`: the job was
    /// resumed. Files the question when there is none yet and the owner has not
    /// already held this exact failure.
    fn settle_job_question(&self, pr: &str, st: &mut WatchState, job: &mut Job) -> bool {
        let why = job.failed.clone().unwrap_or_default();
        if let Some(id) = st.question.clone() {
            match self.questions().get(&id) {
                Ok(q) if q.status.open() => return false,
                Ok(q) => {
                    st.question = None;
                    match &q.answer {
                        Some(Answer::Choice(c)) if c == RETRY => {
                            job.resume();
                            st.held = None;
                            return true;
                        }
                        Some(Answer::Choice(c)) if c == LEAVE_IT => {
                            st.ignored = true;
                            let _ = Notices::at(self.home.join("notifications"))
                                .dismiss(&notices::id_of(&notice_key(pr)));
                        }
                        // Abandoned or anything else: this failure stays held.
                        _ => st.held = Some(why),
                    }
                    return false;
                }
                Err(_) => st.question = None,
            }
        }
        if !st.ignored && st.held.as_deref() != Some(why.as_str()) {
            self.raise(pr, failed_message(pr));
            self.file_job_question(pr, st, job);
        }
        false
    }

    /// Apply a settled question's outcome to the record, once.
    fn apply_answer(&self, pr: &str, st: &mut WatchState, snap: &RollupView, q: &Question) {
        st.question = None;
        if st.applied.contains(&q.id) {
            return;
        }
        st.applied.push(q.id.clone());
        if st.applied.len() > 20 {
            st.applied.remove(0);
        }
        match &q.answer {
            Some(Answer::Choice(c)) if c == RERUN_AGAIN => {
                st.held = None;
                for c in snap.checks.iter().filter(|c| is_failed(c.verdict)) {
                    if let Some(r) = &c.run {
                        st.reruns.remove(r);
                    }
                }
            }
            Some(Answer::Choice(c)) if c == LEAVE_IT => {
                st.ignored = true;
                if let Err(e) = Notices::at(self.home.join("notifications"))
                    .dismiss(&notices::id_of(&notice_key(pr)))
                {
                    tracing::warn!("could not dismiss the notice for {pr}: {e:#}");
                }
            }
            // `hold`, no answer, abandoned: quiet until something changes.
            _ => st.held = Some(fingerprint(snap)),
        }
    }

    /// The pull request is confirmed merged or closed: clear everything.
    fn finish(&self, pr: &str, st: &WatchState) {
        let _ =
            Notices::at(self.home.join("notifications")).dismiss(&notices::id_of(&notice_key(pr)));
        if let Some(id) = &st.question {
            let _ = self.questions().update(id, |q| {
                q.abandon("the release pull request is no longer open");
                Ok(())
            });
        }
        let _ = std::fs::remove_file(self.state_path(pr));
    }
}

/// The task `magi serve` spawns. `settings` returns the checkouts to cover and
/// `[daemon] release_stall_minutes`; it is re-read every lap so a config
/// change is picked up, and `0` minutes switches the lap off.
pub(crate) async fn run(
    watcher: Watcher,
    settings: impl Fn() -> (Vec<PathBuf>, u64),
    stop: crate::daemon::Stop,
) {
    let halt = {
        let stop = stop.clone();
        move || stop.stopped()
    };
    while !stop.stopped() {
        let (repos, minutes) = settings();
        if minutes > 0 {
            let now = jiff::Timestamp::now().as_second();
            watcher
                .lap(&repos, (minutes as i64).saturating_mul(60), now, &halt)
                .await;
        }
        let mut slept = Duration::ZERO;
        while slept < LAP && !stop.stopped() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            slept += Duration::from_secs(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::land::CheckView;
    use anyhow::Context;
    use std::sync::Mutex;

    const URL: &str = "https://github.com/o/r/pull/7";

    fn check(name: &str, v: Verdict, run: Option<&str>) -> CheckView {
        CheckView {
            name: name.to_owned(),
            verdict: v,
            run: run.map(str::to_owned),
            url: run.map(|r| format!("https://github.com/o/r/actions/runs/{r}/job/1")),
        }
    }

    fn snap(state: PrLifecycle, head: &str, checks: Vec<CheckView>) -> RollupView {
        RollupView {
            url: URL.to_owned(),
            number: 7,
            state,
            head: head.to_owned(),
            checks,
        }
    }

    fn open(checks: Vec<CheckView>) -> RollupView {
        snap(PrLifecycle::Open, "h1", checks)
    }

    fn st_at(progress: i64) -> WatchState {
        WatchState {
            progress_at: progress,
            ..WatchState::default()
        }
    }

    #[test]
    fn unreadable_is_unknown_and_merged_or_closed_is_done() {
        assert_eq!(decide(None, &st_at(0), 10, 60), Step::Unknown);
        for s in [PrLifecycle::Merged, PrLifecycle::Closed] {
            assert_eq!(
                decide(Some(&snap(s, "h", vec![])), &st_at(0), 10, 60),
                Step::Done
            );
        }
    }

    #[test]
    fn a_failed_run_is_rerun_even_while_another_run_is_pending() {
        let s = open(vec![
            check("win", Verdict::Fail, Some("11")),
            check("lint", Verdict::Pending, Some("12")),
        ]);
        assert_eq!(
            decide(Some(&s), &st_at(0), 1, 3600),
            Step::Rerun(vec!["11".into()])
        );
    }

    #[test]
    fn a_run_with_a_job_still_pending_is_not_complete() {
        let s = open(vec![
            check("win", Verdict::Fail, Some("11")),
            check("mac", Verdict::Pending, Some("11")),
        ]);
        assert_eq!(decide(Some(&s), &st_at(0), 1, 3600), Step::Wait);
    }

    #[test]
    fn one_rerun_per_run_then_grace_then_escalation() {
        let s = open(vec![check("win", Verdict::Fail, Some("11"))]);
        let mut st = st_at(0);
        st.reruns.insert("11".into(), 100);
        assert_eq!(decide(Some(&s), &st, 100 + RERUN_GRACE - 1, 0), Step::Wait);
        assert_eq!(
            decide(Some(&s), &st, 100 + RERUN_GRACE, 0),
            Step::Escalate(Why::StillRed)
        );
    }

    #[test]
    fn a_failure_with_no_run_id_goes_straight_to_a_human_once_settled() {
        let s = open(vec![check("ci/ext", Verdict::Fail, None)]);
        assert_eq!(
            decide(Some(&s), &st_at(0), 1, 0),
            Step::Escalate(Why::StillRed)
        );
        let s = open(vec![
            check("ci/ext", Verdict::Fail, None),
            check("x", Verdict::Pending, Some("5")),
        ]);
        assert_eq!(decide(Some(&s), &st_at(0), 1, 0), Step::Wait);
    }

    #[test]
    fn no_progress_for_the_bounded_time_escalates_and_zero_disables_it() {
        let s = open(vec![check("a", Verdict::Pass, Some("1"))]);
        assert_eq!(decide(Some(&s), &st_at(0), 3599, 3600), Step::Wait);
        assert_eq!(
            decide(Some(&s), &st_at(0), 3600, 3600),
            Step::Escalate(Why::Stalled)
        );
        assert_eq!(decide(Some(&s), &st_at(0), 99_999, 0), Step::Wait);
    }

    #[test]
    fn held_and_ignored_stay_quiet_until_the_fingerprint_moves() {
        let s = open(vec![check("a", Verdict::Fail, None)]);
        let mut st = st_at(0);
        st.held = Some(fingerprint(&s));
        assert_eq!(decide(Some(&s), &st, 99_999, 60), Step::Wait);
        let moved = open(vec![
            check("a", Verdict::Pass, None),
            check("b", Verdict::Fail, None),
        ]);
        assert_eq!(
            decide(Some(&moved), &st, 99_999, 60),
            Step::Escalate(Why::StillRed)
        );
        st.ignored = true;
        assert_eq!(decide(Some(&moved), &st, 99_999, 60), Step::Wait);
    }

    #[test]
    fn observe_restarts_the_clock_only_on_change_and_forgets_reruns_on_a_new_head() {
        let mut st = st_at(0);
        let a = open(vec![check("a", Verdict::Pending, Some("1"))]);
        observe(&mut st, &a, 10);
        assert_eq!(st.progress_at, 10);
        observe(&mut st, &a, 50);
        assert_eq!(st.progress_at, 10);
        st.reruns.insert("1".into(), 5);
        observe(
            &mut st,
            &snap(PrLifecycle::Open, "h2", a.checks.clone()),
            60,
        );
        assert!(st.reruns.is_empty());
        assert_eq!(st.progress_at, 60);
    }

    #[test]
    fn pr_key_names_owner_repo_and_number() {
        assert_eq!(pr_key(URL).as_deref(), Some("o/r#7"));
        assert_eq!(pr_key("https://github.com/o/r/issues/7"), None);
        assert_eq!(pr_key("nonsense"), None);
    }

    #[test]
    fn notice_wording_does_not_vary_between_polls() {
        assert_eq!(rerun_message("o/r#7"), rerun_message("o/r#7"));
        assert!(
            !human_message("o/r#7")
                .chars()
                .any(|c| c.is_ascii_digit() && c != '7')
        );
    }

    #[test]
    fn the_release_question_is_not_claimed_by_other_machinery() {
        let q = Question::new(
            String::new(),
            crate::bump::NOTICE_NODE.into(),
            "release-watch".into(),
            "s".into(),
            String::new(),
            vec![HOLD.into()],
        );
        // The watcher's question is served by a deputy, but still covers nothing.
        assert_eq!(
            crate::deputy::kind_of(&q),
            Some(crate::deputy::Kind::Release)
        );
        let n = Notice::warn("release-pr:o/r#7", "m").about([String::new()]);
        assert!(!notices::covers(&q, &n));
        // A choice-less notice on the same node (seat `bump`) gets no deputy.
        let mut plain = q.clone();
        plain.seat = "bump".into();
        assert_eq!(crate::deputy::kind_of(&plain), None);
    }

    #[derive(Default)]
    struct Fake {
        snap: Mutex<Option<RollupView>>,
        reruns: Mutex<Vec<String>>,
        local: Mutex<bool>,
        info: Mutex<Option<PrInfo>>,
        merges: Mutex<Vec<String>>,
    }

    impl ReleaseForge for std::sync::Arc<Fake> {
        fn list<'a>(&'a self, _: &'a Path) -> Fut<'a, Result<Vec<(String, String)>>> {
            Box::pin(async { Ok(vec![("chore/release-v1.0.0".to_owned(), URL.to_owned())]) })
        }
        fn snapshot<'a>(&'a self, _: &'a Path, _: &'a str) -> Fut<'a, Result<RollupView>> {
            let s = self.snap.lock().unwrap().clone();
            Box::pin(async move { s.context("unreadable") })
        }
        fn rerun<'a>(&'a self, _: &'a Path, run: &'a str) -> Fut<'a, Result<()>> {
            self.reruns.lock().unwrap().push(run.to_owned());
            Box::pin(async { Ok(()) })
        }
        fn config<'a>(&'a self, _: &'a Path) -> Fut<'a, Result<crate::config::Config>> {
            let mut c = crate::config::Config::default();
            if *self.local.lock().unwrap() {
                c.release.mode = crate::config::ReleaseMode::Local;
                c.release.commands = vec!["true".to_owned()];
            }
            Box::pin(async move { Ok(c) })
        }
        fn info<'a>(&'a self, _: &'a Path, _: &'a str) -> Fut<'a, Result<PrInfo>> {
            let i = self.info.lock().unwrap().clone();
            Box::pin(async move { i.context("no info") })
        }
        fn merge<'a>(&'a self, _: &'a Path, _: &'a str, head: &'a str) -> Fut<'a, Result<bool>> {
            self.merges.lock().unwrap().push(head.to_owned());
            Box::pin(async { Ok(true) })
        }
    }

    fn rig() -> (tempfile::TempDir, std::sync::Arc<Fake>, Watcher) {
        let dir = tempfile::tempdir().unwrap();
        let fake = std::sync::Arc::new(Fake::default());
        let w = Watcher::new(Box::new(fake.clone()), dir.path().to_path_buf());
        (dir, fake, w)
    }

    fn red() -> RollupView {
        open(vec![check("win", Verdict::Fail, Some("11"))])
    }

    #[tokio::test]
    async fn reruns_once_survives_a_restart_then_escalates_with_one_question() {
        let (dir, fake, w) = rig();
        *fake.snap.lock().unwrap() = Some(red());
        let repo = PathBuf::from("/nowhere");
        let no = || false;
        w.lap(std::slice::from_ref(&repo), 3600, 1000, &no).await;
        assert_eq!(*fake.reruns.lock().unwrap(), vec!["11".to_owned()]);

        // A restart is a new Watcher over the same home: no second rerun.
        let w2 = Watcher::new(Box::new(fake.clone()), dir.path().to_path_buf());
        w2.lap(
            std::slice::from_ref(&repo),
            3600,
            1000 + RERUN_GRACE - 1,
            &no,
        )
        .await;
        assert_eq!(fake.reruns.lock().unwrap().len(), 1);
        assert!(w2.questions().list().is_empty());

        w2.lap(
            std::slice::from_ref(&repo),
            3600,
            1000 + RERUN_GRACE + 1,
            &no,
        )
        .await;
        w2.lap(
            std::slice::from_ref(&repo),
            3600,
            1000 + RERUN_GRACE + 400,
            &no,
        )
        .await;
        assert_eq!(fake.reruns.lock().unwrap().len(), 1);
        let qs = w2.questions().list();
        assert_eq!(qs.len(), 1, "asked once, not every lap");
        assert_eq!(qs[0].node, crate::bump::NOTICE_NODE);
        assert!(qs[0].detail.contains("win") && qs[0].detail.contains(URL));
        assert_eq!(qs[0].choices, vec![RERUN_AGAIN, HOLD, LEAVE_IT]);
        let ns = Notices::at(dir.path().join("notifications")).list();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].key, "release-pr:o/r#7");
    }

    async fn escalated() -> (tempfile::TempDir, std::sync::Arc<Fake>, Watcher, String) {
        let (dir, fake, w) = rig();
        *fake.snap.lock().unwrap() = Some(red());
        let repo = PathBuf::from("/nowhere");
        let no = || false;
        w.lap(std::slice::from_ref(&repo), 3600, 1000, &no).await;
        w.lap(&[repo], 3600, 2000, &no).await;
        let id = w.questions().list()[0].id.clone();
        (dir, fake, w, id)
    }

    fn answer(w: &Watcher, id: &str, choice: &str) {
        w.questions()
            .update(id, |q| q.answer(Answer::Choice(choice.to_owned())))
            .unwrap();
    }

    #[tokio::test]
    async fn rerun_again_reruns_exactly_once_more() {
        let (_d, fake, w, id) = escalated().await;
        answer(&w, &id, RERUN_AGAIN);
        let repo = PathBuf::from("/nowhere");
        let no = || false;
        w.lap(std::slice::from_ref(&repo), 3600, 3000, &no).await;
        w.lap(&[repo], 3600, 3001, &no).await;
        assert_eq!(fake.reruns.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn hold_and_silence_do_not_reask_and_leave_it_dismisses() {
        let (d, fake, w, id) = escalated().await;
        answer(&w, &id, HOLD);
        let repo = PathBuf::from("/nowhere");
        let no = || false;
        for t in [3000, 90_000, 200_000] {
            w.lap(std::slice::from_ref(&repo), 3600, t, &no).await;
        }
        assert_eq!(w.questions().list().len(), 1, "held: no second question");
        assert_eq!(fake.reruns.lock().unwrap().len(), 1);

        let (d2, _f2, w2, id2) = escalated().await;
        answer(&w2, &id2, LEAVE_IT);
        w2.lap(std::slice::from_ref(&repo), 3600, 3000, &no).await;
        let ns = Notices::at(d2.path().join("notifications")).list();
        assert!(ns.is_empty(), "dismissed notices are hidden");
        drop(d);
    }

    /// Give the question a deputy seat the way `Deputies::attach` + a turn do.
    fn with_deputy(w: &Watcher, id: &str, home: &Path) -> String {
        let seat = crate::agent::SeatState::new(&crate::ask::deputy_seat_key(id), "a", 1);
        let key = seat.key.clone();
        let brief = deputy_brief(&w.questions().get(id).unwrap(), home);
        w.questions()
            .update(id, |q| {
                let mut d = crate::ask::Deputy::new(brief);
                d.seat = Some(seat);
                q.deputy = Some(d);
                Ok(())
            })
            .unwrap();
        key
    }

    #[tokio::test]
    async fn the_brief_names_the_pull_request_and_what_leave_it_really_does() {
        let (d, _f, w, id) = escalated().await;
        let q = w.questions().get(&id).unwrap();
        let b = deputy_brief(&q, d.path());
        assert!(b.contains(URL), "{b}");
        assert!(
            b.contains("`leave it`") && b.contains("does NOT close"),
            "{b}"
        );
        assert!(b.contains("`rerun again`") && b.contains("`hold`"), "{b}");
        assert!(!b.contains("`merge`"), "no merge on an escalation: {b}");

        // No record naming the question: said, not invented.
        let empty = tempfile::tempdir().unwrap();
        let b = deputy_brief(&q, empty.path());
        assert!(b.contains("could not be found") && !b.contains(URL), "{b}");
    }

    #[tokio::test]
    async fn a_settled_leave_it_is_applied_once_by_the_watcher_and_closes_nothing() {
        let (d, fake, w, id) = escalated().await;
        let seat = with_deputy(&w, &id, d.path());
        let say = "クローズしていいよ。private repo だから、何回やっても失敗しちゃうから";
        w.questions().update(&id, |q| q.say(say)).unwrap();
        w.questions()
            .update(&id, |q| {
                q.settle_by_deputy(&seat, LEAVE_IT, "クローズしていいよ", None)
            })
            .unwrap();
        let q = w.questions().get(&id).unwrap();
        assert_eq!(q.answer, Some(Answer::Choice(LEAVE_IT.to_owned())));

        let repo = PathBuf::from("/nowhere");
        let no = || false;
        for t in [3000, 3100] {
            w.lap(std::slice::from_ref(&repo), 3600, t, &no).await;
        }
        let st = w.stored().pop().unwrap();
        assert!(st.ignored, "watching stopped");
        assert_eq!(st.applied, vec![id.clone()], "applied exactly once");
        assert!(
            Notices::at(d.path().join("notifications"))
                .list()
                .is_empty()
        );
        assert_eq!(fake.reruns.lock().unwrap().len(), 1, "no extra rerun");
        assert_eq!(w.questions().list().len(), 1, "no second question");
    }

    #[tokio::test]
    async fn the_task_of_a_release_question_comes_from_the_watch_record() {
        let (d, _f, w, id) = escalated().await;
        let q = w.questions().get(&id).unwrap();
        let queue = crate::queue::Queue::at(d.path().join("queue"));
        assert!(
            task_of_question(d.path(), &q, &queue).is_none(),
            "no run known"
        );
        assert!(task_of_question(d.path(), &q, &queue).is_none());
        let mut st = w.stored().pop().unwrap();
        st.run = Some("run-1".into());
        w.save("o/r#7", &st);
        // A known run with no task is still nothing to refuse on.
        assert!(task_of_question(d.path(), &q, &queue).is_none());
        let mut t = crate::queue::Task::new(
            "t".into(),
            "i".into(),
            PathBuf::from("/nowhere"),
            crate::queue::Source::Human,
        );
        t.runs.push("run-1".into());
        queue.put(&mut t).unwrap();
        assert_eq!(task_of_question(d.path(), &q, &queue).unwrap().id, t.id);
    }

    #[tokio::test]
    async fn merged_clears_the_notice_the_question_and_the_record() {
        let (d, fake, w, _id) = escalated().await;
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Merged, "h1", vec![]));
        let repo = PathBuf::from("/nowhere");
        w.lap(&[repo], 3600, 5000, &(|| false)).await;
        assert!(
            Notices::at(d.path().join("notifications"))
                .list()
                .is_empty()
        );
        assert!(w.questions().list().iter().all(|q| !q.status.open()));
        assert!(w.stored().is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_forge_changes_nothing() {
        let (d, fake, w) = rig();
        *fake.snap.lock().unwrap() = None;
        w.lap(&[PathBuf::from("/nowhere")], 60, 99_999, &(|| false))
            .await;
        assert!(fake.reruns.lock().unwrap().is_empty());
        assert!(w.questions().list().is_empty());
        assert!(!d.path().join("release-watch").exists());
    }

    #[tokio::test]
    async fn no_rerun_when_the_record_cannot_be_saved() {
        let (d, fake, w) = rig();
        *fake.snap.lock().unwrap() = Some(red());
        // A file where the directory should be makes every save fail.
        std::fs::write(d.path().join("release-watch"), "x").unwrap();
        w.lap(&[PathBuf::from("/nowhere")], 3600, 1000, &(|| false))
            .await;
        assert!(fake.reruns.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_park_stops_the_lap_before_any_call() {
        let (_d, fake, w) = rig();
        *fake.snap.lock().unwrap() = Some(red());
        w.lap(&[PathBuf::from("/nowhere")], 60, 1000, &(|| true))
            .await;
        assert!(fake.reruns.lock().unwrap().is_empty());
    }

    // ---- [release] mode = "local" ----

    fn local_st(asked: Option<&str>, held: Option<&str>) -> WatchState {
        WatchState {
            asked_head: asked.map(str::to_owned),
            held_head: held.map(str::to_owned),
            ..WatchState::default()
        }
    }

    #[test]
    fn local_decisions_never_wait_on_ci_and_bind_approval_to_the_head() {
        use Approved::*;
        let none = local_st(None, None);
        assert_eq!(decide_local("h1", &none, NoQuestion), LocalStep::Ask);
        assert_eq!(decide_local("h1", &none, Open), LocalStep::Wait);
        let asked = local_st(Some("h1"), None);
        assert_eq!(
            decide_local("h1", &asked, Merge),
            LocalStep::Merge("h1".to_owned())
        );
        assert_eq!(
            decide_local("h1", &asked, Hold),
            LocalStep::Hold("h1".to_owned())
        );
        // The head moved after the owner answered: ask again, never merge.
        assert_eq!(decide_local("h2", &asked, Merge), LocalStep::Ask);
        assert_eq!(decide_local("h2", &asked, Hold), LocalStep::Ask);
        // A held head stays quiet until it moves; ignored stays quiet.
        let held = local_st(Some("h1"), Some("h1"));
        assert_eq!(decide_local("h1", &held, NoQuestion), LocalStep::Wait);
        assert_eq!(decide_local("h2", &held, NoQuestion), LocalStep::Ask);
        let mut ign = local_st(None, None);
        ign.ignored = true;
        assert_eq!(decide_local("h1", &ign, NoQuestion), LocalStep::Wait);
    }

    #[test]
    fn pr_info_is_read_from_gh_json() {
        let i = parse_info(
            r#"{"headRefName":"chore/release-v1.2.3","title":"chore: release v1.2.3","mergeCommit":{"oid":"abc"}}"#,
        )
        .unwrap();
        assert_eq!(i.branch, "chore/release-v1.2.3");
        assert_eq!(i.merge_commit.as_deref(), Some("abc"));
        let open = parse_info(r#"{"headRefName":"b","title":"t","mergeCommit":null}"#).unwrap();
        assert_eq!(open.merge_commit, None);
    }

    #[tokio::test]
    async fn a_local_release_asks_once_without_ci_reruns_or_stall_escalation() {
        let (_d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        // Red checks that would rerun in Actions mode, and a stall clock far
        // past the limit: neither matters here.
        *fake.snap.lock().unwrap() = Some(red());
        let repo = PathBuf::from("/nowhere");
        let no = || false;
        w.lap(std::slice::from_ref(&repo), 60, 1_000_000, &no).await;
        w.lap(std::slice::from_ref(&repo), 60, 2_000_000, &no).await;
        assert!(fake.reruns.lock().unwrap().is_empty());
        let qs = w.questions().list();
        assert_eq!(qs.len(), 1, "one approval question, not one per lap");
        assert_eq!(qs[0].choices, vec![land::APPROVE, land::HOLD]);
        assert!(fake.merges.lock().unwrap().is_empty(), "silence is a hold");
    }

    #[tokio::test]
    async fn merge_is_pinned_to_the_approved_head_and_a_moved_head_asks_again() {
        let (_d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        *fake.snap.lock().unwrap() = Some(open(vec![]));
        let repo = PathBuf::from("/nowhere");
        let no = || false;
        w.lap(std::slice::from_ref(&repo), 60, 1, &no).await;
        let id = w.questions().list()[0].id.clone();
        // The head moves before the owner answers.
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Open, "h2", vec![]));
        answer(&w, &id, land::APPROVE);
        w.lap(std::slice::from_ref(&repo), 60, 2, &no).await;
        assert!(fake.merges.lock().unwrap().is_empty());
        assert_eq!(w.questions().list().len(), 2, "asked again about h2");
        // Answering the new question merges exactly h2.
        let id2 = w
            .questions()
            .list()
            .into_iter()
            .find(|q| q.status.open())
            .unwrap()
            .id;
        answer(&w, &id2, land::APPROVE);
        w.lap(std::slice::from_ref(&repo), 60, 3, &no).await;
        assert_eq!(*fake.merges.lock().unwrap(), vec!["h2".to_owned()]);
    }

    #[tokio::test]
    async fn a_failed_release_holds_with_one_notice_and_one_question_and_never_retries() {
        let (d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Merged, "h1", vec![]));
        *fake.info.lock().unwrap() = Some(PrInfo {
            branch: "chore/release-v1.0.0".to_owned(),
            merge_commit: Some("deadbeef".to_owned()),
            title: "chore: release v1.0.0".to_owned(),
        });
        // The checkout is not a repository, so the release cannot start.
        let repo = d.path().join("nowhere");
        let no = || false;
        w.lap(std::slice::from_ref(&repo), 60, 1, &no).await;
        let st = w.load("o/r#7");
        let job = st.job.expect("the job is recorded");
        assert!(job.failed.is_some() && !job.tag_done && !job.finished);
        let qs = w.questions().list();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].choices, vec![RETRY, LEAVE_IT]);
        // Later laps change nothing: no retry, no second question.
        w.lap(std::slice::from_ref(&repo), 60, 2, &no).await;
        assert_eq!(w.questions().list().len(), 1);
        assert_eq!(Notices::at(d.path().join("notifications")).list().len(), 1);
    }

    #[tokio::test]
    async fn an_escalated_pull_request_records_the_title_version_as_acceptable() {
        let (d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Merged, "h1", vec![]));
        *fake.info.lock().unwrap() = Some(PrInfo {
            branch: "chore/release-v0.1.4".to_owned(),
            merge_commit: Some("deadbeef".to_owned()),
            title: "chore: release v0.2.0 (minor bump)".to_owned(),
        });
        let repo = d.path().join("nowhere");
        w.lap(std::slice::from_ref(&repo), 60, 1, &(|| false)).await;
        let job = w.load("o/r#7").job.expect("the job is recorded");
        assert_eq!(job.version, "0.1.4");
        assert_eq!(job.accepted, vec!["0.2.0".to_owned()]);
    }

    #[tokio::test]
    async fn a_failed_release_holds_the_task_whose_run_opened_the_pr() {
        let (d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Merged, "h1", vec![]));
        *fake.info.lock().unwrap() = Some(PrInfo {
            branch: "chore/release-v1.0.0".to_owned(),
            merge_commit: Some("deadbeef".to_owned()),
            title: "t".to_owned(),
        });
        let repo = d.path().join("nowhere");
        let q = crate::queue::Queue::at(d.path().join("queue"));
        let mut t = crate::queue::Task::new(
            "t".to_owned(),
            "i".to_owned(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        t.runs.push("run1".to_owned());
        t.status = crate::queue::TaskStatus::Done;
        q.put(&mut t).unwrap();
        register(d.path(), &repo, URL, "run1");
        w.lap(std::slice::from_ref(&repo), 60, 1, &(|| false)).await;
        let held = q.get(&t.id).unwrap();
        assert_eq!(held.status, crate::queue::TaskStatus::Held);
        assert!(
            held.hold_reason
                .unwrap_or_default()
                .contains("release v1.0.0")
        );
    }

    #[tokio::test]
    async fn unhold_gives_the_task_back_only_when_the_hold_is_ours() {
        let (d, _fake, w) = rig();
        let q = crate::queue::Queue::at(d.path().join("queue"));
        let mut t = crate::queue::Task::new(
            "t".to_owned(),
            "i".to_owned(),
            PathBuf::from("/r"),
            crate::queue::Source::Human,
        );
        t.runs.push("run1".to_owned());
        t.status = crate::queue::TaskStatus::Done;
        q.put(&mut t).unwrap();
        let mut st = WatchState {
            run: Some("run1".to_owned()),
            ..WatchState::default()
        };
        w.hold_task(&mut st, "failed");
        assert_eq!(q.get(&t.id).unwrap().status, crate::queue::TaskStatus::Held);
        // A restart reconciling again does not hold twice.
        w.hold_task(&mut st, "failed again");
        assert!(w.unhold_task(&mut st));
        assert_eq!(q.get(&t.id).unwrap().status, crate::queue::TaskStatus::Done);
        assert!(st.held_task.is_none());

        // The owner re-held it by hand with their own reason: left alone.
        let mut h = q.get(&t.id).unwrap();
        st.held_task = Some(h.id.clone());
        h.hold_manual(Some("mine".to_owned()));
        q.put(&mut h).unwrap();
        assert!(w.unhold_task(&mut st));
        assert_eq!(q.get(&t.id).unwrap().status, crate::queue::TaskStatus::Held);
    }

    #[tokio::test]
    async fn a_restart_after_the_failure_was_saved_still_holds_the_task() {
        let (d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Merged, "h1", vec![]));
        let repo = d.path().join("nowhere");
        let q = crate::queue::Queue::at(d.path().join("queue"));
        let mut t = crate::queue::Task::new(
            "t".to_owned(),
            "i".to_owned(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        t.runs.push("run1".to_owned());
        t.status = crate::queue::TaskStatus::Done;
        q.put(&mut t).unwrap();
        // The state a crash leaves: failure persisted, task never held.
        let mut job = Job::new("1.0.0", URL, "deadbeef");
        job.failed = Some("command 1 exited 3".to_owned());
        let st = WatchState {
            repo: repo.to_string_lossy().into_owned(),
            url: URL.to_owned(),
            run: Some("run1".to_owned()),
            job: Some(job),
            ..WatchState::default()
        };
        w.save("o/r#7", &st);
        w.lap(std::slice::from_ref(&repo), 60, 1, &(|| false)).await;
        assert_eq!(q.get(&t.id).unwrap().status, crate::queue::TaskStatus::Held);
    }

    #[tokio::test]
    async fn a_restart_after_the_job_finished_still_gives_the_task_back() {
        let (d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Merged, "h1", vec![]));
        let repo = d.path().join("nowhere");
        let q = crate::queue::Queue::at(d.path().join("queue"));
        let mut t = crate::queue::Task::new(
            "t".to_owned(),
            "i".to_owned(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        t.runs.push("run1".to_owned());
        t.hold_machine(Some(format!("{HOLD_PREFIX}release v1.0.0 failed")));
        q.put(&mut t).unwrap();
        // The state a crash leaves: finished saved, task never given back.
        let mut job = Job::new("1.0.0", URL, "deadbeef");
        job.finished = true;
        let st = WatchState {
            repo: repo.to_string_lossy().into_owned(),
            url: URL.to_owned(),
            run: Some("run1".to_owned()),
            job: Some(job),
            held_task: Some(t.id.clone()),
            ..WatchState::default()
        };
        w.save("o/r#7", &st);
        w.lap(std::slice::from_ref(&repo), 60, 1, &(|| false)).await;
        assert_eq!(q.get(&t.id).unwrap().status, crate::queue::TaskStatus::Done);
        assert!(!w.state_path("o/r#7").exists());
    }

    #[tokio::test]
    async fn a_pull_request_registered_at_open_is_picked_up_even_if_unlisted() {
        let (d, _fake, w) = rig();
        register(d.path(), Path::new("/r"), URL, "run1");
        let st = w.load("o/r#7");
        assert_eq!(st.url, URL);
        assert_eq!(st.repo, "/r");
        // Never overwrites a live record.
        let mut live = st.clone();
        live.head = "keep".to_owned();
        w.save("o/r#7", &live);
        register(d.path(), Path::new("/r"), URL, "run1");
        assert_eq!(w.load("o/r#7").head, "keep");
    }

    #[tokio::test]
    async fn a_finished_release_is_kept_on_the_run_and_a_missing_run_still_completes() {
        let (d, fake, w) = rig();
        *fake.local.lock().unwrap() = true;
        *fake.snap.lock().unwrap() = Some(snap(PrLifecycle::Merged, "h1", vec![]));
        let repo = d.path().join("nowhere");
        let mut run = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "0123456789abcdef".to_owned(),
            "t".to_owned(),
            crate::config::Config::default(),
        );
        run.id = "run1".to_owned();
        run.save_under(d.path()).unwrap();
        let mut job = Job::new("1.0.0", URL, "deadbeef");
        job.finished = true;
        job.log.push(release_local::StepLog {
            name: "cmd".to_owned(),
            code: Some(0),
            tail: "ok".to_owned(),
            output: None,
        });
        let mk = |run: &str| WatchState {
            repo: repo.to_string_lossy().into_owned(),
            url: URL.to_owned(),
            run: Some(run.to_owned()),
            job: Some(job.clone()),
            ..WatchState::default()
        };
        w.save("o/r#7", &mk("run1"));
        w.lap(std::slice::from_ref(&repo), 60, 1, &(|| false)).await;
        assert!(!w.state_path("o/r#7").exists());
        let kept = RunState::load_under("run1", d.path()).unwrap();
        assert_eq!(kept.release_bump.unwrap().release, Some(job.clone()));

        // A run that is gone must not keep the watcher record forever.
        w.save("o/r#7", &mk("gone"));
        w.lap(std::slice::from_ref(&repo), 60, 1, &(|| false)).await;
        assert!(!w.state_path("o/r#7").exists());
    }
}
