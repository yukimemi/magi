//! `magi task add --urgent` (`Task::urgent`) must dispatch through an extra,
//! one-shot concurrency slot the moment the task is runnable, alongside
//! whatever `magi serve` already has in flight — never by pausing or waiting
//! for that other run to free its own ordinary slot. An ordinary
//! (non-urgent) task must see no change at all: it still waits for the
//! ordinary `[daemon] max_concurrent_runs` pool exactly as before.
//!
//! Both scenarios block a genuinely-running ordinary task's implement call on
//! a file marker (`common`'s `MOCK_BLOCK_SEAT`/`MOCK_BLOCK_DIR`, the same
//! mechanism `graph_park.rs` uses) rather than a fixed sleep, so the test
//! proves the second task's dispatch decision against a run that is actually
//! in flight and finishes in milliseconds once released.

mod common;

use std::time::{Duration, Instant};

use common::{Judges, fixture, home_lock};
use magi::daemon::{self, Opts, Stop};
use magi::queue::{Queue, Source, Task, TaskStatus};

/// Write `config` to a `magi.toml` under `dir` and hand back the path, so
/// `Opts::config` can point the daemon at it explicitly rather than at
/// whatever layer stack `fx.repo` would otherwise resolve.
fn write_config(dir: &std::path::Path, config: &magi::config::Config) -> std::path::PathBuf {
    let path = dir.join("magi.toml");
    std::fs::write(&path, toml::to_string(config).expect("serialize config")).unwrap();
    path
}

/// One implementer, one reviewer, every housekeeping pass off — the
/// scheduling contract under test is between two tasks' dispatch, not the
/// panel or the janitor, and a bigger roster only adds process and worktree
/// churn while this test holds the process-wide test home lock.
fn daemon_config(fx: &common::Fixture) -> magi::config::Config {
    let mut config = fx.config.clone();
    config.graph.candidates = 1;
    config.graph.reviewers = 1;
    config.disk.min_free_bytes = 0;
    config.disk.auto_fold = false;
    config.disk.cache_limit_bytes = 0;
    config
}

/// Block the sole implementer seat (`impl-A`, regardless of candidate count
/// — see `graph_park.rs`) on a file marker until released.
fn block_impl_a(fx: &mut common::Fixture, block_dir: &std::path::Path) {
    std::fs::create_dir_all(block_dir).expect("block dir");
    for a in &mut fx.config.agents {
        a.env
            .insert("MOCK_BLOCK_SEAT".to_owned(), "impl-A".to_owned());
        a.env.insert(
            "MOCK_BLOCK_DIR".to_owned(),
            block_dir.to_string_lossy().into_owned(),
        );
    }
}

