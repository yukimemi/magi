//! Runs started by hand are tasks.
//!
//! `magi run` and `magi review` used to mint a run no task knew about: no
//! detail page, no attempt history, invisible to duplicate detection, and an
//! orphan the moment anything cleaned up after it. They now file a task first,
//! through the queue like `magi task add`, and then either hand it to a live
//! loop or execute it here through the daemon's own attempt
//! ([`crate::daemon::run_claimed`]). There is no second dispatcher in this
//! module: it decides *who* runs the task and watches it, nothing more.
//!
//! * A live loop in another process (a fresh `daemon.json` heartbeat, see
//!   [`crate::daemon::foreign_loop`]) gets the task as `urgent`, runs it in
//!   its own urgent slot, and the caller [`follow`]s it. The caller never
//!   claims or executes it: two processes must not drive one task.
//! * Otherwise the caller claims the task **before it is written**, so a loop
//!   that starts mid-run finds the claim and leaves it alone, and executes it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;

use crate::config::Config;
use crate::queue::{Claim, Queue, RunOverrides, Source, Task, TaskStatus};

/// What to file. Everything `magi run` / `magi review` take that changes what
/// the run does rides on the task, so a loop that executes it behaves the same
/// as this process would have.
#[derive(Debug, Clone)]
pub struct Filing {
    /// The task text; for a review, a line saying which branch.
    pub instruction: String,
    /// The task's title.
    pub title: String,
    /// Absolute repository path.
    pub repo: PathBuf,
    /// Who asked: the operator, or the agent seat named by `MAGI_RUN` / `MAGI_NODE`.
    pub source: Source,
    /// `--solo`.
    pub solo: bool,
    /// Command-line choices carried onto the task.
    pub overrides: RunOverrides,
    /// `magi review <branch>`: the branch to review.
    pub review_of: Option<String>,
}

impl Filing {
    fn into_task(self) -> Task {
        let mut task = Task::new(self.title, self.instruction, self.repo, self.source);
        task.solo = self.solo;
        task.review_of = self.review_of;
        task.overrides = Some(self.overrides);
        task
    }
}

/// Who will execute a filed task.
#[derive(Debug)]
pub enum Filed {
    /// A loop in another process owns the queue; the task is `urgent` and
    /// unclaimed. `pid` is the loop's, when it published one.
    Daemon {
        /// The filed task.
        task: Task,
        /// The loop's pid.
        pid: Option<u32>,
    },
    /// Nobody else is serving: the task is claimed for this process, and the
    /// claim lives as long as this value.
    Standalone {
        /// The filed task.
        task: Task,
        /// Held until dropped, which is what keeps a later loop away.
        claim: Claim,
    },
}

/// File `filing` and decide who runs it. `home` is the magi home whose
/// `daemon.json` is judged; `own_pid` is this process (a loop of our own is
/// not "another" one). `force_local` (`--dry-run`) never hands over: a dry run
/// spends no agent call and a loop would not honour that.
pub fn file(
    queue: &Queue,
    home: &Path,
    now: Timestamp,
    own_pid: u32,
    filing: Filing,
    force_local: bool,
) -> Result<Filed> {
    let mut task = filing.into_task();
    let owner = if force_local {
        None
    } else {
        crate::daemon::foreign_loop(crate::daemon::read_status(home).as_ref(), now, own_pid)
    };
    match owner {
        Some(pid) => {
            task.urgent = true;
            queue.put(&mut task).context("file the task")?;
            Ok(Filed::Daemon { task, pid })
        }
        None => {
            // Claimed first: between `put` and the claim a loop could take it.
            let claim = queue.claim(&task.id)?;
            queue.put(&mut task).context("file the task")?;
            Ok(Filed::Standalone { task, claim })
        }
    }
}

/// The config a task's run is built with: the task's `--config` layer, then
/// its overrides and `solo`. What the loop's `attempt` does, for the dry run.
pub fn config_for(task: &Task, repo: &Path) -> Result<Config> {
    let path = task.overrides.as_ref().and_then(|o| o.config.as_deref());
    let (mut cfg, _) = Config::discover(repo, path)?;
    if let Some(o) = &task.overrides {
        o.apply(&mut cfg);
    }
    if task.solo {
        cfg.graph.candidates = 1;
    }
    Ok(cfg)
}

/// How a followed task ended up.
#[derive(Debug)]
pub struct Followed {
    /// The task as last read.
    pub task: Task,
    /// False when `max_wait` ran out first: the loop still owns the task and
    /// nothing is known of its outcome yet.
    pub finished: bool,
}

