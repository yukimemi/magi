//! One long deputy must not occupy the entire deputy service across restarts.
mod common;
#[path = "deputy_worker_restart/diagnostics.rs"]
mod diagnostics;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Duration;

use common::{Judges, fixture, home_lock, magi_cli};
use magi::ask::{Deputy, Question, Questions};

struct Scheduler(Child);
impl Scheduler {
    fn stop(&mut self) {
        self.0.kill().expect("stop scheduler, not deputy workers");
        self.0.wait().expect("reap scheduler");
    }
}
impl Drop for Scheduler {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Release {
    home: PathBuf,
    gate: PathBuf,
}
impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gate, "go");
        for _ in 0..100 {
            if magi::worker::active(&self.home)
                .is_ok_and(|jobs| jobs.iter().all(|job| job.kind != "deputy"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn scheduler(home: &Path, repo: &Path, config: &Path, log: &Path) -> Scheduler {
    let output = File::create(log).unwrap();
    Scheduler(
        magi_cli(home)
            .args(["serve", "--poll", "1", "--repo"])
            .arg(repo)
            .arg("--config")
            .arg(config)
            .args(["--merge", "none"])
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .expect("start real scheduler"),
    )
}

async fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn question(store: &Questions, repo: &Path, run: &str, agent: &str, title: &str) -> Question {
    let mut q = Question::new(
        run.into(),
        magi::conduct::NODE.into(),
        "conduct".into(),
        title.into(),
        "manual setup question".into(),
        vec!["operator: ready".into()],
    );
    q.cwd = Some(repo.display().to_string());
    q.answer_timeout = 3600;
    let mut deputy = Deputy::new("Read the owner's setup answer.".into());
    deputy.agent = agent.to_owned();
    q.deputy = Some(deputy);
    store.put(&mut q).unwrap();
    q
}

common::e2e! {
async fn a_restarted_scheduler_admits_another_deputy_and_keeps_the_global_limit() {
    let home = home_lock().await;
    let mut scenario = diagnostics::Scenario::new(fixture(home, Judges::Unanimous, false));
    let home = magi::run::home();
    let state = scenario.fx.tmp.path().join("deputy-state");
    std::fs::create_dir_all(&state).unwrap();
    let release = state.join("release");
    scenario.release = Some(Release { home: home.clone(), gate: release.clone() });
    let script = scenario.fx.tmp.path().join("deputy.sh");
    std::fs::write(&script, r#"#!/bin/sh
set -eu
: > "$DEPUTY_STATE/$MAGI_SEAT.started"
while [ ! -f "$DEPUTY_STATE/release" ] && [ ! -f "$DEPUTY_STATE/$MAGI_SEAT.release" ]; do sleep 0.05; done
printf 'setup conversation finished\n'
"#).unwrap();
    for agent in &mut scenario.fx.config.agents {
        agent.command = vec!["sh".into(), script.display().to_string()];
        agent.env.insert("DEPUTY_STATE".into(), state.display().to_string());
    }
    let deputy_agent = scenario.fx.config.agents[0].id.clone();
    let mut conductor = scenario.fx.config.agents[0].clone();
    conductor.id = "neutral-conductor".into();
    conductor.command = vec!["sh".into(), "-c".into(), "printf '%s\\n' '{\"decisions\":[]}'".into()];
    scenario.fx.config.roles.conductor = Some(conductor.id.as_str().into());
    scenario.fx.config.agents.push(conductor);
    scenario.fx.config.graph.sessions = false;
    scenario.fx.config.daemon.max_deputies = 2;
    scenario.fx.config.daemon.release_stall_minutes = 0;
    scenario.fx.config.repos.fetch_interval = 0;
    scenario.fx.config.disk.min_free_bytes = 0;
    scenario.fx.config.disk.auto_fold = false;
    let config = scenario.fx.tmp.path().join("deputy-config.toml");
    std::fs::write(&config, toml::to_string(&scenario.fx.config).unwrap()).unwrap();
    // Use a real standalone run: a missing task/run is intentionally held by
    // migration as an ownerless decision, not admitted to legacy deputy work.
    let head = std::process::Command::new("git").args(["rev-parse", "HEAD"])
        .current_dir(&scenario.fx.repo).output().unwrap();
    assert!(head.status.success());
    let mut run = magi::run::RunState::new(
        scenario.fx.repo.clone(), "main".into(),
        String::from_utf8(head.stdout).unwrap().trim().into(),
        "Standalone setup conversation".into(), scenario.fx.config.clone(),
    );
    run.origin = Some(magi::run::Origin::operator());
    run.reserve_under(&home).unwrap();
    let store = Questions::at(home.join("questions"));
    let first = question(&store, &scenario.fx.repo, &run.id, &deputy_agent, "first setup");
    scenario.questions.push(first.id.clone());
    let started = |q: &Question| state.join(format!("{}.started", magi::ask::deputy_seat_key(&q.id)));
    scenario.schedulers.push(scheduler(&home, &scenario.fx.repo, &config, &scenario.fx.tmp.path().join("old.log")));
    wait_for(|| started(&first).exists(), "first deputy invocation").await;
    let original = magi::worker::active(&home).unwrap().into_iter()
        .find(|job| job.key == format!("deputy:{}", first.id)).expect("independent first deputy");
    scenario.original = Some(original.clone());
    scenario.schedulers[0].stop();
    let second = question(&store, &scenario.fx.repo, &run.id, &deputy_agent, "second setup");
    scenario.questions.push(second.id.clone());
    scenario.schedulers.push(scheduler(&home, &scenario.fx.repo, &config, &scenario.fx.tmp.path().join("new.log")));
    let new_pid = scenario.schedulers[1].0.id();
    wait_for(|| started(&second).exists(), "second deputy while the first is still blocked").await;
    assert!(!release.exists());
    let third = question(&store, &scenario.fx.repo, &run.id, &deputy_agent, "third setup");
    scenario.questions.push(third.id.clone());
    let baseline = magi::daemon::read_status(&home).unwrap().polls;
    wait_for(|| magi::daemon::read_status(&home).is_some_and(|reading|
        reading.pid == Some(new_pid) && reading.polls >= baseline + 7), "several admission ticks at capacity").await;
    assert!(!started(&third).exists(), "max_deputies counts the old worker after restart");
    let active: Vec<_> = magi::worker::active(&home).unwrap().into_iter()
        .filter(|job| job.kind == "deputy").collect();
    assert_eq!(active.len(), 2);
    assert!(active.iter().any(|job| job.id == original.id && job.pid == original.pid));
    assert_eq!(store.get(&first.id).unwrap().deputy.unwrap().starts, 1);
    for q in [&first, &second] {
        store.update(&q.id, |saved| {
            saved.answer(magi::ask::Answer::Choice(saved.choices[0].clone()))
        }).unwrap();
        std::fs::write(state.join(format!("{}.release", magi::ask::deputy_seat_key(&q.id))), "go").unwrap();
    }
    wait_for(|| started(&third).exists(), "third deputy after a capacity slot is released").await;
    store.update(&third.id, |saved| {
        saved.answer(magi::ask::Answer::Choice(saved.choices[0].clone()))
    }).unwrap();
    std::fs::write(state.join(format!("{}.release", magi::ask::deputy_seat_key(&third.id))), "go").unwrap();
    wait_for(|| magi::worker::active(&home).unwrap().iter().all(|job| job.kind != "deputy"),
        "all settled question workers to finish").await;
    for q in [&first, &second, &third] {
        let deputy = store.get(&q.id).unwrap().deputy.unwrap();
        assert_eq!(deputy.starts, 1, "restart must not count another deputy start");
        assert!(deputy.seat.is_some());
        assert!(store.get(&q.id).unwrap().routed_task.is_none());
    }
    let imported = magi::task_inquiry::Outbox::at(&home).list().unwrap();
    assert!(imported.iter().all(|record| !scenario.questions.contains(&record.id)),
        "standalone deputy questions must retain legacy ownership across migration and restart");
}
}
