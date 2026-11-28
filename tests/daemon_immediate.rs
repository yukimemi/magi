//! End-to-end scheduler tests for owner-selected immediate execution:
//! 1. Multiple immediate tasks dispatch on the next poll even when both the
//!    ordinary and urgent lanes are saturated, without parking or cancelling
//!    existing workers, while additional ordinary/urgent tasks stay queued.
//! 2. Replacing the scheduler process preserves live ordinary and immediate
//!    reservations, prevents duplicate execution, keeps ordinary concurrency
//!    bounded, and lets a new immediate task dispatch after restart.

mod common;

use std::fs::File;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use common::seat_block::{SeatBlock, task_ended, task_failed, wait_while_running};
use common::{Judges, fixture, home_lock, magi_cli};
use magi::daemon::{self, Opts, Stop};
use magi::queue::{Queue, RunOverrides, Source, Task, TaskStatus};
use magi::run::RunState;

fn write_config(dir: &Path, config: &magi::config::Config) -> std::path::PathBuf {
    let path = dir.join("magi.toml");
    std::fs::write(&path, toml::to_string(config).expect("serialize config")).unwrap();
    path
}

fn daemon_config(fx: &common::Fixture) -> magi::config::Config {
    let mut config = fx.config.clone();
    config.graph.implementers = 1;
    config.graph.reviewers = 1;
    config.disk.min_free_bytes = 0;
    config.disk.auto_fold = false;
    config.disk.cache_limit_bytes = 0;
    config.daemon.max_concurrent_runs = 1;
    config.daemon.max_deputies = 0;
    config.daemon.release_stall_minutes = 0;
    config.repos.fetch_interval = 0;
    let roster: Vec<_> = config.agents.iter().map(|a| a.id.clone()).collect();
    config.roles.implementers = roster.clone();
    config.roles.reviewers = roster.clone();
    config.roles.judges = roster;
    let mut conductor = config.agents[0].clone();
    conductor.id = "neutral-conductor".into();
    conductor.command = vec![
        "sh".into(),
        "-c".into(),
        "printf '%s\\n' '{\"decisions\":[]}'".into(),
    ];
    config.roles.conductor = Some(conductor.id.as_str().into());
    config.agents.push(conductor);
    config
}

fn file_task(
    queue: &Queue,
    repo: &Path,
    config: &Path,
    title: &str,
    urgent: bool,
    immediate: bool,
) -> Task {
    let mut task = Task::new(
        title.to_owned(),
        "create note.txt".to_owned(),
        repo.to_path_buf(),
        Source::Human,
    );
    task.solo = true;
    task.urgent = urgent;
    task.immediate = immediate;
    task.overrides = Some(RunOverrides {
        config: Some(config.to_path_buf()),
        merge: Some("none".into()),
        ..RunOverrides::default()
    });
    queue.put(&mut task).unwrap();
    task
}