/// Watch `id` until the loop that owns it is done with it, calling `seen` with
/// each run id the first time it appears on the task, and `progress` with a
/// line whenever the newest run's status changes.
///
/// Finished is `done`, `held` or `blocked`: a `failed` or `queued` task is the
/// loop's to retry. A task that stays unfinished while no loop is alive, or
/// past `max_wait`, is an error - never taken over here, because the loop may
/// merely be slow to heartbeat and a second driver would race it. Recovery is
/// the loop's own claim reclaim and `Runner::resume`. Running out of
/// `max_wait` is not an error: it returns with `finished: false`.
pub async fn follow(
    queue: &Queue,
    home: &Path,
    id: &str,
    poll: Duration,
    max_wait: Option<Duration>,
    mut seen: impl FnMut(&str),
    mut progress: impl FnMut(&str),
) -> Result<Followed> {
    let began = Instant::now();
    let mut announced = 0usize;
    let mut last_status = String::new();
    loop {
        let task = queue.get(id)?;
        for run in task.runs.iter().skip(announced) {
            seen(run);
        }
        announced = task.runs.len();
        if let Some(run) = task.runs.last()
            && crate::run::try_home().is_some()
            && let Ok(state) = crate::run::RunState::load(run)
        {
            let line = format!("run {}: {}", state.short(), state.status.as_str());
            if line != last_status {
                progress(&line);
                last_status = line;
            }
        }
        if matches!(
            task.status,
            TaskStatus::Done | TaskStatus::Held | TaskStatus::Blocked
        ) {
            return Ok(Followed {
                task,
                finished: true,
            });
        }
        let reading = crate::daemon::read_status(home);
        if crate::daemon::foreign_loop(reading.as_ref(), Timestamp::now(), std::process::id())
            .is_none()
        {
            bail!(
                "no magi loop is serving the queue any more, and task {} is still {}; it was \
                 not taken over here (the loop may only be slow to heartbeat). Start `magi \
                 serve` to carry on, or follow it with `magi task show {}`",
                task.short(),
                task.status.as_str(),
                task.short()
            );
        }
        if max_wait.is_some_and(|m| began.elapsed() >= m) {
            return Ok(Followed {
                task,
                finished: false,
            });
        }
        tokio::time::sleep(poll).await;
    }
}

/// A run opened inside this process (the follow-up review of `magi fix`) and
/// the task that owns it. See [`adopt`].
#[derive(Debug)]
pub struct Adopted {
    queue: Queue,
    /// The task this process claimed and started for the run; `None` when the
    /// claim could not be taken and the run was only linked.
    task: Option<Task>,
    claim: Option<Claim>,
    quota_before: Vec<crate::run::QuotaLoss>,
}

/// Give a run this process is about to execute an owning task. A parent task
/// that exists and is queued, failed or held is claimed, started with the
/// run and settled by [`Adopted::finish`] exactly like an ownerless run's; if
/// somebody else holds its claim, or the task is Done, blocked or running, the run is only linked and nothing is settled. With no such
/// parent, a task is filed and claimed for the duration. A no-op (`None`) when
/// no magi home is pinned.
pub fn adopt(state: &crate::run::RunState, parent_task: Option<&str>) -> Option<Adopted> {
    crate::run::try_home()?;
    adopt_in(Queue::open(), state, parent_task)
}

fn adopt_in(
    queue: Queue,
    state: &crate::run::RunState,
    parent_task: Option<&str>,
) -> Option<Adopted> {
    if let Some(parent) = parent_task
        && let Ok(id) = queue.resolve_id(parent)
    {
        // The parent exists: never file a second owner for this run.
        return match queue.claim(&id) {
            Ok(claim) => {
                let started = queue.get(&id).and_then(|mut task| {
                    // A task the daemon could pick up, or one a failed
                    // hand-started run left held (that is what a requested
                    // follow-up review re-verifies), is this run's to settle.
                    // A Done, blocked or running one is settled already or
                    // owned by somebody else: restarting it would let a
                    // follow-up review overwrite its outcome, so it only
                    // gains the run.
                    if !(task.status.runnable() || task.status == TaskStatus::Held) {
                        return Ok(None);
                    }
                    task.start(state.id.clone());
                    queue.put(&mut task)?;
                    Ok(Some(task))
                });
                match started {
                    Ok(None) => {
                        drop(claim);
                        let _ = queue.link_run(&id, &state.id);
                        None
                    }
                    Ok(Some(task)) => Some(Adopted {
                        queue,
                        task: Some(task),
                        claim: Some(claim),
                        quota_before: state.quota.clone(),
                    }),
                    Err(e) => {
                        tracing::warn!("could not start task {id} for run {}: {e:#}", state.id);
                        drop(claim);
                        let _ = queue.link_run(&id, &state.id);
                        None
                    }
                }
            }
            Err(e) => {
                // Another driver owns the task; do not settle over its result.
                tracing::warn!("task {id} is claimed elsewhere ({e:#}); linking run only");
                let _ = queue.link_run(&id, &state.id);
                None
            }
        };
    }
    let mut task = Task::new(
        crate::queue::title_from(&state.instruction, 72),
        state.instruction.clone(),
        state.repo.clone(),
        Source::Human,
    );
    let claim = queue.claim(&task.id).ok()?;
    task.start(state.id.clone());
    queue.put(&mut task).ok()?;
    Some(Adopted {
        queue,
        task: Some(task),
        claim: Some(claim),
        quota_before: state.quota.clone(),
    })
}

