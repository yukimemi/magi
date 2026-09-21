//! A fixer's CLI turn ending cleanly — `success`/`end_turn`, exit 0, a
//! non-empty reply — is not proof the fixer's own report ever arrived. Run
//! 20260912-114326-d3b8's fix-1 and fix-2 both ended this way, on a prose
//! reply promising to report back once a background test run finished; no
//! `FixReport` was ever recovered, and `graph::Runner::review_loop` moved on
//! to the next round regardless.
//!
//! These tests pin `graph::Runner::continue_fix_report`: a bounded, session-
//! gated resume of the same seat, distinct from an unparsable *shape*
//! (`ask_json_wave`'s own nudge already covers judge/review/vote) or a
//! dropped stream (`Runner::resume_undelivered`), and gated purely on
//! `extract_json::<FixReport>` having failed — never on any wording.
mod common;

use common::{
    fixture_with_fix_report_always_lost, fixture_with_fix_report_lost,
    fixture_with_fixer_mentioning_waiting,
};
use magi::graph::Runner;
use magi::run::{ContinuationOutcome, RunStatus};

#[tokio::test]
async fn a_fix_report_lost_on_a_clean_turn_is_recovered_by_resuming_the_seat() {
    let _home = common::home_lock().await;
    let fx = fixture_with_fix_report_lost(_home, &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let round = state.reviews.first().expect("at least one review round");
    let fix = round.fix.as_ref().expect("round 1 had a fix step");
    assert!(
        fix.failed.is_none(),
        "the report was recovered on resume: {fix:?}"
    );
    let cont = fix.continuation.as_ref().expect("continuation recorded");
    assert_eq!(cont.outcome, ContinuationOutcome::Resumed, "{cont:?}");
    assert_eq!(cont.attempts, 1, "{cont:?}");

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "fix")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events
            .iter()
            .any(|m| m.contains("resuming the conversation")),
        "{events:?}"
    );
    assert!(
        events.iter().any(|m| m.contains("recovered after")),
        "{events:?}"
    );

    let art = state.dir().join("artifacts");
    assert!(
        art.join("fix-1-continue1.out").exists(),
        "the resumed call must have actually run"
    );
    assert!(
        !art.join("fix-1-continue2.out").exists(),
        "one recovered attempt must not spend a second"
    );

    // The recovered report unblocked the round: review found nothing left,
    // and the run reached a healthy terminal status rather than stalling.
    assert_eq!(
        state.status,
        RunStatus::Ready,
        "{}",
        magi::report::run(state)
    );
}

#[tokio::test]
async fn a_fix_report_that_never_recovers_gives_up_within_the_continuation_budget() {
    let _home = common::home_lock().await;
    let fx = fixture_with_fix_report_always_lost(_home, &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // Exhaustion stops the review loop outright rather than opening another
    // round: a second round would dispatch a fresh reviewer wave and, if
    // this round's blocking finding is still open, a fresh fix call — both
    // against the very worktree the exhausted continuation's CLI was just
    // in, with no way to confirm nothing it started is still running there.
    // See `Runner::review_loop`'s own check on `ContinuationOutcome`.
    assert_eq!(
        state.reviews.len(),
        1,
        "an exhausted continuation must end the loop, not open a second round: {:?}",
        state.reviews
    );
    for round in &state.reviews {
        let Some(fix) = &round.fix else { continue };
        let cont = fix
            .continuation
            .as_ref()
            .expect("continuation recorded even on exhaustion");
        assert_eq!(cont.outcome, ContinuationOutcome::Exhausted, "{cont:?}");
        // The cap, never more: proof the loop is bounded rather than retried
        // indefinitely against a seat that will never answer differently.
        assert_eq!(cont.attempts, 2, "{cont:?}");
        assert!(
            fix.failed
                .as_deref()
                .is_some_and(|e| e.contains("after 2 continuation(s)")),
            "{fix:?}"
        );
    }

    // Bounded in practice, not just in the record: no third continuation
    // call was ever made for any round.
    let art = state.dir().join("artifacts");
    for round in 1..=state.reviews.len() {
        assert!(
            art.join(format!("fix-{round}-continue1.out")).exists(),
            "round {round}: first continuation must have run"
        );
        assert!(
            art.join(format!("fix-{round}-continue2.out")).exists(),
            "round {round}: second continuation must have run"
        );
        assert!(
            !art.join(format!("fix-{round}-continue3.out")).exists(),
            "round {round}: the budget is exactly two, never a third call"
        );
    }
    assert!(
        !art.join("fix-2.out").exists() && !art.join("review-1-round2.out").exists(),
        "no second-round fix or reviewer call may ever run against a worktree an exhausted \
         continuation left unconfirmed"
    );

    // A run this stuck must never read like a clean pass: no fixer report
    // ever landed, so nothing here is "0 findings" dressed up as success.
    assert_ne!(state.status, RunStatus::Merged);
    assert!(state.reviews.iter().all(|r| !r.clean));
}

#[tokio::test]
async fn a_fix_report_lost_with_no_session_left_is_not_resumed_into_a_blank_prompt() {
    let _home = common::home_lock().await;
    let mut fx = fixture_with_fix_report_lost(_home, &["impl-A"]);
    // No session continuation at all: `has_context` is false for every seat
    // regardless of what the fixer's reply carried. Resuming anyway would
    // send `prompt::resume_incomplete` — which says nothing about the task —
    // into a brand-new conversation, worse than the ordinary failure this
    // already is.
    fx.config.graph.sessions = false;
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let round = state.reviews.first().expect("at least one review round");
    let fix = round.fix.as_ref().expect("round 1 had a fix step");
    let cont = fix.continuation.as_ref().expect("continuation recorded");
    assert_eq!(cont.outcome, ContinuationOutcome::NoSession, "{cont:?}");
    assert_eq!(cont.attempts, 0, "{cont:?}");
    assert!(fix.failed.is_some(), "{fix:?}");

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "fix")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events
            .iter()
            .any(|m| m.contains("no session left to resume into")),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|m| m.contains("resuming the conversation")),
        "{events:?}"
    );

    let art = state.dir().join("artifacts");
    assert!(
        !art.join("fix-1-continue1.out").exists(),
        "there is nothing to resume into, so no continuation call should have run"
    );
}

#[tokio::test]
async fn a_valid_first_try_report_mentioning_waiting_is_never_resumed() {
    let _home = common::home_lock().await;
    let fx = fixture_with_fixer_mentioning_waiting(_home);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // The completion contract is structural — did `FixReport` parse — never
    // a keyword match: a first-try report that parses fine, even one whose
    // own prose mentions having waited on a background build, must not be
    // read as an incomplete turn.
    let round = state.reviews.first().expect("at least one review round");
    let fix = round.fix.as_ref().expect("round 1 had a fix step");
    assert!(fix.failed.is_none(), "{fix:?}");
    assert!(
        fix.notes.contains("waiting"),
        "the mock's notes must actually mention waiting, or this test proves nothing: {fix:?}"
    );
    let cont = fix.continuation.as_ref().expect("continuation recorded");
    assert_eq!(cont.outcome, ContinuationOutcome::NotNeeded, "{cont:?}");
    assert_eq!(cont.attempts, 0, "{cont:?}");

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "fix")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        !events
            .iter()
            .any(|m| m.contains("resuming the conversation")),
        "{events:?}"
    );
    let art = state.dir().join("artifacts");
    assert!(
        !art.join("fix-1-continue1.out").exists(),
        "a normal final report must never trigger a continuation call"
    );
}