common::e2e! {
async fn multiple_immediate_tasks_dispatch_while_ordinary_and_urgent_lanes_are_saturated() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let block = SeatBlock::install(&mut fx, "impl-A", "implement");
    let config = daemon_config(&fx);
    let config_path = write_config(fx.tmp.path(), &config);
    let queue = Queue::open();
    let magi_home = magi::run::home();

    // 1. Saturate the single ordinary lane.
    let ordinary_1 = file_task(&queue, &fx.repo, &config_path, "ordinary-1", false, false);

    let stop = Stop::new();
    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config_path.clone()),
        once: false,
        poll: Duration::from_millis(20),
        merge: Some("none".into()),
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    let mut serve = tokio::spawn(daemon::serve_until(opts, stop.clone()));

    block
        .started(&mut serve, || task_ended(&queue, &ordinary_1.id))
        .await;
    assert_eq!(queue.get(&ordinary_1.id).unwrap().status, TaskStatus::Running);

    // 2. Saturate the single urgent lane.
    let urgent_1 = file_task(&queue, &fx.repo, &config_path, "urgent-1", true, false);
    wait_while_running(
        &mut serve,
        || queue.get(&urgent_1.id).is_ok_and(|t| t.status == TaskStatus::Running),
        || task_ended(&queue, &ordinary_1.id).or_else(|| task_ended(&queue, &urgent_1.id)),
        "urgent-1 never joined ordinary-1 in flight",
    )
    .await;

    // 3. Queue a second ordinary and a second urgent task (both must wait)
    //    plus TWO immediate tasks (both must dispatch immediately!).
    let ordinary_2 = file_task(&queue, &fx.repo, &config_path, "ordinary-2", false, false);
    let urgent_2 = file_task(&queue, &fx.repo, &config_path, "urgent-2", true, false);
    let imm_1 = file_task(&queue, &fx.repo, &config_path, "immediate-1", false, true);
    let imm_2 = file_task(&queue, &fx.repo, &config_path, "immediate-2", true, true);

    wait_while_running(
        &mut serve,
        || {
            queue.get(&imm_1.id).is_ok_and(|t| t.status == TaskStatus::Running)
                && queue.get(&imm_2.id).is_ok_and(|t| t.status == TaskStatus::Running)
        },
        || {
            task_ended(&queue, &ordinary_1.id)
                .or_else(|| task_ended(&queue, &urgent_1.id))
                .or_else(|| task_ended(&queue, &imm_1.id))
                .or_else(|| task_ended(&queue, &imm_2.id))
        },
        "both immediate tasks never dispatched while ordinary and urgent lanes were saturated",
    )
    .await;

    // Existing ordinary and urgent runs remain untouched (not parked/cancelled/restarted).
    assert_eq!(queue.get(&ordinary_1.id).unwrap().status, TaskStatus::Running);
    assert_eq!(queue.get(&ordinary_1.id).unwrap().runs.len(), 1);
    assert_eq!(queue.get(&urgent_1.id).unwrap().status, TaskStatus::Running);
    assert_eq!(queue.get(&urgent_1.id).unwrap().runs.len(), 1);

    // Extra ordinary and urgent tasks MUST still be Queued while their lanes are full.
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            queue.get(&ordinary_2.id).unwrap().status,
            TaskStatus::Queued,
            "ordinary-2 must keep waiting for the occupied ordinary lane"
        );
        assert_eq!(
            queue.get(&urgent_2.id).unwrap().status,
            TaskStatus::Queued,
            "urgent-2 must keep waiting for the occupied urgent lane"
        );
    }

    // Verify persisted reservation lanes via shared read-only AdmissionSnapshot.
    let snap = daemon::AdmissionSnapshot::capture(&magi_home, jiff::Timestamp::now());
    let adm_imm1 = snap.evaluate(&queue.get(&imm_1.id).unwrap(), &queue, None, &magi_home);
    let adm_imm2 = snap.evaluate(&queue.get(&imm_2.id).unwrap(), &queue, None, &magi_home);
    let adm_ord1 = snap.evaluate(&queue.get(&ordinary_1.id).unwrap(), &queue, None, &magi_home);
    let adm_urg1 = snap.evaluate(&queue.get(&urgent_1.id).unwrap(), &queue, None, &magi_home);
    assert_eq!(adm_imm1.reservation_lane, Some("immediate"));
    assert_eq!(adm_imm2.reservation_lane, Some("immediate"));
    assert_eq!(adm_ord1.reservation_lane, Some("ordinary"));
    assert_eq!(adm_urg1.reservation_lane, Some("urgent"));

    block.release();

    let all_tasks = [&ordinary_1, &urgent_1, &ordinary_2, &urgent_2, &imm_1, &imm_2];
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            if all_tasks
                .iter()
                .all(|t| queue.get(&t.id).is_ok_and(|s| s.status == TaskStatus::Done))
            {
                break;
            }
            if let Some(reason) = all_tasks.iter().find_map(|t| task_failed(&queue, &t.id)) {
                panic!("task failed after release: {reason}");
            }
            tokio::select! {
                biased;
                out = &mut serve => panic!("daemon exited before tasks finished: {out:?}"),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        let states: Vec<_> = all_tasks
            .iter()
            .map(|t| {
                let s = queue.get(&t.id).ok();
                (
                    t.title.clone(),
                    s.as_ref().map(|t| t.status),
                    s.as_ref().and_then(|t| t.last_error.clone()),
                )
            })
            .collect();
        panic!("all six tasks never reached Done within 180s: {states:?}");
    });

    stop.stop();
    tokio::time::timeout(Duration::from_secs(60), serve)
        .await
        .expect("daemon did not stop in time")
        .expect("daemon task panicked")
        .expect("daemon returned error");

    for t in all_tasks {
        let saved = queue.get(&t.id).unwrap();
        assert_eq!(saved.status, TaskStatus::Done, "{saved:?}");
        assert_eq!(saved.runs.len(), 1, "no duplicate runs for {}", t.title);
    }
}
}

struct Process(Child);

