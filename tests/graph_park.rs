//! Parking at a node boundary, and resuming into the node it stopped before.
//!
//! This is the "stop everything, replace the binary, resume" cycle: an
//! operator with fixes to install cannot wait out a competition, and killing
//! the process loses whatever the seats in flight had not written.
mod common;

use common::{Judges, fixture, home_lock};
use magi::graph::{Pause, Runner};
use magi::run::RunStatus;

common::e2e! {
async fn a_parked_run_keeps_its_work_and_resumes_into_the_next_node() {
    let _home = home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, false);
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
}

common::e2e! {
async fn a_resumed_run_drops_a_stale_active_marker_left_by_a_killed_process() {
    let _home = home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, false);
    let pause = Pause::new();
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.on_pause(pause.clone());
    pause.park();
    runner.execute().await.expect("execute parks cleanly");

    // A daemon `SIGKILL`ed mid-wave never gets to clear the seat it was
    // asking, so its last batch is exactly what a reload would see: an entry
    // in `active` with nobody left to answer it.
    runner.state.seat_started(
        "implement",
        "impl-B",
        std::time::Duration::from_secs(3600),
        0,
    );
    runner.state.save().expect("save");
    let id = runner.state.id.clone();
    drop(runner);

    let resumed = Runner::resume(&id).expect("resume");
    assert!(
        resumed.state.active.contains_key("impl-B"),
        "loading a run reflects exactly what was on disk, stale or not"
    );

    // `execute` must not read that leftover as "impl-B is still running" and
    // must not let it survive into the run it carries on: the very next thing
    // it does is drop it, before dispatching any wave of its own.
    let mut resumed = resumed;
    resumed.execute().await.expect("execute to a verdict");
    assert!(
        !resumed.state.active.contains_key("impl-B"),
        "a resumed run must not go on claiming a leftover seat is still \
         answering: {:?}",
        resumed.state.active
    );
}
}

common::e2e! {
/// `driver_pid` is what `RunState::liveness` checks when no daemon claims a
/// run — a plain `magi run` / `magi review` claims nothing there. `execute`
/// must write this process's own pid every time it runs, including on a
/// resume, so a stale pid from whatever process drove an earlier attempt
/// (possibly dead by the time this one starts) never survives into a fresh
/// process's own report.
async fn execute_records_its_own_pid_as_the_driver_on_every_entry() {
    let _home = home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, false);
    let pause = Pause::new();
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.on_pause(pause.clone());
    pause.park();
    runner.execute().await.expect("execute parks cleanly");
    assert_eq!(runner.state.driver_pid, Some(std::process::id()));

    // Pretend an earlier, now-dead process drove this run — exactly what a
    // resume after a kill finds on disk.
    runner.state.driver_pid = Some(999_999_999);
    runner.state.save().expect("save");
    let id = runner.state.id.clone();
    drop(runner);

    let mut resumed = Runner::resume(&id).expect("resume");
    resumed.execute().await.expect("execute to a verdict");
    assert_eq!(
        resumed.state.driver_pid,
        Some(std::process::id()),
        "the resuming process's own pid replaces whatever a dead one left behind"
    );
}
}

common::e2e! {
async fn a_park_asked_for_mid_walk_stops_at_the_boundary_after_it() {
    let _home = home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, false);
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
}

common::e2e! {
/// The property both prior attempts at the interrupt-scheduling feature were
/// rejected for missing: a park requested while an agent call is genuinely
/// in flight must not cut that call short, and must only be honoured once
/// the node it belongs to actually finishes.
///
/// `impl-A`'s mock process proves it has actually started (a real file it
/// writes, not a guessed sleep) before this test asks for a park; only then
/// is the park released, and only after `execute` returns is the run checked
/// for what happened. If a park were somehow observed mid-call, `impl-A`
/// would be missing from the candidates `implement` returns, or `judge`
/// would already have started — either failure this test would catch.
async fn a_park_requested_while_a_seat_is_mid_call_does_not_cut_it_short() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);

    let block_dir = fx.tmp.path().join("block");
    std::fs::create_dir_all(&block_dir).expect("block dir");
    for a in &mut fx.config.agents {
        a.env
            .insert("MOCK_BLOCK_SEAT".to_owned(), "impl-A".to_owned());
        a.env.insert(
            "MOCK_BLOCK_DIR".to_owned(),
            block_dir.to_string_lossy().into_owned(),
        );
    }

    let pause = Pause::new();
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.on_pause(pause.clone());

    let started_marker = block_dir.join("started-impl-A");
    let release_marker = block_dir.join("release-impl-A");
    let interrupter = tokio::spawn({
        let pause = pause.clone();
        async move {
            // A real signal that `impl-A`'s call has begun, not a guessed
            // duration - bounded so a regression that never reaches it fails
            // fast instead of hanging the suite.
            let mut seen = false;
            for _ in 0..500 {
                if started_marker.exists() {
                    seen = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(seen, "the mock never signalled that impl-A had started");

            // The call is now genuinely in flight. Ask it to park.
            pause.park();
            // Nothing inside the call can observe that - only `execute`'s
            // own boundary check, after `implement` returns, can - so
            // letting it finish on its own is what proves the point rather
            // than merely being lucky about timing.
            std::fs::write(&release_marker, b"go").expect("release impl-A");
        }
    });

    runner
        .execute()
        .await
        .expect("execute parks after implement");
    interrupter.await.expect("interrupter");

    assert_eq!(
        runner.state.candidates.len(),
        3,
        "all three candidates exist, including impl-A's own"
    );
    let a = runner
        .state
        .candidates
        .iter()
        .find(|c| c.label == 'A')
        .expect("candidate A");
    assert!(
        a.commits > 0 && a.failed.is_none(),
        "impl-A's work was not thrown away by the park request: {a:?}"
    );
    assert!(
        runner.state.judgements.is_empty(),
        "the park took effect at the boundary right after `implement`, \
         before `judge` ever started"
    );
    assert!(runner.state.parked);
}
}
