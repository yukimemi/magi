//! A failed `verify.e2e` is run once more on the same commit before it is
//! believed. A failure that does not reproduce is a recorded, filed flake that
//! still verifies the head; one that reproduces stands, with the second run's
//! output; a build/link failure keeps its own single retry; and
//! `[graph] rerun_failed_e2e = false` restores the first-result-decides
//! behaviour.
//!
//! Every scenario runs real shell commands against the mock agent's fixture.
//! The commands keep their state outside the worktree (`runs`, a counter file
//! under the fixture's temp tree) so counting leaves the tree clean.
mod common;

use common::{Judges, fixture};
use magi::config::Config;
use magi::graph::Runner;
use magi::queue::{Queue, Source};
use magi::run::{E2eStatus, RunState, RunStatus};

/// `path` as a POSIX shell word: forward slashes, quoted.
fn sh(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\\', "/"))
}

/// Fails with a test-shaped failure on every odd invocation and passes on
/// every even one, counting invocations in `counter`. The line number in the
/// panic message differs per call, as a real flaky assertion's would.
fn fails_then_passes(counter: &std::path::Path) -> String {
    let c = sh(counter);
    format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $((n % 2)) -eq 1 ]; then \
         echo 'test web::tests::layout ... FAILED'; \
         echo \"thread 'web::tests::layout' panicked at src/web.rs:$n:9:\"; exit 1; fi"
    )
}

fn count(counter: &std::path::Path) -> usize {
    std::fs::read_to_string(counter)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn verify_events(state: &RunState) -> Vec<&str> {
    state
        .events
        .iter()
        .filter(|e| e.node == "verify")
        .map(|e| e.message.as_str())
        .collect()
}

fn flake_tasks() -> Vec<magi::queue::Task> {
    Queue::open()
        .list()
        .into_iter()
        .filter(|t| matches!(&t.source, Source::Agent { node, .. } if node == magi::flake::NODE))
        .collect()
}

async fn run_once(fx: &common::Fixture, config: Config) -> Runner {
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".to_owned(),
        config,
        magi::run::Origin::operator(),
    )
    .await
    .expect("start");
    runner.execute().await.expect("execute");
    runner
}

common::e2e! {
async fn a_flake_verifies_the_head_and_files_one_task_across_two_runs() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    let counter = fx.tmp.path().join("e2e-count");
    fx.config.verify.e2e = vec![fails_then_passes(&counter)];

    let first = run_once(&fx, fx.config.clone()).await;
    let state = &first.state;
    let round = state.reviews.last().expect("a round");
    assert_eq!(round.e2e_status(), E2eStatus::Passed, "{:?}", verify_events(state));
    assert!(round.clean, "{:?}", verify_events(state));
    assert_eq!(state.status, RunStatus::Ready, "{:?}", verify_events(state));
    assert_eq!(count(&counter), 2, "one failure, one rerun");

    // Recorded on the run, visibly, and never as a plain green.
    assert_eq!(state.verify_flakes.len(), 1);
    let flake = &state.verify_flakes[0];
    assert_eq!(flake.round, Some(round.round));
    assert_eq!(Some(flake.head.as_str()), round.verified_head.as_deref());
    assert!(flake.first_failure.contains("web::tests::layout"));
    assert!(flake.filed && flake.filing_error.is_none());
    let events = verify_events(state).join("\n");
    assert!(events.contains("once more on the same commit"), "{events}");
    assert!(events.contains("a flake, not a verdict on the change"), "{events}");
    let report = magi::report::run(state);
    assert!(report.contains("e2e green on rerun only"), "{report}");
    assert!(report.contains("flaky verification"), "{report}");

    // Exactly one task, asking for the root cause.
    let tasks = flake_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(Some(tasks[0].id.as_str()), flake.task.as_deref());
    assert!(tasks[0].instruction.contains("Find the root cause"));
    assert!(tasks[0].instruction.contains("web::tests::layout"));
    assert!(matches!(&tasks[0].source, Source::Agent { run, node }
        if run == &state.id && node == magi::flake::NODE));
    let task_id = tasks[0].id.clone();

    // A second run hitting the same failure leaves the one task alone and
    // records the occurrence on the run.
    let second = run_once(&fx, fx.config.clone()).await;
    let state = &second.state;
    assert_eq!(count(&counter), 4);
    assert_eq!(state.status, RunStatus::Ready, "{:?}", verify_events(state));
    assert_eq!(state.verify_flakes.len(), 1);
    assert!(!state.verify_flakes[0].filed);
    assert_eq!(state.verify_flakes[0].task.as_deref(), Some(task_id.as_str()));
    assert_eq!(flake_tasks().len(), 1, "ten runs, one task");
}
}