impl Adopted {
    /// Settle the task of an adopted ownerless run after it executed.
    pub fn finish(mut self, state: &crate::run::RunState, result: Result<()>) {
        if let Some(task) = &mut self.task {
            crate::daemon::finish_attempt(
                crate::daemon::Opts::default().max_attempts,
                &self.queue,
                task,
                state,
                &self.quota_before,
                result,
            );
            crate::daemon::hold_if_runnable(&self.queue, task);
        }
        drop(self.claim.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Status;

    fn filing(repo: &Path) -> Filing {
        Filing {
            instruction: "review the branch".to_owned(),
            title: "review x".to_owned(),
            repo: repo.to_path_buf(),
            source: Source::Human,
            solo: false,
            overrides: RunOverrides {
                merge: Some("none".to_owned()),
                ..RunOverrides::default()
            },
            review_of: Some("feat/x".to_owned()),
        }
    }

    fn heartbeat(home: &Path, pid: u32, age_secs: i64) {
        let mut status = Status::new();
        status.pid = pid;
        status.updated_at = Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_secs(age_secs))
            .unwrap();
        crate::daemon::write_status_to(&home.join("daemon.json"), &status).unwrap();
    }

    #[test]
    fn without_a_loop_the_task_is_claimed_before_anyone_else_can_take_it() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let filed = file(
            &q,
            dir.path(),
            Timestamp::now(),
            1,
            filing(dir.path()),
            false,
        )
        .unwrap();
        let Filed::Standalone { task, claim } = filed else {
            panic!("no loop is alive");
        };
        assert!(!task.urgent);
        assert_eq!(task.review_of.as_deref(), Some("feat/x"));
        assert_eq!(
            task.overrides.as_ref().unwrap().merge.as_deref(),
            Some("none")
        );
        assert!(
            q.claim(&task.id).is_err(),
            "a second process must not be able to claim a standalone run's task"
        );
        drop(claim);
        assert!(q.claim(&task.id).is_ok(), "released with the claim");
    }

