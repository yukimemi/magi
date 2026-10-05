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
}

/// Watch `id` until the loop that owns it is done with it, calling `seen` with
/// each run id the first time it appears on the task, and `progress` with a
/// line whenever the newest run's status changes.
///
/// Finished is `done`, `held` or `blocked`: a `failed` or `queued` task is the
/// loop's to retry. A task that stays unfinished while no loop is alive, or
/// past `max_wait`, is an error - never taken over here, because the loop may
/// merely be slow to heartbeat and a second driver would race it. Recovery is
/// the loop's own claim reclaim and `Runner::resume`.
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
            return Ok(Followed { task });
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
            bail!(
                "gave up following task {} ({}); it is still the loop's - see `magi task show {}`",
                task.short(),
                task.status.as_str(),
                task.short()
            );
        }
        tokio::time::sleep(poll).await;
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
        let err = follow(
            &q,
            dir.path(),
            &t.id,
            Duration::from_millis(5),
            Some(Duration::from_millis(30)),
            |_| {},
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("gave up"), "{err}");
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
}
