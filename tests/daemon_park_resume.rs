//! A task whose last run was parked at a node boundary is resumed by the
//! daemon on its own - never held for a human to run `magi resume`.
//!
//! The run is parked for real: a judge seat is blocked on a file marker, the
//! park is requested while its call is in flight, and the graph stops right
//! after `judge`. The task then sits `failed` with its attempt refunded, the
//! way `daemon::settle` leaves it. The conductor is rigged to hold every task
//! it is shown, so the test fails if the daemon still hands a parked task to
//! it before `attempt` resumes the run.

mod common;

use std::time::Duration;

use common::{Judges, fixture, home_lock};
use magi::daemon::{self, Opts};
use magi::graph::{Pause, Runner};
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{RunState, RunStatus};

common::e2e! {
async fn a_run_parked_after_judging_is_resumed_by_the_daemon_without_an_operator() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.graph.reviewers = 1;
    fx.config.disk.min_free_bytes = 0;
    fx.config.disk.auto_fold = false;
    fx.config.disk.cache_limit_bytes = 0;

    let block_dir = fx.tmp.path().join("block");
    std::fs::create_dir_all(&block_dir).expect("block dir");
    let conduct_log = fx.tmp.path().join("conduct.log");
    for a in &mut fx.config.agents {
        a.env.insert("MOCK_BLOCK_SEAT".to_owned(), "judge-1".to_owned());
        a.env.insert("MOCK_BLOCK_NODE".to_owned(), "judge".to_owned());
        a.env.insert(
            "MOCK_BLOCK_DIR".to_owned(),
            block_dir.to_string_lossy().into_owned(),
        );
        a.env.insert(
            "MOCK_CONDUCT_LOG".to_owned(),
            conduct_log.to_string_lossy().into_owned(),
        );
    }

    // Park a real run while a judge call is in flight.
    let pause = Pause::new();
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.on_pause(pause.clone());
    let started = block_dir.join("started-judge-1");
    let release = block_dir.join("release-judge-1");
    let interrupter = tokio::spawn({
        let pause = pause.clone();
        async move {
            let mut seen = false;
            for _ in 0..1500 {
                if started.exists() {
                    seen = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(seen, "the mock never signalled that judge-1 had started");
            pause.park();
            std::fs::write(&release, b"go").expect("release judge-1");
        }
    });
    runner.execute().await.expect("execute parks after judge");
    interrupter.await.expect("interrupter");
    let run_id = runner.state.id.clone();
    let judged = runner.state.judgements.len();
    assert!(runner.state.parked);
    assert_eq!(runner.state.status, RunStatus::Judging);
    assert!(judged > 0, "the judging node finished and recorded its work");
    drop(runner);

    // The task as `settle` leaves a parked run: failed, attempt refunded.
    let queue = Queue::open();
    let mut task = Task::new(
        "parked task".to_owned(),
        "create note.txt".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    task.start(run_id.clone());
    task.stall("parked after `judging` - resume to carry on from here");
    queue.put(&mut task).expect("file the task");

    let config_path = fx.tmp.path().join("magi.toml");
    std::fs::write(&config_path, toml::to_string(&fx.config).unwrap()).unwrap();
    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config_path),
        once: true,
        poll: Duration::from_millis(20),
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    tokio::time::timeout(Duration::from_secs(120), daemon::serve(opts))
        .await
        .expect("the daemon drained in time")
        .expect("serve");

    let after = queue.get(&task.id).unwrap();
    assert_ne!(after.status, TaskStatus::Held, "held: {:?}", after.hold_reason);
    // `Task::start` records the resumed run again; a re-competition would
    // have minted a different id.
    assert!(
        after.runs.iter().all(|r| r == &run_id),
        "the run was resumed, not re-competed: {:?}",
        after.runs
    );
    assert_eq!(
        after.status,
        TaskStatus::Done,
        "the resumed run finished: {:?}",
        after.last_error
    );
    assert!(
        !conduct_log.exists(),
        "the conductor was consulted about a task that only awaits its resume"
    );

    let state = RunState::load(&run_id).unwrap();
    assert!(!state.parked, "the resume clears the park");
    assert_ne!(state.status, RunStatus::Judging, "it carried on past judging");
    assert_eq!(state.judgements.len(), judged, "judging was not run again");
}
}
