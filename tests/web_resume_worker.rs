//! Manual Web Resume uses the same durable task workers as the scheduler.
#![cfg(unix)]

mod common;

use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use common::{Judges, SeatBlock, fixture, home_lock, magi_cli};
use magi::graph::{Pause, Runner};
use magi::queue::{Queue, RunOverrides, Source, Task, TaskStatus};
use magi::run::RunState;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Web(Child);

impl Web {
    fn stop(&mut self) {
        self.0.kill().expect("stop only the web process");
        self.0.wait().expect("reap web process");
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Workers(std::path::PathBuf);

impl Drop for Workers {
    fn drop(&mut self) {
        let _ = magi::worker::request(&self.0, "scheduler", magi::worker::Control::Stop);
        if let Ok(jobs) = magi::worker::active(&self.0) {
            for job in jobs.into_iter().filter(|job| job.kind == "task") {
                let _ = magi::worker::request(&self.0, &job.key, magi::worker::Control::Park);
            }
        }
        for _ in 0..100 {
            if magi::worker::active(&self.0).is_ok_and(|jobs| {
                jobs.iter()
                    .all(|job| !matches!(job.kind.as_str(), "task" | "scheduler"))
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
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

async fn configure(fx: &mut common::Fixture) -> std::path::PathBuf {
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    fx.config.disk.min_free_bytes = 0;
    fx.config.disk.auto_fold = false;
    fx.config.disk.cache_limit_bytes = 0;
    fx.config.daemon.max_concurrent_runs = 1;
    fx.config.daemon.max_deputies = 0;
    fx.config.daemon.release_stall_minutes = 0;
    fx.config.repos.fetch_interval = 0;
    fx.config.update.mode = magi::config::UpdateMode::Off;
    let roster: Vec<_> = fx
        .config
        .agents
        .iter()
        .map(|agent| agent.id.clone())
        .collect();
    fx.config.roles.implementers = roster.clone();
    fx.config.roles.reviewers = roster.clone();
    fx.config.roles.judges = roster;
    let path = fx.repo.join("magi.toml");
    std::fs::write(&path, toml::to_string(&fx.config).unwrap()).unwrap();
    for args in [
        vec!["add", "magi.toml"],
        vec!["commit", "-m", "configure isolated resume worker"],
        vec!["push", "origin", "main"],
        vec!["fetch", "origin"],
    ] {
        magi::git::git(&fx.repo, &args)
            .await
            .expect("configure fixture repository");
    }
    path
}

async fn web(home: &Path, repo: &Path, log: &Path) -> (Web, SocketAddr) {
    let output = File::create(log).unwrap();
    let process = Web(magi_cli(home)
        .args(["web", "--bind", "127.0.0.1", "--port", "0", "--repo"])
        .arg(repo)
        .env("NO_COLOR", "1")
        .env("MAGI_NO_AUTOUPDATE", "1")
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .expect("start real web process"));
    let mut address = None;
    wait_for(
        || {
            address = std::fs::read_to_string(log).ok().and_then(|log| {
                let start = log.find("http://127.0.0.1:")? + "http://".len();
                let text: String = log[start..]
                    .chars()
                    .take_while(|ch| ch.is_ascii_digit() || matches!(ch, '.' | ':'))
                    .collect();
                text.parse().ok()
            });
            address.is_some()
        },
        "ephemeral web listener",
    )
    .await;
    (process, address.unwrap())
}

async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let body = body.map(|value| value.to_string()).unwrap_or_default();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response");
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        let value = serde_json::from_str(body).unwrap_or_else(|error| {
            panic!("invalid JSON: {error}: {response}")
        });
        (status, value)
    }).await.expect("HTTP request timeout")
}

common::e2e! {
async fn manual_web_resume_keeps_its_task_worker_after_web_replacement() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let home = magi::run::home();
    let _workers = Workers(home.clone());
    let block = SeatBlock::install(&mut fx, "impl-A", "implement");
    let config = configure(&mut fx).await;

    // Create a real, resumable run with prepared worktrees, before any agent
    // invocation. Resume must carry those same worktrees into a task worker.
    let mut runner = Runner::start(&fx.repo, "create note.txt".into(), fx.config.clone(),
        magi::run::Origin::operator()).await.expect("start real run");
    let pause = Pause::new();
    pause.park();
    runner.on_pause(pause);
    runner.execute().await.expect("park after prep");
    assert!(runner.state.parked);
    assert!(!block.started_path().exists());
    let run_id = runner.state.id.clone();
    drop(runner);

    let queue = Queue::open();
    let mut task = Task::new("manually resumed task".into(), "create note.txt".into(),
        fx.repo.clone(), Source::Human);
    task.solo = true;
    task.overrides = Some(RunOverrides {
        config: Some(config), merge: Some("none".into()), ..Default::default()
    });
    task.start(run_id.clone());
    task.stall("parked before implementation");
    queue.put(&mut task).unwrap();

    let (mut predecessor, address) = web(&home, &fx.repo, &fx.tmp.path().join("old-web.log")).await;
    let (status, body) = request(address, "POST", &format!("/api/runs/{run_id}/resume"), None).await;
    assert_eq!(status, 202, "{body}");
    wait_for(|| block.started_path().exists(), "resumed implementation worker").await;
    let before_task = queue.get(&task.id).unwrap();
    let before_state = RunState::load(&run_id).unwrap();
    let worker = magi::worker::active(&home).unwrap().into_iter()
        .find(|job| job.kind == "task" && job.key == format!("task:{}", task.id))
        .expect("manual resume published an ordinary task worker");
    assert_eq!(before_state.driver_pid, worker.pid);
    assert_ne!(worker.pid, Some(predecessor.0.id()));
    assert!(!before_state.parked);

    predecessor.stop();
    assert!(queue.claim(&task.id).is_err(), "web never owns the execution claim");
    let (_successor, address) = web(&home, &fx.repo, &fx.tmp.path().join("new-web.log")).await;
    let (status, health) = request(address, "GET", "/api/health", None).await;
    assert_eq!(status, 200, "{health}");
    assert!(health["jobs"].as_array().unwrap().iter().any(|job| {
        job["id"].as_str() == Some(worker.id.as_str())
            && job["generation"].as_str() == Some(worker.generation.as_str())
            && job["pid"].as_u64() == worker.pid.map(u64::from)
    }), "replacement web sees the original task worker: {health}");
    let still_running = queue.get(&task.id).unwrap();
    assert_eq!(still_running.attempts, before_task.attempts);
    assert_eq!(still_running.runs, before_task.runs);
    assert_eq!(still_running.passes.len(), before_task.passes.len());
    let state = RunState::load(&run_id).unwrap();
    assert_eq!(state.driver_pid, worker.pid);
    assert_eq!(state.passes.len(), before_state.passes.len());
    assert!(!state.parked);

    block.release();
    wait_for(|| queue.get(&task.id).unwrap().status == TaskStatus::Done,
        "the same resumed worker to finish").await;
    let finished = queue.get(&task.id).unwrap();
    assert_eq!(finished.runs, vec![run_id.clone()]);
    assert_eq!(finished.attempts, before_task.attempts);
    let state = RunState::load(&run_id).unwrap();
    assert_eq!(state.passes.len(), before_state.passes.len());
    assert!(!state.parked);
}
}

common::e2e! {
async fn replacement_web_controls_a_foreign_upgraded_scheduler() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let home = magi::run::home();
    let _workers = Workers(home.clone());
    configure(&mut fx).await;
    let (mut predecessor, address) = web(&home, &fx.repo, &fx.tmp.path().join("old-web.log")).await;
    let (status, body) = request(address, "POST", "/api/loop", Some(json!({"running": true}))).await;
    assert_eq!(status, 200, "{body}");
    wait_for(|| magi::worker::active(&home).is_ok_and(|jobs| jobs.iter().any(|job| {
        job.kind == "scheduler" && job.status == magi::worker::Status::Running
    })), "independent scheduler acknowledgement").await;
    let scheduler = magi::worker::active(&home).unwrap().into_iter()
        .find(|job| job.kind == "scheduler").expect("live scheduler worker");
    wait_for(|| magi::daemon::read_status(&home).is_some_and(|reading| {
        reading.pid == scheduler.pid && reading.worker_protocol == Some(1)
            && reading.running(jiff::Timestamp::now())
    }), "the actual upgraded scheduler heartbeat").await;
    predecessor.stop();

    let (successor, address) = web(&home, &fx.repo, &fx.tmp.path().join("new-web.log")).await;
    let (status, state) = request(address, "GET", "/api/loop", None).await;
    assert_eq!(status, 200, "{state}");
    assert_eq!(state["running"], true);
    assert_eq!(state["owned"], true);
    assert_ne!(state["daemon"]["pid"].as_u64(), Some(u64::from(successor.0.id())));
    assert_eq!(state["daemon"]["pid"].as_u64(), scheduler.pid.map(u64::from));
    // Updates are disabled before launch, so this only exercises the real
    // ownership gate. No release lookup, download, or executable swap occurs.
    let (status, update) = request(address, "POST", "/api/upgrade", None).await;
    assert_eq!(status, 200, "foreign upgraded schedulers are controllable: {update}");
    assert!(update["to"].is_null());
    let (status, body) = request(address, "POST", "/api/loop", Some(json!({"running": false}))).await;
    assert_eq!(status, 200, "{body}");
    wait_for(|| magi::worker::active(&home).is_ok_and(|jobs| {
        jobs.iter().all(|job| job.kind != "scheduler")
    }), "phone stop to finish the detached scheduler").await;
}
}