    #[test]
    fn a_live_foreign_loop_gets_an_urgent_unclaimed_task() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        heartbeat(dir.path(), 4242, 0);
        let filed = file(
            &q,
            dir.path(),
            Timestamp::now(),
            1,
            filing(dir.path()),
            false,
        )
        .unwrap();
        let Filed::Daemon { task, pid } = filed else {
            panic!("a loop is alive");
        };
        assert_eq!(pid, Some(4242));
        assert!(task.urgent);
        let stored = q.get(&task.id).unwrap();
        assert_eq!(stored.status, TaskStatus::Queued);
        assert!(stored.runs.is_empty(), "nothing ran in this process");
        assert!(q.claim(&task.id).is_ok(), "and this process holds no claim");
    }

    #[test]
    fn a_stale_heartbeat_our_own_pid_and_a_dry_run_do_not_count_as_a_loop() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        heartbeat(dir.path(), 4242, 3600);
        assert!(matches!(
            file(
                &q,
                dir.path(),
                Timestamp::now(),
                1,
                filing(dir.path()),
                false
            )
            .unwrap(),
            Filed::Standalone { .. }
        ));
        heartbeat(dir.path(), 7, 0);
        assert!(matches!(
            file(
                &q,
                dir.path(),
                Timestamp::now(),
                7,
                filing(dir.path()),
                false
            )
            .unwrap(),
            Filed::Standalone { .. }
        ));
        heartbeat(dir.path(), 4242, 0);
        assert!(matches!(
            file(
                &q,
                dir.path(),
                Timestamp::now(),
                1,
                filing(dir.path()),
                true
            )
            .unwrap(),
            Filed::Standalone { .. }
        ));
    }

    #[tokio::test]
    async fn following_gives_up_on_a_task_nobody_is_serving() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut t = filing(dir.path()).into_task();
        q.put(&mut t).unwrap();
        let err = follow(
            &q,
            dir.path(),
            &t.id,
            Duration::from_millis(5),
            None,
            |_| {},
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("no magi loop"), "{err}");
        assert!(q.claim(&t.id).is_ok(), "never taken over");
    }

    #[tokio::test]
    async fn following_returns_when_the_loop_finishes_the_task_and_times_out_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        heartbeat(dir.path(), 4242, 0);
        let mut t = filing(dir.path()).into_task();
        q.put(&mut t).unwrap();
        let waited = follow(
            &q,
            dir.path(),
            &t.id,
            Duration::from_millis(5),
            Some(Duration::from_millis(30)),
            |_| {},
            |_| {},
        )
        .await
        .unwrap();
        assert!(!waited.finished, "the wait ran out, the loop still owns it");
        t.link_run("20260101-000000-abcd");
        t.succeed();
        q.put(&mut t).unwrap();
        let mut runs = Vec::new();
        let done = follow(
            &q,
            dir.path(),
            &t.id,
            Duration::from_millis(5),
            Some(Duration::from_secs(5)),
            |r| runs.push(r.to_owned()),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(done.task.status, TaskStatus::Done);
        assert_eq!(runs, ["20260101-000000-abcd"]);
    }

    fn parent_in(q: &Queue, dir: &Path) -> Task {
        let mut t = filing(dir).into_task();
        q.put(&mut t).unwrap();
        t
    }

    fn follow_up_state(dir: &Path) -> crate::run::RunState {
        crate::run::RunState::new(
            dir.to_path_buf(),
            "main".to_owned(),
            "abc1234".to_owned(),
            "review the branch".to_owned(),
            Config::default(),
        )
    }

    #[test]
    fn an_adopted_review_claims_starts_and_settles_a_runnable_task() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let parent = parent_in(&q, dir.path());
        let state = follow_up_state(dir.path());
        let adopted = adopt_in(q.clone(), &state, Some(&parent.id)).expect("adopted");
        let running = q.get(&parent.id).unwrap();
        assert_eq!(running.status, TaskStatus::Running);
        assert_eq!(running.attempts, parent.attempts + 1);
        assert_eq!(running.runs, std::slice::from_ref(&state.id));
        assert!(q.claim(&parent.id).is_err(), "exclusive while it runs");
        adopted.finish(&state, Err(anyhow::anyhow!("boom")));
        let settled = q.get(&parent.id).unwrap();
        assert_eq!(settled.status, TaskStatus::Held, "{settled:?}");
        assert_eq!(
            settled.runs,
            std::slice::from_ref(&state.id),
            "no duplicate run"
        );
        assert!(q.claim(&parent.id).is_ok(), "claim released");
        assert_eq!(q.list().len(), 1, "no second owner was filed");
    }

    #[test]
    fn a_task_claimed_elsewhere_only_gains_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let parent = parent_in(&q, dir.path());
        let _theirs = q.claim(&parent.id).unwrap();
        let state = follow_up_state(dir.path());
        assert!(adopt_in(q.clone(), &state, Some(&parent.id)).is_none());
        let after = q.get(&parent.id).unwrap();
        assert_eq!(after.status, parent.status);
        assert_eq!(after.attempts, parent.attempts);
        assert_eq!(after.runs, std::slice::from_ref(&state.id));
        assert_eq!(q.list().len(), 1, "no second owner was filed");
    }

    #[test]
    fn a_done_task_only_gains_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut parent = parent_in(&q, dir.path());
        parent.succeed();
        q.put(&mut parent).unwrap();
        let state = follow_up_state(dir.path());
        assert!(adopt_in(q.clone(), &state, Some(&parent.id)).is_none());
        let after = q.get(&parent.id).unwrap();
        assert_eq!(after.status, TaskStatus::Done);
        assert_eq!(after.attempts, parent.attempts);
        assert_eq!(after.runs, std::slice::from_ref(&state.id));
        assert!(q.claim(&parent.id).is_ok(), "claim released");
    }

    #[test]
    fn a_held_task_is_claimed_and_settled_by_its_follow_up_review() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut parent = parent_in(&q, dir.path());
        parent.hold_manual(Some("the run did not finish: stale".to_owned()));
        q.put(&mut parent).unwrap();
        let state = follow_up_state(dir.path());
        let adopted = adopt_in(q.clone(), &state, Some(&parent.id)).expect("adopted");
        assert_eq!(q.get(&parent.id).unwrap().status, TaskStatus::Running);
        assert!(q.claim(&parent.id).is_err(), "claimed while it runs");
        adopted.finish(&state, Err(anyhow::anyhow!("fresh failure")));
        let after = q.get(&parent.id).unwrap();
        assert_eq!(after.status, TaskStatus::Held);
        assert!(
            after
                .hold_reason
                .as_deref()
                .unwrap()
                .contains("fresh failure"),
            "{after:?}"
        );
        assert_eq!(after.runs, std::slice::from_ref(&state.id));
        assert!(q.claim(&parent.id).is_ok(), "claim released");
    }
}