common::e2e! {
async fn a_failure_that_reproduces_stands_with_the_second_runs_output() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, true);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.implementers = 1;
    fx.config.graph.e2e_every_round = true;
    let runs = fx.tmp.path().join("e2e-runs");
    // Red until the fixer's marker file exists, whatever the attempt.
    fx.config.verify.e2e = vec![format!(
        "echo run >> {}; test -f fixed.txt || {{ echo 'test web::tests::layout ... FAILED'; exit 1; }}",
        sh(&runs)
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    let r1 = &state.reviews[0];
    assert_eq!(r1.e2e_status(), E2eStatus::Failed);
    assert!(r1.e2e[0].output_tail.contains("failed twice on the same commit"));
    assert!(r1.e2e[0].output_tail.contains("web::tests::layout"));
    // What the fixer is shown is exactly that second run's output.
    let summary = r1.verification_summary(&r1.head).expect("a failing round has a summary");
    let tail = summary.tail.expect("failed output");
    assert!(tail.contains("failed twice on the same commit") && tail.contains("web::tests::layout"));

    let events = verify_events(state).join("\n");
    assert!(events.contains("round 1: rerun `"), "{events}");
    assert!(events.contains("the failure reproduced on the second run"), "{events}");
    assert!(state.verify_flakes.is_empty(), "a reproduced failure is not a flake");
    assert!(flake_tasks().is_empty());
    // Round 1 ran twice (failure, rerun); round 2 passed on its first run.
    let ran = std::fs::read_to_string(&runs).unwrap_or_default().lines().count();
    assert_eq!(ran, 3, "{events}");
}
}

common::e2e! {
async fn with_the_switch_off_the_first_result_decides_as_before() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, true);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.implementers = 1;
    fx.config.graph.e2e_every_round = true;
    fx.config.graph.rerun_failed_e2e = false;
    let runs = fx.tmp.path().join("e2e-runs");
    fx.config.verify.e2e = vec![format!(
        "echo run >> {}; test -f fixed.txt || {{ echo 'test web::tests::layout ... FAILED'; exit 1; }}",
        sh(&runs)
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    assert_eq!(state.reviews[0].e2e_status(), E2eStatus::Failed);
    assert!(!state.reviews[0].e2e[0].output_tail.contains("failed twice"));
    // One execution per round, exactly the events there were before the rerun.
    let ran = std::fs::read_to_string(&runs).unwrap_or_default().lines().count();
    assert_eq!(ran, state.reviews.len());
    let events = verify_events(state);
    assert_eq!(events.len(), state.reviews.len(), "{events:?}");
    assert!(events[0].starts_with("round 1: `") && events[0].contains("-> FAIL (Some(1))"));
    assert!(events[1].starts_with("round 2: `") && events[1].contains("-> pass"));
    assert!(state.verify_flakes.is_empty() && flake_tasks().is_empty());
}
}

common::e2e! {
async fn a_build_failure_keeps_its_one_retry_and_is_never_rerun_as_a_test() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    let runs = fx.tmp.path().join("e2e-runs");
    fx.config.verify.e2e = vec![format!(
        "echo run >> {}; echo 'error: could not compile `fixture`'; exit 101",
        sh(&runs)
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    let events = verify_events(state).join("\n");
    assert!(events.contains("retrying once before concluding"), "{events}");
    assert!(!events.contains("once more on the same commit"), "{events}");
    assert!(state.reviews.iter().any(|r| r.verify_retried));
    let ran = std::fs::read_to_string(&runs).unwrap_or_default().lines().count();
    let rounds_that_ran = state.reviews.iter().filter(|r| !r.e2e.is_empty()).count();
    assert_eq!(ran, 2 * rounds_that_ran, "attempt + build retry, nothing more: {events}");
    assert!(state.verify_flakes.is_empty() && flake_tasks().is_empty());
}
}

common::e2e! {
async fn a_verification_never_runs_more_than_three_times() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    let counter = fx.tmp.path().join("e2e-count");
    let c = sh(&counter);
    // 1: cannot build; 2: a test failure; 3 and later: green.
    fx.config.verify.e2e = vec![format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $n -eq 1 ]; then echo 'error: could not compile `fixture`'; exit 101; fi; \
         if [ $n -eq 2 ]; then echo 'test web::tests::layout ... FAILED'; exit 1; fi"
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    let round = state.reviews.last().expect("a round");
    assert_eq!(round.e2e_status(), E2eStatus::Passed, "{:?}", verify_events(state));
    assert!(round.verify_retried);
    assert_eq!(count(&counter), 3, "build retry, then one rerun, then stop");
    assert_eq!(state.verify_flakes.len(), 1);
    assert_eq!(flake_tasks().len(), 1);
}
}
