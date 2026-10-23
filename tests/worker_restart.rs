//! Real CLI processes: replacing dispatchers must not replace paid executions.

mod common;

use std::fs::File;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use common::{Judges, SeatBlock, fixture, home_lock, magi_cli};
use magi::queue::{Queue, RunOverrides, Source, Task, TaskStatus};
use magi::run::RunState;

struct Process(Child);

impl Process {
    fn stop(&mut self) {
        self.0.kill().expect("stop dispatcher, not its workers");
        self.0.wait().expect("reap dispatcher");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn configure(fx: &mut common::Fixture) -> std::path::PathBuf {
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    fx.config.disk.min_free_bytes = 0;
    fx.config.disk.auto_fold = false;
    fx.config.disk.cache_limit_bytes = 0;
    fx.config.daemon.max_concurrent_runs = 1;
    fx.config.daemon.max_deputies = 0;
    fx.config.daemon.release_stall_minutes = 0;
    fx.config.repos.fetch_interval = 0;
    let roster: Vec<_> = fx.config.agents.iter().map(|a| a.id.clone()).collect();
    fx.config.roles.implementers = roster.clone();
    fx.config.roles.reviewers = roster.clone();
    fx.config.roles.judges = roster;
    let mut conductor = fx.config.agents[0].clone();
    conductor.id = "neutral-conductor".into();
    conductor.command = vec![
        "sh".into(),
        "-c".into(),
        "printf '%s\\n' '{\"decisions\":[]}'".into(),
    ];
    fx.config.roles.conductor = Some(conductor.id.as_str().into());
    fx.config.agents.push(conductor);
    let config = fx.tmp.path().join("worker-test.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    config
}

fn scheduler(home: &Path, repo: &Path, config: &Path, log: &Path) -> Process {
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

fn file(queue: &Queue, repo: &Path, config: &Path, title: &str) -> Task {
    let mut task = Task::new(
        title.into(),
        "create note.txt".into(),
        repo.into(),
        Source::Human,
    );
    task.solo = true;
    task.overrides = Some(RunOverrides {
        config: Some(config.into()),
        merge: Some("none".into()),
        ..RunOverrides::default()
    });
    queue.put(&mut task).unwrap();
    task
}

common::e2e! {
async fn scheduler_restart_keeps_the_same_worker_and_occupied_slot() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let block = SeatBlock::install(&mut fx, "impl-A", "implement");
    let config = configure(&mut fx);
    let home = magi::run::home();
    let queue = Queue::open();
    let first = file(&queue, &fx.repo, &config, "first task");
    let mut old = scheduler(&home, &fx.repo, &config, &fx.tmp.path().join("old.log"));
    wait_for(|| block.started_path().exists(), "the first agent to start").await;
    let before = queue.get(&first.id).unwrap();
    assert_eq!(before.attempts, 1);
    let run_id = before.runs[0].clone();
    let running = RunState::load(&run_id).unwrap();
    let worker_pid = running.driver_pid.expect("execution belongs to a worker");
    assert_ne!(worker_pid, old.0.id());
    let generation = magi::worker::active(&home).unwrap().into_iter()
        .find(|j| j.kind == "task").expect("live task reservation").generation;

    old.stop();
    assert!(queue.claim(&first.id).is_err(), "worker, not scheduler, holds the task");
    std::fs::remove_file(home.join("daemon.json")).unwrap();
    let followed = magi::direct::follow(
        &queue, &home, &first.id, Duration::from_millis(20),
        Some(Duration::from_millis(50)), |_| {}, |_| {},
    ).await.expect("a live worker remains followable without a scheduler");
    assert!(!followed.finished, "the blocked execution has not finished");
    let second = file(&queue, &fx.repo, &config, "second task");
    let new = scheduler(&home, &fx.repo, &config, &fx.tmp.path().join("new.log"));
    wait_for(|| magi::daemon::read_status(&home).is_some_and(|r| {
        r.pid == Some(new.0.id()) && r.polls >= 3
            && r.current.iter().any(|c| c.task == first.id)
    }), "new scheduler to adopt the occupied execution slot").await;
    let still_running = RunState::load(&run_id).unwrap();
    assert_eq!(still_running.driver_pid, Some(worker_pid));
    assert!(!still_running.parked);
    assert_eq!(queue.get(&second.id).unwrap().attempts, 0,
        "restart must not give a second task the occupied ordinary slot");
    assert!(magi::worker::active(&home).unwrap().iter().any(|j|
        j.kind == "task" && j.pid == Some(worker_pid) && j.generation == generation));

    block.release();
    wait_for(|| queue.get(&first.id).unwrap().status == TaskStatus::Done
        && queue.get(&second.id).unwrap().status == TaskStatus::Done,
        "both independent executions to finish").await;
    let finished = queue.get(&first.id).unwrap();
    assert_eq!(finished.attempts, 1);
    assert_eq!(finished.runs, vec![run_id.clone()]);
    assert_eq!(finished.passes.len(), 1, "restart is not another execution pass");
    let state = RunState::load(&run_id).unwrap();
    assert!(!state.parked);
    assert_eq!(state.passes.len(), 1);
}
}

common::e2e! {
async fn standalone_execution_finishes_after_its_following_cli_exits() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let block = SeatBlock::install(&mut fx, "impl-A", "implement");
    let config = configure(&mut fx);
    let home = magi::run::home();
    let output = File::create(fx.tmp.path().join("standalone.log")).unwrap();
    let mut caller = Process(magi_cli(&home)
        .args(["run", "--repo"]).arg(&fx.repo)
        .arg("--config").arg(&config)
        .args(["--implementers", "1", "--merge", "none", "create note.txt"])
        .stdout(output.try_clone().unwrap()).stderr(output)
        .stdin(Stdio::null()).spawn().expect("start standalone run"));
    wait_for(|| block.started_path().exists(), "standalone implementation").await;
    let queue = Queue::open();
    let task = queue.list().into_iter().next().expect("run filed its task");
    let state = RunState::load(&task.runs[0]).unwrap();
    assert_ne!(state.driver_pid, Some(caller.0.id()));
    caller.stop();
    assert!(queue.claim(&task.id).is_err());
    block.release();
    wait_for(|| queue.get(&task.id).unwrap().status == TaskStatus::Done,
        "worker completion without a scheduler or following CLI").await;
    let finished = queue.get(&task.id).unwrap();
    assert_eq!(finished.attempts, 1);
    assert_eq!(finished.runs, task.runs);
    assert_eq!(finished.passes.len(), 1);
}
}
