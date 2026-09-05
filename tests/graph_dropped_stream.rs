//! A CLI that hangs up on its own stream after billed work is not the same as
//! an agent that produced nothing. The implement node must resume such a seat
//! once, but only when it still has a session to resume into — never send the
//! context-free resume prompt to a brand-new conversation.

mod common;

use common::{fixture_with_dropped_deliberation, fixture_with_dropped_stream};
use magi::graph::Runner;
use magi::run::RunStatus;

#[tokio::test]
async fn a_dropped_stream_is_resumed_once_and_the_candidate_recovers() {
    let _home = common::home_lock().await;
    let fx = fixture_with_dropped_stream(&["impl-B"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // The dropped seat's own event is recorded, and a resumed call actually
    // went out — this is the wiring `wave()` used to lose: `dropped` was set
    // but never reached `resume_undelivered` because `usable()` had already
    // routed the result to `AgentOutcome::Failed`.
    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events
            .iter()
            .any(|m| m.contains("resuming the conversation")),
        "{events:?}"
    );
    let art = state.dir().join("artifacts");
    assert!(
        art.join("impl-B-resume.out").exists(),
        "the resumed call must have run"
    );

    // And the candidate recovered: the resumed reply falls through to the
    // mock's ordinary implementation branch, so it is neither empty nor
    // failed.
    let b = state
        .candidates
        .iter()
        .find(|c| c.label == 'B')
        .expect("candidate B");
    assert!(!b.empty, "{b:?}");
    assert!(b.failed.is_none(), "{b:?}");
    assert!(b.commits > 0, "{b:?}");
}

#[tokio::test]
async fn a_dropped_stream_with_no_session_left_is_not_resumed_into_a_blank_prompt() {
    let _home = common::home_lock().await;
    let mut fx = fixture_with_dropped_stream(&["impl-B"]);
    // No session continuation at all: `has_context` is false for every seat
    // regardless of what the dropped reply carried. Resuming anyway would
    // send the context-free `resume_after_drop` prompt to a brand-new
    // conversation — a wasted call that is worse than just leaving this as
    // the ordinary failure it already is.
    fx.config.graph.sessions = false;
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events
            .iter()
            .any(|m| m.contains("no session left to resume")),
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
        !art.join("impl-B-resume.out").exists(),
        "there is nothing to resume into, so no retry call should have run"
    );

    // Left as the ordinary failure it always was: no change, no commits.
    let b = state
        .candidates
        .iter()
        .find(|c| c.label == 'B')
        .expect("candidate B");
    assert!(b.empty, "{b:?}");
    assert_eq!(b.commits, 0, "{b:?}");
}

#[tokio::test]
async fn a_dropped_deliberation_turn_is_skipped_not_read_as_the_judges_position() {
    let _home = common::home_lock().await;
    // Judge 1's *deliberation round* reply is the dropped-stream shape, with a
    // non-zero exit code — the exact combination that used to reach
    // `AgentOutcome::Ok` (any exit code, so long as `work_undelivered()` was
    // true) and get read by `deliberate()` as if the CLI's raw error JSON were
    // judge 1's argued position.
    let fx = fixture_with_dropped_deliberation(&["judge-1"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.deliberation.len(), 1);
    let round = &state.deliberation[0];
    // Judge 1 contributed no turn at all — not one whose body is the CLI's
    // error JSON.
    assert_eq!(round.turns.len(), 2, "{:?}", round.turns);
    assert!(
        round.turns.iter().all(|t| t.judge != 1),
        "{:?}",
        round.turns
    );
    for t in &round.turns {
        assert!(
            !t.body.contains("conversation_id") && !t.body.contains("subscriber fell behind"),
            "a judge's position must never be the CLI's raw error JSON: {:?}",
            t.body
        );
    }

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "deliberate")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events
            .iter()
            .any(|m| m.contains("judge 1 skipped") && m.contains("dropped the stream")),
        "{events:?}"
    );

    // The rest of the run is unaffected: judge 1 still ranked and voted, so
    // the run reaches a healthy `Ready` rather than stalling over one skipped
    // deliberation turn.
    assert_eq!(
        state.status,
        RunStatus::Ready,
        "{}",
        magi::report::run(state)
    );
}
