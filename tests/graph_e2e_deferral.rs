//! Review scheduling: a round that already has a blocking finding, with a
//! repair round still available, must go straight to the fixer instead of
//! running `verify.e2e` first — that command is the loop's slowest step, and
//! a round that already knows it is going back to the fixer regardless of
//! what verification says has nothing to learn from running it yet. `e2e`
//! must still run for real, on the actual tree, before a round can be called
//! clean or before the loop hands off — never fabricated, never silently
//! read as green because it was empty for an unrelated reason.
//!
//! These assertions are on real recorded command traces from the mock agent
//! and the shell commands `verify.e2e`/`verify.gate` actually run (`test -f
//! note.txt` / `test -f note.txt`, both fast, real subprocesses — no sleeps
//! standing in for the 18-minute `cargo test` this change exists to stop
//! duplicating), never on model response timing.
mod common;

use common::{Judges, fixture, fixture_that_never_clears, fixture_with_noop_fixer};
use magi::graph::Runner;
use magi::run::{Event, RunStatus};

/// Every `verify` event whose message names the real configured command —
/// i.e. a round where `verify.e2e` (`test -f note.txt`) actually ran, as
/// opposed to the deferral notice, which never quotes the command.
fn e2e_execution_events(events: &[Event], round: usize) -> Vec<&str> {
    let needle = format!("round {round}: `test -f note.txt`");
    events
        .iter()
        .filter(|e| e.node == "verify" && e.message.starts_with(&needle))
        .map(|e| e.message.as_str())
        .collect()
}

#[tokio::test]
async fn a_blocking_round_with_rounds_left_fixes_before_running_e2e() {
    let _guard = common::home_lock().await;
    // Blocking every round until the seats stop lying: perfect for pinning
    // "round 1 must not have run e2e" without racing whether the fixer
    // happens to clear the finding immediately.
    let fx = fixture_that_never_clears(_guard, 3);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.reviews.len() >= 2, "{:?}", state.reviews);
    let first = &state.reviews[0];
    assert!(first.blocking > 0, "round 1 must have a blocking finding");
    assert!(
        first.e2e.is_empty(),
        "e2e must not have run on a blocking round with a repair round left: {:?}",
        first.e2e
    );
    assert!(
        first.e2e_deferred,
        "the empty e2e above must be recorded as deferred, not unconfigured"
    );
    assert!(
        first.e2e_defer_reason.is_some(),
        "a deferred round must carry why"
    );
    assert!(first.fix.is_some(), "the fixer must still have run");
    assert!(
        e2e_execution_events(&state.events, 1).is_empty(),
        "no real command must have been run for round 1: {:?}",
        state.events
    );
    // The deferral itself is still visible in the event log — silent
    // deferral is exactly what "fabricate green" would look like.
    assert!(
        state
            .events
            .iter()
            .any(|e| e.node == "verify" && e.message.contains("deferred to the fixer")),
        "the deferral must be recorded, not just implied by an empty vec: {:?}",
        state.events
    );
}

#[tokio::test]
async fn a_head_with_no_more_blocking_findings_runs_e2e_before_going_clean() {
    let _guard = common::home_lock().await;
    // `require_fix = true` clears its single blocking finding once the fixer
    // creates `fixed.txt`, so round 2 has nothing left to defer against.
    let fx = fixture(_guard, Judges::Unanimous, true);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.reviews.len(), 2, "{:?}", state.reviews);
    assert!(state.reviews[0].blocking > 0);
    assert!(state.reviews[0].e2e_deferred, "round 1 deferred");

    let clean = &state.reviews[1];
    assert!(clean.clean, "round 2 has nothing left to fix");
    assert!(
        !clean.e2e_deferred,
        "a round that goes clean must have actually run e2e, not deferred it"
    );
    assert!(
        !clean.e2e.is_empty(),
        "a clean round's e2e must hold real outcomes: {:?}",
        clean.e2e
    );
    assert_eq!(
        e2e_execution_events(&state.events, 2).len(),
        1,
        "round 2 must show exactly one real e2e run: {:?}",
        state.events
    );

    // The final gate still runs on the actual tree that would land, exactly
    // as before — a deferred earlier round changes nothing about this.
    assert!(!state.gate.is_empty(), "the gate must have run");
    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(state.status, RunStatus::Ready);
}

#[tokio::test]
async fn the_e2e_every_round_flag_restores_the_old_diagnostic_behaviour() {
    let _guard = common::home_lock().await;
    let mut fx = fixture_that_never_clears(_guard, 3);
    fx.config.graph.e2e_every_round = true;
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.reviews.len() >= 2, "{:?}", state.reviews);
    let first = &state.reviews[0];
    assert!(first.blocking > 0);
    assert!(
        !first.e2e_deferred,
        "the operator asked for the old every-round behaviour"
    );
    assert!(
        !first.e2e.is_empty(),
        "e2e must have run on round 1 with the flag set: {:?}",
        first.e2e
    );
    assert_eq!(
        e2e_execution_events(&state.events, 1).len(),
        1,
        "round 1 must show a real e2e run under the compatibility flag: {:?}",
        state.events
    );
}

#[tokio::test]
async fn the_final_round_never_defers_even_with_blocking_findings_left() {
    let _guard = common::home_lock().await;
    // Two rounds, never clears: round 1 has a repair round left (defers),
    // round 2 is the budget's last round (must not defer — nothing would
    // ever verify it otherwise, since the run stops right after).
    let fx = fixture_that_never_clears(_guard, 2);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.reviews.len(), 2, "{:?}", state.reviews);
    assert!(state.reviews[0].e2e_deferred, "round 1 had a round left");
    let last = &state.reviews[1];
    assert!(
        !last.e2e_deferred,
        "the last round of the budget must always run e2e for real"
    );
    assert!(!last.e2e.is_empty(), "{:?}", last.e2e);
}

#[tokio::test]
async fn exhausted_rounds_catch_up_on_a_deferred_e2e_before_deciding_the_gate() {
    let _guard = common::home_lock().await;
    // The fixer never actually touches the tree, so two stagnant rounds stop
    // the loop before the (generous) round budget — and both of those rounds
    // had a blocking finding with a round left, so both deferred e2e. The
    // stop must not read that silence as green: it must run e2e for real on
    // the tree it is about to stop touching before deciding anything.
    let fx = fixture_with_noop_fixer(_guard, 6);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.reviews.len(), 2, "{:?}", state.reviews);
    assert!(
        state.reviews.iter().all(|r| r.blocking > 0 && !r.clean),
        "{:?}",
        state.reviews
    );

    let last = state.reviews.last().expect("a round was recorded");
    assert!(
        !last.e2e_deferred,
        "the round the loop stopped on must have a real, executed e2e result: {:?}",
        last
    );
    assert!(!last.e2e.is_empty(), "{:?}", last.e2e);
    assert!(
        state
            .events
            .iter()
            .any(|e| e.node == "verify" && e.message.contains("catching up")),
        "the catch-up run must be visible in the event log: {:?}",
        state.events
    );

    // Exhausting the round budget with a deferred last round must not skip
    // the gate — it must still run on the actual final tree.
    assert!(!state.gate.is_empty(), "the gate must have run");
    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(state.status, RunStatus::Ready);
    assert!(state.handed_off_with_open_findings());
}
