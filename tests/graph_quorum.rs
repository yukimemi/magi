//! Rate limiting: when judge seats run out of quota and fall away, the run
//! must not look like a healthy `Ready`. That is the whole point of the quorum
//! logic — a verdict backed by a minority of the panel is not trustworthy.
mod common;

use common::fixture_with_quota;
use magi::graph::Runner;
use magi::report;
use magi::run::RunStatus;

#[tokio::test]
async fn below_quorum_run_is_stalled_not_ready_and_resumable() {
    let _home = common::home_lock().await;
    // Two of the three judges are rate-limited out at their ranking.
    let fx = fixture_with_quota(&["judge-1", "judge-2"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // The two rate-limited seats are recorded, with node and reset.
    let losses = &state.quota;
    assert_eq!(losses.len(), 2, "both rate-limited judges recorded");
    let seats: Vec<&str> = losses.iter().map(|l| l.seat.as_str()).collect();
    assert!(seats.contains(&"judge-1"), "{seats:?}");
    assert!(seats.contains(&"judge-2"), "{seats:?}");
    assert!(losses.iter().all(|l| l.node == "judge"));
    // The seed keeps the run resumable and its work intact: candidates +
    // the surviving judge are all still there.
    assert_eq!(state.candidates.len(), 3);
    assert_eq!(state.judgements.len(), 3, "all three judge slots recorded");

    // Only one judge forms the verdict, below quorum (majority of 3 = 2).
    let tally = state.tally.as_ref().expect("a tally is still computed");
    assert_eq!(tally.judges, 3);
    assert_eq!(tally.present, 1);
    assert_eq!(tally.quorum, 2);
    assert!(!tally.met_quorum);

    // The run is stalled — not the same status a healthy run ends in.
    assert_eq!(state.status, RunStatus::Stalled);

    // The graph stops at the tally: no review, no gate, no merge.
    assert!(state.reviews.is_empty());
    assert!(state.gate.is_empty());
    assert!(state.merge.is_none());
    // And no loser worktree was folded away: the run is still resumable.
    assert!(state.candidates.iter().all(|c| !c.folded));

    // A one-line listing and the report must both say the panel collapsed.
    assert!(report::line(state).contains("stalled"));
    assert!(
        report::line(state).contains("quorum 1/3"),
        "{}",
        report::line(state)
    );
    let full = report::run(state);
    assert!(full.contains("BELOW QUORUM"), "{full}");

    // `--resume` reopens it unchanged: nothing is re-run, nothing lost.
    let mut again = Runner::resume(&state.id).expect("resume");
    again.execute().await.expect("re-execute");
    assert_eq!(again.state.status, RunStatus::Stalled);
    assert_eq!(again.state.judgements.len(), 3);
    assert_eq!(again.state.tally.as_ref().unwrap().present, 1);
}

#[tokio::test]
async fn a_rate_limited_seat_is_not_retried() {
    let _home = common::home_lock().await;
    let fx = fixture_with_quota(&["judge-1"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // The fixture configures one retry. A normal failure would produce a
    // `judge-N-retry1` artifact; a rate-limited seat must not be re-asked at
    // all, because a retry now is known to fail the same way.
    let art = state.dir().join("artifacts");
    assert!(
        !art.join("judge-1-retry1.out").exists(),
        "the rate-limited judge must not be retried"
    );
    // With only one judge lost, the remaining two still form a quorum.
    let tally = state.tally.as_ref().expect("tally");
    assert!(tally.met_quorum, "2 of 3 still meets the majority quorum");
}

#[tokio::test]
async fn the_full_panel_present_is_still_a_healthy_ready() {
    let _home = common::home_lock().await;
    // No quota: all three judges rank and vote, so the run reaches Ready as
    // before. This is the control that proves the new quorum machinery does
    // not change what a healthy run looks like.
    let fx = fixture_with_quota(&[]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;
    assert!(state.quota.is_empty());
    let tally = state.tally.as_ref().expect("tally");
    assert!(tally.met_quorum);
    assert_eq!(tally.present, 3);
    assert_eq!(state.status, RunStatus::Ready);
    assert!(
        !report::line(state).contains("quorum"),
        "{}",
        report::line(state)
    );
}
