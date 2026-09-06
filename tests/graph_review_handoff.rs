//! Round-limit and stagnant-round endings, and what they must **not** all
//! collapse into: `blocked`.
//!
//! `graph::Runner::review_loop` used to answer every way of running out of
//! review rounds with the same three letters, whether the tree was clean and
//! verified or genuinely on fire. These tests pin the split: gate and e2e
//! green hands off, red blocks with what failed, and a tree that stops moving
//! is not worth waiting on the full round budget for.
mod common;

use common::{Judges, fixture, fixture_that_never_clears, fixture_with_noop_fixer};
use magi::graph::Runner;
use magi::run::RunStatus;

#[tokio::test]
async fn a_spent_round_budget_hands_off_when_gate_and_e2e_stay_green() {
    let _guard = common::home_lock().await;
    let fx = fixture_that_never_clears(2);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // The budget really was spent: two rounds, neither clean.
    assert_eq!(state.reviews.len(), 2, "{:?}", state.reviews);
    assert!(state.reviews.iter().all(|r| !r.clean));
    assert!(state.reviews.last().unwrap().blocking > 0);

    // Verification never failed, so this is a hand-off, not a block.
    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(
        state.status,
        RunStatus::Ready,
        "gate and e2e were green; a spent round budget must not read as blocked"
    );
    assert!(state.handed_off_with_open_findings());
    assert!(
        !state.open_findings().is_empty(),
        "the findings that were still open must stay readable"
    );
}

#[tokio::test]
async fn a_red_gate_after_the_round_budget_blocks_with_what_failed() {
    let _guard = common::home_lock().await;
    let mut fx = fixture_that_never_clears(2);
    // The review loop's own e2e (`test -f note.txt`) still passes; only the
    // final gate is red, so this exercises the gate path specifically.
    fx.config.verify.gate = vec!["false".to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.gate.iter().any(|o| !o.ok()));
    let event_text = state
        .events
        .iter()
        .filter(|e| e.node == "gate")
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        event_text.contains("FAIL"),
        "the outcome must name what failed: {event_text}"
    );
}

#[tokio::test]
async fn a_red_e2e_at_the_round_budget_blocks_with_what_failed() {
    let _guard = common::home_lock().await;
    let mut fx = fixture_that_never_clears(2);
    // Red inside the review loop's own verification, not the separate gate
    // step — this exercises `stop_reviewing`'s e2e branch directly.
    fx.config.verify.e2e = vec!["false".to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.status, RunStatus::Blocked);
    let last = state.reviews.last().expect("a round was recorded");
    assert!(last.e2e.iter().any(|o| !o.ok()));
    let event_text = state
        .events
        .iter()
        .filter(|e| e.node == "review")
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        event_text.contains("e2e failed"),
        "the outcome must say verification failed, not just findings: {event_text}"
    );
}

#[tokio::test]
async fn a_tree_that_stops_moving_hands_off_before_the_round_budget() {
    let _guard = common::home_lock().await;
    // A generous budget the run must not need: the fixer never actually
    // changes the tree, so two stagnant rounds must be enough to stop.
    let fx = fixture_with_noop_fixer(6);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(
        state.reviews.len(),
        2,
        "two rounds of no progress must stop the loop, not the {}-round budget: {:?}",
        fx.config.graph.review_rounds,
        state.reviews
    );
    assert!(
        state.reviews.iter().all(|r| !r.progressed),
        "the fixer never touched the tree: {:?}",
        state.reviews
    );
    // The fixer's own report still claims success — this is exactly the
    // false signal the tree-diff check exists to see through.
    let fix = state.reviews[0].fix.as_ref().expect("the fixer ran");
    assert!(!fix.addressed.is_empty(), "the (lying) report says fixed");

    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(state.status, RunStatus::Ready);
    assert!(state.handed_off_with_open_findings());
}

#[tokio::test]
async fn a_clean_round_is_unaffected_by_any_of_this() {
    let _guard = common::home_lock().await;
    // The ordinary happy path (one blocker, one fix, clean) must still behave
    // exactly as before: this guards against the round-ending refactor
    // changing what a genuinely clean run looks like.
    let fx = fixture(Judges::Unanimous, true);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.reviews.last().unwrap().clean);
    assert_eq!(state.status, RunStatus::Ready);
    assert!(!state.handed_off_with_open_findings());
    assert!(state.open_findings().is_empty());
}
