//! A run that already reached its conclusion must keep it on reentry.
//!
//! `graph.candidates = 1` is the shipped default, so `judge` skips the panel
//! on every run, not just an edge case. That skip leaves `judgements` empty
//! forever, which used to be indistinguishable from "not yet judged" on a
//! later reentry: `judge` ran a second time, rewrote `status` back to
//! `Judging`, and a run that had already exhausted its review budget and
//! stopped `Blocked` was gated and merged a second time around.
mod common;

use common::{fixture_always_blocked, home_lock};
use magi::graph::Runner;
use magi::run::RunStatus;

#[tokio::test]
async fn a_blocked_run_stays_blocked_on_reentry() {
    let _home = home_lock().await;
    let fx = fixture_always_blocked(&_home);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");

    let state = &runner.state;
    assert_eq!(state.candidates.len(), 1, "the solo-candidate default");
    assert!(
        state.judgements.is_empty(),
        "a solo candidate is not judged"
    );
    assert!(
        state.judge_skipped,
        "the skip is recorded, not just inferred from an empty vec"
    );
    assert_eq!(state.reviews.len(), 3, "{:?}", state.reviews);
    assert!(
        state.reviews.iter().all(|r| !r.clean),
        "every round stayed blocked: {:?}",
        state.reviews
    );
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.gate.is_empty(), "the gate never ran");
    assert!(state.merge.is_none(), "nothing was merged");

    let skip_events = |events: &[magi::run::Event]| {
        events
            .iter()
            .filter(|e| e.node == "judge" && e.message.contains("judging skipped"))
            .count()
    };
    assert_eq!(
        skip_events(&state.events),
        1,
        "exactly one skip event so far: {:?}",
        state.events
    );

    let id = state.id.clone();
    drop(runner);

    // Reenter exactly as `--resume` or the web UI's resume button would.
    let mut again = Runner::resume(&id).expect("resume");
    again.execute().await.expect("re-execute");
    let state = &again.state;

    assert_eq!(
        state.status,
        RunStatus::Blocked,
        "the conclusion must not change on reentry"
    );
    assert_eq!(
        state.reviews.len(),
        3,
        "no new round is opened on an exhausted run: {:?}",
        state.reviews
    );
    assert!(
        state.gate.is_empty(),
        "a reentered blocked run must not reach the gate"
    );
    assert!(
        state.merge.is_none(),
        "a reentered blocked run must not be merged"
    );
    assert_eq!(
        skip_events(&state.events),
        1,
        "judge must not run — and log its skip — a second time: {:?}",
        state.events
    );
}
