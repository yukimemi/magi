//! A failing `verify.gate` gets a bounded fix round instead of ending the run
//! blocked - but only for an ordinary non-zero exit that printed something.
mod common;

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::run::RunStatus;

/// Red until `gatefix.txt` exists, saying why on stdout: the shape of a real
/// lint failure, with the output a fixer can act on.
const GATE_NEEDS_FIX: &str = "test -f gatefix.txt || { echo 'gatefix.txt is missing'; exit 1; }";

fn gate_events(runner: &Runner) -> String {
    runner
        .state
        .events
        .iter()
        .filter(|e| e.node == "gate")
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

common::e2e! {
async fn a_gate_that_fails_once_is_fixed_and_the_run_proceeds_to_merge() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.gate_fixes.len(), 1, "{}", gate_events(&runner));
    assert!(state.gate_fixes[0].committed);
    assert!(state.gate_fixes[0].failed[0].output_tail.contains("gatefix.txt is missing"));
    assert!(state.gate_ran);
    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
    assert_eq!(state.status, RunStatus::Ready, "{}", gate_events(&runner));
    assert!(state.merge.as_ref().is_some_and(|m| m.ok), "the run must reach merge");

    // The fixer was told the failure was the gate's, not a reviewer's.
    let prompt = std::fs::read_to_string(state.dir().join("artifacts/gate-fix-1.prompt.md"))
        .expect("gate-fix prompt artifact");
    assert!(prompt.contains("failed the verification gate"));
    assert!(prompt.contains("gatefix.txt is missing"));
    assert!(!prompt.contains("Your patch was reviewed"));
}
}

common::e2e! {
async fn a_gate_that_keeps_failing_blocks_after_the_cap_with_its_last_output() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.gate_fix_rounds = 2;
    fx.config.verify.gate = vec!["echo still red; exit 1".to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.gate_fixes.len(), 2, "{}", gate_events(&runner));
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.gate_ran);
    assert!(state.gate.iter().any(|o| !o.ok()));
    assert!(state.gate[0].output_tail.contains("still red"));
    let events = gate_events(&runner);
    assert!(events.contains("2 gate-fix round(s)"), "{events}");
    assert!(events.contains("still red"), "{events}");
    assert!(state.merge.is_none());
}
}

common::e2e! {
async fn a_gate_timeout_is_not_a_code_failure_and_spends_no_fix_round() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.timeout_verify = Some(1);
    fx.config.verify.gate = vec!["sleep 5".to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.gate_fixes.is_empty(), "{}", gate_events(&runner));
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.gate.iter().any(|o| o.code.is_none()));
}
}

common::e2e! {
async fn a_gate_fix_that_changes_nothing_ends_blocked_after_one_round() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.gate_fix_rounds = 2;
    for agent in &mut fx.config.agents {
        agent.env.insert("MOCK_GATE_FIX_NOOP".to_owned(), "1".to_owned());
    }
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.gate_fixes.len(), 1, "a no-op fix must not be retried");
    assert!(!state.gate_fixes[0].committed);
    assert_eq!(state.status, RunStatus::Blocked);
}
}
