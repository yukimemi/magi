//! A task whose last run was parked at a node boundary is resumed by the
//! daemon on its own - never held for a human to run `magi resume`.
//!
//! The run is parked for real: a judge seat is blocked on a file marker, the
//! park is requested while its call is in flight, and the graph stops right
//! after `judge`. Upgrade parking records its cause and refunds the attempt;
//! the legacy fixture also covers a task left `failed` by older versions. The conductor is rigged to hold every task
//! it is shown, so the test fails if the daemon still hands a parked task to
//! it before `attempt` resumes the run.

mod common;

use std::time::Duration;

use common::{Judges, SeatBlock, fixture, home_lock};
use magi::daemon::{self, Opts};
use magi::graph::{Pause, Runner};
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{ParkCause, RunState, RunStatus};

common::e2e! {
async fn a_run_parked_after_judging_is_resumed_by_the_daemon_without_an_operator() {
    park_resume(false).await;
}
}

common::e2e! {
async fn upgrade_resume_precedes_newer_higher_priority_work_after_restart() {
    park_resume(true).await;
}
}

async fn park_resume(upgrade: bool) {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.graph.reviewers = 1;
    fx.config.daemon.max_concurrent_runs = 1;
    fx.config.disk.min_free_bytes = 0;
    fx.config.disk.auto_fold = false;
    fx.config.disk.cache_limit_bytes = 0;

    let block = SeatBlock::install(&mut fx, "judge-1", "judge");
    let conduct_log = fx.tmp.path().join("conduct.log");
    for a in &mut fx.config.agents {
        a.env.insert(
            "MOCK_CONDUCT_LOG".to_owned(),
            conduct_log.to_string_lossy().into_owned(),
        );
    }

    // Park a real run while a judge call is in flight.
    let pause = Pause::new();
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".to_owned(),
        fx.config.clone(),
        magi::run::Origin::operator(),
    )
    .await
    .expect("start");
    runner.on_pause(pause.clone());
    let mut exec = Box::pin(runner.execute());
    block.started(&mut exec, || None).await;
    if upgrade {
        pause.park_for(ParkCause::Upgrade {
            from: "old".into(),
            to: Some("new".into()),
        });
    } else {
        pause.park();
    }
    block.release();
    exec.await.expect("execute parks after judge");
    let run_id = runner.state.id.clone();
    let judged = runner.state.judgements.len();
    assert!(runner.state.parked);
    assert_eq!(runner.state.status, RunStatus::Judging);
    assert!(
        judged > 0,
        "the judging node finished and recorded its work"
    );
    let pass = runner.state.passes.last().unwrap().clone();
    let candidates = implementation_evidence(&runner.state);
    drop(runner);

    // Persist the upgrade settle evidence, or the legacy failed-task shape.
    let queue = Queue::open();
    let mut task = Task::new(
        "parked task".to_owned(),
        "create note.txt".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    task.start(run_id.clone());
    if upgrade {
        task.park("upgrade after judging");
        let cause = match pass.end {
            Some(magi::run::PassEnd::Parked { cause }) => cause,
            _ => panic!("upgrade must leave a parked pass"),
        };
        task.record_pass_at(&run_id, pass.index, Some(pass.started_at), 1, Some(cause));
    } else {
        task.stall("parked after `judging` - resume to carry on from here");
    }
    queue.put(&mut task).expect("file the task");

    let fresh = if upgrade {
        let mut fresh = Task::new(
            "fresh task".into(),
            "create note.txt".into(),
            fx.repo.clone(),
            Source::Human,
        );
        fresh.priority = 100;
        queue.put(&mut fresh).unwrap();
        Some(fresh)
    } else {
        None
    };
    // Re-open the durable store as the successor daemon does. No in-memory
    // scheduler state or manually increased priority carries the resume.
    let queue = Queue::at(queue.root().to_owned());
    if upgrade {
        assert_eq!(queue.next_runnable().unwrap().id, task.id);
    }

    if upgrade {
        // Fresh work legitimately reaches the conductor. Give it a neutral
        // answer so it cannot hold the queued contender before scheduling.
        let mut conductor = fx.config.agents[0].clone();
        conductor.id = "neutral-conductor".into();
        conductor.command = vec![
            "sh".into(),
            "-c".into(),
            "printf '%s\\n' '{\"decisions\":[]}'".into(),
        ];
        // Keep the graph's existing roster unchanged by the extra role.
        let roster: Vec<String> = fx.config.agents.iter().map(|a| a.id.clone()).collect();
        fx.config.roles.implementers = roster.clone();
        fx.config.roles.judges = roster.clone();
        fx.config.roles.reviewers = roster;
        fx.config.roles.conductor = Some(conductor.id.as_str().into());
        fx.config.agents.push(conductor);
    }

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
    assert_ne!(
        after.status,
        TaskStatus::Held,
        "held: {:?}",
        after.hold_reason
    );
    // Resuming must neither add a run id nor re-execute completed nodes.
    assert!(
        after.runs == vec![run_id.clone()],
        "the run was resumed, not re-competed: {:?}",
        after.runs
    );
    assert_eq!(
        after.status,
        TaskStatus::Done,
        "the resumed run finished: {:?}",
        after.last_error
    );
    if !upgrade {
        assert!(
            !conduct_log.exists(),
            "the conductor was consulted about a task that only awaits its resume"
        );
    }

    let state = RunState::load(&run_id).unwrap();
    assert!(!state.parked, "the resume clears the park");
    assert_ne!(
        state.status,
        RunStatus::Judging,
        "it carried on past judging"
    );
    assert_eq!(state.judgements.len(), judged, "judging was not run again");
    assert_eq!(
        implementation_evidence(&state),
        candidates,
        "completed implementation candidates were retained"
    );
    if let Some(fresh) = fresh {
        assert_eq!(after.attempts, 1, "only the completed attempt is spent");
        let fresh = queue.get(&fresh.id).unwrap();
        assert_eq!(fresh.status, TaskStatus::Done, "fresh work also progresses");
        assert_eq!(fresh.runs.len(), 1);
        let fresh_run = RunState::load(&fresh.runs[0]).unwrap();
        assert_eq!(state.passes.len(), 2, "one parked pass and one resume");
        assert!(state.passes[1].resumed);
        assert_eq!(after.passes.len(), 2);
        assert_eq!(after.passes[0].cost, magi::queue::PassCost::Refunded);
        assert_eq!(after.passes[1].cost, magi::queue::PassCost::Spent);
        assert!(
            fresh_run.passes[0].started_at >= after.passes.last().unwrap().settled_at,
            "fresh high-priority work only starts after the upgrade resume completes"
        );
    }
}

// Finishing may fold losing worktrees; all implementation evidence must stay.
fn implementation_evidence(state: &RunState) -> serde_json::Value {
    let mut candidates = state.candidates.clone();
    for candidate in &mut candidates {
        candidate.folded = false;
    }
    serde_json::to_value(candidates).unwrap()
}