impl Process {
    fn stop(&mut self) {
        self.0.kill().expect("stop dispatcher");
        self.0.wait().expect("reap dispatcher");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_scheduler(home: &Path, repo: &Path, config: &Path, log: &Path) -> Process {
    let output = File::create(log).unwrap();
    let child = magi_cli(home)
        .args(["serve", "--poll", "1", "--repo"])
        .arg(repo)
        .arg("--config")
        .arg(config)
        .arg("--merge")
        .arg("none")
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .expect("start real scheduler");
    Process(child)
}

async fn wait_for(mut predicate: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

common::e2e! {
async fn interrupt_rescans_after_first_ordinary_reservation() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let block = SeatBlock::install(&mut fx, "impl-A", "implement");
    let mut config = daemon_config(&fx);
    config.daemon.max_concurrent_runs = 2;
    config.daemon.pause_for_interrupts = true;
    let config_path = write_config(fx.tmp.path(), &config);
    let queue = Queue::open();
    let ordinary = file_task(&queue, &fx.repo, &config_path, "ordinary", false, false);
    let mut marked = file_task(&queue, &fx.repo, &config_path, "interrupt", false, false);
    marked.priority = ordinary.priority - 1;
    marked.interrupt = true;
    queue.put(&mut marked).unwrap();

    let magi_home = magi::run::home();
    let stop = Stop::new();
    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config_path),
        poll: Duration::from_millis(20),
        merge: Some("none".into()),
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    let serve = tokio::spawn(daemon::serve_until(opts, stop.clone()));
    let interrupt_state = magi_home.join("scheduler-interrupt.json");
    let marked_key = format!("task:{}", marked.id);
    wait_for(
        || {
            interrupt_state.exists()
                || magi::worker::active(&magi_home)
                    .unwrap()
                    .iter()
                    .any(|job| job.key == marked_key)
        },
        "interrupt sequence or premature interrupt dispatch",
    )
    .await;
    assert!(interrupt_state.exists(), "interrupt dispatched without a park sequence");
    block.release();
    wait_for(
        || [&ordinary, &marked].iter().all(|t| queue.get(&t.id).unwrap().status == TaskStatus::Done),
        "interrupt and resumed ordinary task to finish",
    )
    .await;
    stop.stop();
    tokio::time::timeout(Duration::from_secs(60), serve)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(queue.get(&ordinary.id).unwrap().runs.len(), 1);
}
}

common::e2e! {
async fn scheduler_restart_preserves_immediate_and_ordinary_lanes_without_duplication() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let block = SeatBlock::install(&mut fx, "impl-A", "implement");
    let config = daemon_config(&fx);
    let config_path = write_config(fx.tmp.path(), &config);
    let magi_home = magi::run::home();
    let queue = Queue::open();

    let ordinary = file_task(&queue, &fx.repo, &config_path, "ordinary-restart", false, false);
    let imm_1 = file_task(&queue, &fx.repo, &config_path, "immediate-restart-1", false, true);

    let mut old = spawn_scheduler(
        &magi_home,
        &fx.repo,
        &config_path,
        &fx.tmp.path().join("old-imm.log"),
    );

    let has_driver = |task_id: &str| {
        queue
            .get(task_id)
            .ok()
            .filter(|t| t.status == TaskStatus::Running)
            .and_then(|t| t.runs.first().cloned())
            .and_then(|run_id| RunState::load(&run_id).ok())
            .and_then(|state| state.driver_pid)
            .is_some()
    };

    wait_for(
        || block.started_path().exists() && has_driver(&ordinary.id) && has_driver(&imm_1.id),
        "both ordinary and immediate-1 drivers to start",
    )
    .await;

    let ord_run_id = queue.get(&ordinary.id).unwrap().runs[0].clone();
    let imm_run_id = queue.get(&imm_1.id).unwrap().runs[0].clone();
    let ord_pid = RunState::load(&ord_run_id).unwrap().driver_pid.unwrap();
    let imm_pid = RunState::load(&imm_run_id).unwrap().driver_pid.unwrap();

    // Kill the old scheduler; independent workers must keep their claims and reservations.
    old.stop();
    assert!(queue.claim(&ordinary.id).is_err());
    assert!(queue.claim(&imm_1.id).is_err());
    let _ = std::fs::remove_file(magi_home.join("daemon.json"));

    // File another ordinary task (must wait) and another immediate task (must dispatch after restart).
    let ordinary_after = file_task(&queue, &fx.repo, &config_path, "ordinary-after", false, false);
    let imm_2 = file_task(&queue, &fx.repo, &config_path, "immediate-restart-2", false, true);

    let new = spawn_scheduler(
        &magi_home,
        &fx.repo,
        &config_path,
        &fx.tmp.path().join("new-imm.log"),
    );

    wait_for(
        || {
            daemon::read_status(&magi_home).is_some_and(|r| {
                r.pid == Some(new.0.id())
                    && r.polls >= 2
                    && r.current.iter().any(|c| c.task == ordinary.id)
                    && r.current.iter().any(|c| c.task == imm_1.id)
            }) && queue.get(&imm_2.id).is_ok_and(|t| t.status == TaskStatus::Running)
        },
        "new scheduler to adopt both reservations and dispatch immediate-2",
    )
    .await;

    // Neither adopted task was duplicated or restarted, and ordinary_after still waits.
    assert_eq!(RunState::load(&ord_run_id).unwrap().driver_pid, Some(ord_pid));
    assert_eq!(RunState::load(&imm_run_id).unwrap().driver_pid, Some(imm_pid));
    assert_eq!(queue.get(&ordinary.id).unwrap().attempts, 1);
    assert_eq!(queue.get(&imm_1.id).unwrap().attempts, 1);
    assert_eq!(
        queue.get(&ordinary_after.id).unwrap().status,
        TaskStatus::Queued,
        "ordinary lane must remain bounded across scheduler replacement"
    );

    block.release();

    wait_for(
        || {
            [&ordinary, &imm_1, &ordinary_after, &imm_2]
                .iter()
                .all(|t| queue.get(&t.id).is_ok_and(|s| s.status == TaskStatus::Done))
        },
        "all tasks to finish cleanly after restart",
    )
    .await;

    for t in [&ordinary, &imm_1, &ordinary_after, &imm_2] {
        let saved = queue.get(&t.id).unwrap();
        assert_eq!(saved.attempts, 1);
        assert_eq!(saved.runs.len(), 1);
    }
}
}