/// Poll `cond` until it is true, or panic naming `what` — bounded so a
/// regression fails this test fast instead of hanging the suite.
async fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
    let start = Instant::now();
    while !cond() {
        if start.elapsed() > timeout {
            panic!("timed out after {:?} waiting for: {what}", start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

const MARKER_WAIT: Duration = Duration::from_secs(60);

common::e2e! {
async fn an_urgent_task_starts_alongside_an_already_running_task() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let block_dir = fx.tmp.path().join("block");
    block_impl_a(&mut fx, &block_dir);
    let started_marker = block_dir.join("started-impl-A");
    let release_marker = block_dir.join("release-impl-A");

    let config = daemon_config(&fx);
    let config_path = write_config(fx.tmp.path(), &config);
    let queue = Queue::open();

    let mut ordinary = Task::new(
        "ordinary task".to_owned(),
        "create note.txt".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    queue.put(&mut ordinary).expect("file the ordinary task");

    let stop = Stop::new();
    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config_path),
        once: false,
        poll: Duration::from_millis(20),
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    let serve = tokio::spawn(daemon::serve_until(opts, stop.clone()));

    // The ordinary run's implement call is genuinely in flight, not merely
    // claimed - proven by the mock's own marker, the same proof
    // `graph_park.rs` uses.
    wait_until(
        || started_marker.exists(),
        MARKER_WAIT,
        "the ordinary task's implement seat never signalled it started",
    )
    .await;
    assert_eq!(
        queue.get(&ordinary.id).unwrap().status,
        TaskStatus::Running,
        "the ordinary task is claimed and running before the urgent task exists"
    );

    let mut urgent = Task::new(
        "urgent task".to_owned(),
        "create note.txt".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    urgent.urgent = true;
    queue.put(&mut urgent).expect("file the urgent task");

    // The urgent task dispatches through its own slot without waiting for the
    // ordinary run - still blocked on its own implement call - to finish.
    // `TaskStatus::Running` is written synchronously the moment `attempt`
    // claims and starts a run (`daemon::record`, right before `Runner::
    // execute` is even called) - unlike the daemon's own status file, which
    // is only refreshed on a multi-second heartbeat and so is not a fast or
    // reliable signal for a test to poll.
    wait_until(
        || {
            queue
                .get(&urgent.id)
                .is_ok_and(|t| t.status == TaskStatus::Running)
        },
        MARKER_WAIT,
        "the urgent task never joined the blocked ordinary run in flight",
    )
    .await;
    assert_eq!(
        queue.get(&ordinary.id).unwrap().status,
        TaskStatus::Running,
        "the existing run was not touched, let alone restarted, by the urgent dispatch"
    );
    assert_eq!(queue.get(&ordinary.id).unwrap().runs.len(), 1);

    std::fs::write(&release_marker, b"go").expect("release impl-A");

    wait_until(
        || {
            queue
                .get(&ordinary.id)
                .is_ok_and(|t| t.status == TaskStatus::Done)
                && queue
                    .get(&urgent.id)
                    .is_ok_and(|t| t.status == TaskStatus::Done)
        },
        MARKER_WAIT,
        "both tasks never reached a terminal status",
    )
    .await;

    stop.stop();
    tokio::time::timeout(MARKER_WAIT, serve)
        .await
        .expect("the daemon loop did not stop in time")
        .expect("the daemon task panicked")
        .expect("the daemon loop returned an error");

    let after_ordinary = queue.get(&ordinary.id).unwrap();
    assert_eq!(
        after_ordinary.status,
        TaskStatus::Done,
        "{after_ordinary:?}"
    );
    assert_eq!(
        after_ordinary.runs.len(),
        1,
        "the ordinary run was never recompeted, only ever the one run: {after_ordinary:?}"
    );

    let after_urgent = queue.get(&urgent.id).unwrap();
    assert_eq!(after_urgent.status, TaskStatus::Done, "{after_urgent:?}");
    assert_eq!(after_urgent.runs.len(), 1);
}
}

common::e2e! {
async fn an_ordinary_task_still_waits_for_the_ordinary_slot_to_free() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let block_dir = fx.tmp.path().join("block");
    block_impl_a(&mut fx, &block_dir);
    let started_marker = block_dir.join("started-impl-A");
    let release_marker = block_dir.join("release-impl-A");

    let config = daemon_config(&fx);
    let config_path = write_config(fx.tmp.path(), &config);
    let queue = Queue::open();

    let mut ordinary = Task::new(
        "ordinary task".to_owned(),
        "create note.txt".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    queue.put(&mut ordinary).expect("file the ordinary task");

    let stop = Stop::new();
    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config_path),
        once: false,
        poll: Duration::from_millis(20),
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    let serve = tokio::spawn(daemon::serve_until(opts, stop.clone()));

    wait_until(
        || started_marker.exists(),
        MARKER_WAIT,
        "the ordinary task's implement seat never signalled it started",
    )
    .await;

    // No `--urgent` this time: this task must sit `Queued` behind the
    // ordinary pool exactly as it always has, unaffected by the urgent slot
    // this change adds.
    let mut plain = Task::new(
        "second ordinary task".to_owned(),
        "create note.txt".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    queue.put(&mut plain).expect("file the second task");

    // Give the loop several ordinary poll intervals to prove it is *not*
    // starting the second task, not merely that it has not started it yet.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            queue.get(&plain.id).unwrap().status,
            TaskStatus::Queued,
            "an ordinary task must not dispatch while the one ordinary slot \
             is already spent on the blocked task"
        );
    }

    std::fs::write(&release_marker, b"go").expect("release impl-A");

    wait_until(
        || {
            queue
                .get(&ordinary.id)
                .is_ok_and(|t| t.status == TaskStatus::Done)
        },
        MARKER_WAIT,
        "the ordinary task never finished",
    )
    .await;

    // Only now, with the ordinary slot free, does the second task get its
    // turn.
    wait_until(
        || {
            queue
                .get(&plain.id)
                .is_ok_and(|t| t.status == TaskStatus::Done)
        },
        MARKER_WAIT,
        "the second ordinary task never started once the slot freed",
    )
    .await;

    stop.stop();
    tokio::time::timeout(MARKER_WAIT, serve)
        .await
        .expect("the daemon loop did not stop in time")
        .expect("the daemon task panicked")
        .expect("the daemon loop returned an error");

    let after_ordinary = queue.get(&ordinary.id).unwrap();
    assert_eq!(
        after_ordinary.status,
        TaskStatus::Done,
        "{after_ordinary:?}"
    );
    let after_plain = queue.get(&plain.id).unwrap();
    assert_eq!(after_plain.status, TaskStatus::Done, "{after_plain:?}");
}
}
