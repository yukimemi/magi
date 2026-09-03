//! Parking at a node boundary, and resuming into the node it stopped before.
//!
//! This is the "stop everything, replace the binary, resume" cycle: an
//! operator with fixes to install cannot wait out a competition, and killing
//! the process loses whatever the seats in flight had not written.
mod common;

use common::{Judges, fixture, home_lock};
use magi::graph::{Pause, Runner};
use magi::run::RunStatus;

#[tokio::test]
async fn a_parked_run_keeps_its_work_and_resumes_into_the_next_node() {
    let _home = home_lock().await;
    let fx = fixture(Judges::Unanimous, false);
    let pause = Pause::new();
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.on_pause(pause.clone());

    // Asked before the walk begins, so it parks at the first boundary it
    // reaches: after `prep`, with the worktrees built and nothing implemented.
    pause.park();
    runner.execute().await.expect("execute parks cleanly");

    let id = runner.state.id.clone();
    assert!(runner.state.parked, "the run records that it parked");
    assert!(
        !runner.state.status.done(),
        "a park is not a terminal status: {:?}",
        runner.state.status
    );
    assert_eq!(
        runner.state.candidates.len(),
        3,
        "prep finished, so its worktrees are kept"
    );
    assert!(
        runner.state.judgements.is_empty(),
        "and the graph stopped before judging"
    );
    assert!(
        runner
            .state
            .events
            .iter()
            .any(|e| e.node == "park" && e.message.contains("resume to carry on")),
        "the timeline says why it is sitting there: {:?}",
        runner.state.events.last()
    );

    // Resuming with nobody asking for a park carries the run to the end. The
    // candidates prep built are the ones it competes: no second competition,
    // and no repeated prep.
    let mut resumed = Runner::resume(&id).expect("resume");
    resumed.execute().await.expect("execute to a verdict");

    assert_eq!(
        resumed.state.candidates.len(),
        3,
        "the same three candidates, not a fresh set"
    );
    assert!(
        matches!(
            resumed.state.status,
            RunStatus::Ready | RunStatus::Merged | RunStatus::Blocked
        ),
        "the run reached a terminal status: {:?}",
        resumed.state.status
    );
    assert!(
        !resumed.state.parked,
        "and it is no longer parked once it has been carried on"
    );
}

#[tokio::test]
async fn a_park_asked_for_mid_walk_stops_at_the_boundary_after_it() {
    let _home = home_lock().await;
    let fx = fixture(Judges::Unanimous, false);
    let pause = Pause::new();
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.on_pause(pause.clone());

    // A park that arrives while the graph is walking is honoured at the next
    // boundary, never mid-node: the node in progress finishes and writes what
    // it produced, which is the whole reason parking is cheap.
    let asker = tokio::spawn({
        let pause = pause.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            pause.park();
        }
    });
    runner.execute().await.expect("execute");
    asker.await.expect("asker");

    assert!(runner.state.parked);
    assert!(
        !runner.state.candidates.is_empty(),
        "whatever node it was in finished and recorded its work"
    );
    assert!(
        !runner.state.status.done(),
        "and it stopped short of a verdict: {:?}",
        runner.state.status
    );
}
