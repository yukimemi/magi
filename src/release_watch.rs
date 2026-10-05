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

type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Every forge access, so tests inject a fake instead of `gh`.
pub(crate) trait ReleaseForge: Send + Sync {
    /// `(branch, url)` of every open release-bump pull request in `repo`.
    fn list<'a>(&'a self, repo: &'a Path) -> Fut<'a, Result<Vec<(String, String)>>>;
    fn snapshot<'a>(&'a self, repo: &'a Path, url: &'a str) -> Fut<'a, Result<RollupView>>;
    fn rerun<'a>(&'a self, repo: &'a Path, run: &'a str) -> Fut<'a, Result<()>>;
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

    fn save(&self, pr: &str, st: &WatchState) {
        let write = || -> Result<()> {
            std::fs::create_dir_all(self.dir())?;
            let path = self.state_path(pr);
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(st)?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!("could not save the release watch for {pr}: {e:#}");
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
        for repo in repos {
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
                self.save(&pr, &st);
                if halt() {
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
        assert_eq!(crate::deputy::kind_of(&q), None);
        let n = Notice::warn("release-pr:o/r#7", "m").about([String::new()]);
        assert!(!notices::covers(&q, &n));
    }

    #[derive(Default)]
    struct Fake {
        snap: Mutex<Option<RollupView>>,
        reruns: Mutex<Vec<String>>,
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
    async fn a_park_stops_the_lap_before_any_call() {
        let (_d, fake, w) = rig();
        *fake.snap.lock().unwrap() = Some(red());
        w.lap(&[PathBuf::from("/nowhere")], 60, 1000, &(|| true))
            .await;
        assert!(fake.reruns.lock().unwrap().is_empty());
    }
}
