//! `verify.pre_gate` runs on the winner just before the gate, and whatever it
//! changes reaches the gate as one commit. It never fails a run by itself.
mod common;

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::run::RunStatus;

/// Passes only when exactly `$1` pre_gate commits are in the tree's history
/// and nothing is left uncommitted - what the gate must see.
fn gate_expects(n: usize) -> String {
    format!(
        "test \"$(git log --format=%s | grep -c 'magi: pre_gate')\" = {n} && git diff --quiet HEAD"
    )
}

common::e2e! {
async fn a_hook_that_changes_files_lands_as_one_commit_before_the_gate() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.pre_gate = vec!["echo formatted >> fmt.txt".to_owned()];
    fx.config.verify.gate = vec![gate_expects(1)];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.gate_ran);
    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
    assert_eq!(state.pre_gate.len(), 1);
    assert!(state.pre_gate[0].ok());
    assert!(state.pre_gate_commit.is_some());
}
}

common::e2e! {
async fn a_hook_that_changes_nothing_makes_no_commit() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.pre_gate = vec!["true".to_owned()];
    fx.config.verify.gate = vec![gate_expects(0)];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
    assert_eq!(state.pre_gate.len(), 1);
    assert!(state.pre_gate_commit.is_none());
}
}

common::e2e! {
async fn a_failing_hook_warns_and_the_gate_still_runs() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.pre_gate = vec!["exit 3".to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(!state.pre_gate[0].ok());
    assert_eq!(state.pre_gate[0].code, Some(3));
    assert!(state.gate_ran, "the gate must still run");
    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
    assert_ne!(state.status, RunStatus::Blocked);
    assert!(state.events.iter().any(|e| e.node == "pre_gate" && e.message.contains("FAIL")));
}
}

common::e2e! {
async fn an_empty_pre_gate_changes_nothing() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.gate = vec![gate_expects(0)];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.pre_gate.is_empty());
    assert!(state.pre_gate_commit.is_none());
    assert!(state.events.iter().all(|e| e.node != "pre_gate"));
    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
}
}
