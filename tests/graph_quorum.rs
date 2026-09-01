//! Rate limiting: when judge seats run out of quota and fall away, the run
//! must not look like a healthy `Ready`. That is the whole point of the quorum
//! logic — a verdict backed by a minority of the panel is not trustworthy.
mod common;

use common::{fixture_with_failure, fixture_with_quota};
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

    // `--resume` reopens it: the collapsed seats are re-asked but their quota
    // is still exhausted in this fixture, so they fall away again and the run
    // stays stale — no verdict is clobbered back into a healthy-looking one.
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
    let listing = || {
        let mut names: Vec<String> = std::fs::read_dir(&art)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names.join(" ")
    };
    let head = |name: &str| {
        std::fs::read_to_string(art.join(name))
            .unwrap_or_default()
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join(" / ")
    };
    assert!(
        !art.join("judge-1-retry1.out").exists(),
        "the rate-limited judge must not be retried.\n\
         artifacts: {}\n\
         attempt 0 stdout: {:?}\n\
         retry prompt head: {:?}\n\
         retry stdout: {:?}\n\
         candidates: {:?}\n\
         quota losses: {:?}",
        listing(),
        std::fs::read_to_string(art.join("judge-1.out")).unwrap_or_default(),
        head("judge-1-retry1.prompt.md"),
        std::fs::read_to_string(art.join("judge-1-retry1.out")).unwrap_or_default(),
        state
            .candidates
            .iter()
            .map(|c| (c.label, c.viable(), c.commits, c.failed.clone()))
            .collect::<Vec<_>>(),
        state.quota,
    );
    // With only one judge lost, the remaining two still form a quorum.
    let tally = state.tally.as_ref().expect("tally");
    assert!(tally.met_quorum, "2 of 3 still meets the majority quorum");
}

#[tokio::test]
async fn a_stalled_run_recovers_to_ready_once_the_quota_resets() {
    let _home = common::home_lock().await;
    // Phase 1: two of three judges are rate-limited out — below quorum,
    // the run collapses to `Stalled` and stops before review/gate/merge.
    let fx = fixture_with_quota(&["judge-1", "judge-2"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    assert_eq!(runner.state.status, RunStatus::Stalled);
    assert_eq!(runner.state.tally.as_ref().unwrap().present, 1);
    let id = runner.state.id.clone();
    drop(runner);

    // The quota resets between attempts: clear the simulated limit so the
    // collapsed seats can answer on `--resume`.
    {
        let mut state = magi::run::RunState::load(&id).expect("load");
        for a in &mut state.config.agents {
            a.env.remove("MOCK_QUOTA_SEAT");
        }
        state.save().expect("save");
    }

    // Resuming re-asks the lost judges; with their quota back the quorum is
    // restored and the run finishes the graph to a healthy `Ready`.
    let mut again = Runner::resume(&id).expect("resume");
    again.execute().await.expect("re-execute");
    assert_eq!(
        again.state.status,
        RunStatus::Ready,
        "{}",
        report::run(&again.state)
    );
    let tally = again.state.tally.as_ref().expect("tally");
    assert!(tally.met_quorum, "quorum restored on resume");
    assert_eq!(tally.present, 3);
    assert!(
        again.state.quota.is_empty(),
        "recovered seats drop their loss"
    );
    assert!(
        !report::line(&again.state).contains("quorum"),
        "{}",
        report::line(&again.state)
    );
}

#[tokio::test]
async fn a_stalled_run_stays_stalled_when_the_quota_has_not_reset() {
    let _home = common::home_lock().await;
    // judge-1 and judge-2 are still rate-limited on resume, so the run cannot
    // reach a quorum no matter how often it is retried: it must stay `Stalled`
    // and resumable rather than finish on the back of one surviving judge.
    let fx = fixture_with_quota(&["judge-1", "judge-2"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    assert_eq!(runner.state.status, RunStatus::Stalled);
    let id = runner.state.id.clone();
    drop(runner);

    let mut again = Runner::resume(&id).expect("resume");
    again.execute().await.expect("re-execute");
    assert_eq!(again.state.status, RunStatus::Stalled);
    assert_eq!(again.state.tally.as_ref().unwrap().present, 1);
    // Still below quorum: nothing was folded, reviewed, gated or merged.
    assert!(again.state.reviews.is_empty());
    assert!(again.state.gate.is_empty());
    assert!(again.state.merge.is_none());
}

#[tokio::test]
async fn a_plain_failure_collapse_stalls_and_recovers_on_resume() {
    let _home = common::home_lock().await;
    // Two of the three judges fail with an *ordinary* error — no usable output,
    // no rate-limit shape. That collapses the quorum exactly like quota, and the
    // run must not pretend to be healthy.
    let fx = fixture_with_failure(&["judge-1", "judge-2"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // A plain failure is retried (records a retry artifact) but not mistaken for
    // quota.
    let art = state.dir().join("artifacts");
    assert!(
        art.join("judge-1-retry1.out").exists(),
        "a plain failure is retried like any other unusable reply"
    );
    assert!(
        state.quota.is_empty(),
        "a plain failure is not a quota loss"
    );
    let failed_j = |seat: &str| {
        state
            .judgements
            .iter()
            .find(|j| j.seat == seat)
            .map(|j| j.failed.is_some())
            .unwrap_or(false)
    };
    assert!(failed_j("judge-1"), "judge-1 failed outright");
    assert!(failed_j("judge-2"), "judge-2 failed outright");
    assert!(!failed_j("judge-3"), "judge-3 ranked and voted");
    assert_eq!(state.status, RunStatus::Stalled);
    let tally = state.tally.as_ref().expect("tally");
    assert_eq!(tally.present, 1);
    assert!(!tally.met_quorum);
    let id = state.id.clone();
    drop(runner);

    // The transient failure clears: with the mock no longer failing, resume
    // re-asks the two absent judges, restores the quorum, and reaches `Ready`.
    {
        let mut s = magi::run::RunState::load(&id).expect("load");
        for a in &mut s.config.agents {
            a.env.remove("MOCK_FAILED_SEAT");
        }
        s.save().expect("save");
    }
    let mut again = Runner::resume(&id).expect("resume");
    again.execute().await.expect("re-execute");
    assert_eq!(
        again.state.status,
        RunStatus::Ready,
        "{}",
        report::run(&again.state)
    );
    assert!(again.state.tally.as_ref().unwrap().met_quorum);
    assert_eq!(again.state.tally.as_ref().unwrap().present, 3);
}

#[tokio::test]
async fn a_plain_failure_collapse_stays_stalled_while_the_failure_persists() {
    let _home = common::home_lock().await;
    let fx = fixture_with_failure(&["judge-1", "judge-2"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    assert_eq!(runner.state.status, RunStatus::Stalled);
    let id = runner.state.id.clone();
    drop(runner);

    let mut again = Runner::resume(&id).expect("resume");
    again.execute().await.expect("re-execute");
    assert_eq!(again.state.status, RunStatus::Stalled);
    assert_eq!(again.state.tally.as_ref().unwrap().present, 1);
    assert!(again.state.reviews.is_empty());
    assert!(again.state.gate.is_empty());
    assert!(again.state.merge.is_none());
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
