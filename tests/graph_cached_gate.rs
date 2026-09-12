//! Reentry of completed final-gate records.
//!
//! A clean review is intentionally recomputed on resume. That must not turn a
//! recorded red final gate into an active-looking `Gating` run, nor spend a
//! second review or gate attempt.
mod common;

use common::{Judges, fixture, home_lock};
use magi::graph::Runner;
use magi::run::RunStatus;

#[tokio::test]
async fn a_cached_failed_gate_stays_blocked_and_does_not_run_again() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.verify.gate = vec!["false".to_owned()];

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("first execution");

    assert!(runner.state.reviews.last().is_some_and(|round| round.clean));
    assert_eq!(runner.state.status, RunStatus::Blocked);
    assert_eq!(runner.state.gate.len(), 1);
    assert!(!runner.state.gate[0].ok());
    assert!(runner.state.merge.is_none(), "a red gate cannot merge");

    let id = runner.state.id.clone();
    let gate = runner.state.gate.clone();
    let review_events = runner
        .state
        .events
        .iter()
        .filter(|event| event.node == "review")
        .count();
    let gate_events = runner
        .state
        .events
        .iter()
        .filter(|event| event.node == "gate")
        .count();
    drop(runner);

    // Exercise the public resume path with the fixture's isolated MAGI_HOME,
    // not merely an in-process call to `execute`.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["run", "--resume", &id])
        .current_dir(&fx.repo)
        .env("MAGI_HOME", fx.tmp.path().join("magi-home"))
        .output()
        .expect("run magi --resume");
    assert!(
        !output.status.success(),
        "a cached failed gate must remain an unsuccessful CLI result: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("blocked"),
        "the report must show the terminal state: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let resumed = Runner::resume(&id).expect("load resumed state");

    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.gate.len(), gate.len());
    assert_eq!(resumed.state.gate[0].command, gate[0].command);
    assert_eq!(resumed.state.gate[0].code, gate[0].code);
    assert_eq!(resumed.state.gate[0].output_tail, gate[0].output_tail);
    assert!(
        resumed.state.merge.is_none(),
        "a cached red gate still cannot merge"
    );
    assert_eq!(
        resumed
            .state
            .events
            .iter()
            .filter(|event| event.node == "review")
            .count(),
        review_events,
        "healthy reviewers are not called again"
    );
    assert_eq!(
        resumed
            .state
            .events
            .iter()
            .filter(|event| event.node == "gate")
            .count(),
        gate_events,
        "the old failed command is not represented as a new attempt"
    );
}

#[tokio::test]
async fn a_cached_successful_gate_still_allows_the_interrupted_merge_step() {
    let _home = home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, false);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("first execution");
    assert_eq!(runner.state.status, RunStatus::Ready);
    assert!(runner.state.gate.iter().all(|outcome| outcome.ok()));

    // Model an interruption after the final command was saved and before the
    // merge node persisted its own outcome. This is resumable work, unlike a
    // completed red gate.
    runner.state.status = RunStatus::Gating;
    runner.state.merge = None;
    runner.state.save().expect("save interrupted gate state");
    let id = runner.state.id.clone();
    let gate = runner.state.gate.clone();
    let gate_events = runner
        .state
        .events
        .iter()
        .filter(|event| event.node == "gate")
        .count();
    drop(runner);

    let mut resumed = Runner::resume(&id).expect("resume");
    resumed.execute().await.expect("resume execution");

    assert_eq!(resumed.state.status, RunStatus::Ready);
    assert_eq!(resumed.state.gate.len(), gate.len());
    assert_eq!(resumed.state.gate[0].command, gate[0].command);
    assert_eq!(resumed.state.gate[0].code, gate[0].code);
    assert_eq!(resumed.state.gate[0].output_tail, gate[0].output_tail);
    assert!(
        resumed.state.merge.is_some(),
        "resume completes the pending merge node"
    );
    assert_eq!(
        resumed
            .state
            .events
            .iter()
            .filter(|event| event.node == "gate")
            .count(),
        gate_events,
        "a green cached command is not needlessly rerun"
    );
}
