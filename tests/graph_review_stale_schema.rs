//! End-to-end: a stale reconsideration reply cannot become an empty review.
mod common;

use common::{
    fixture_with_empty_reject_revote, fixture_with_rejected_minor_review,
    fixture_with_stale_review_reply, fixture_with_stale_revote_reply,
};
use magi::config::IncompleteReviewPolicy;
use magi::graph::Runner;
use magi::run::RunStatus;
use magi::verdict::ReviewVote;

#[tokio::test]
async fn stale_revote_shape_is_retried_then_remains_an_incomplete_review() {
    let home = common::home_lock().await;
    let mut fx = fixture_with_stale_review_reply(home);
    // `warn` still permits a genuine timeout to gate; a stale structured
    // reply is different and must never be promoted to a clean review.
    fx.config.graph.incomplete_review = IncompleteReviewPolicy::Warn;
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.reviews.len(), 2, "{:#?}", state.reviews);
    let first = &state.reviews[0];
    assert!(first.vote_split, "the stale reply follows reconsideration");
    assert!(
        first.reconsideration.iter().all(|r| r.vote.is_some()),
        "reconsideration itself was valid: {:#?}",
        first.reconsideration
    );

    let second = &state.reviews[1];
    let stale = &second.reviews[1];
    assert!(stale.failed.is_some(), "{stale:#?}");
    assert!(
        stale
            .failed
            .as_deref()
            .is_some_and(|why| why.contains("expected shape")),
        "the stale schema must be recorded, not converted to no findings: {stale:#?}"
    );
    assert!(second.incomplete());
    assert!(
        !second.clean,
        "an unresolved reject can never read as clean"
    );
    assert!(
        state.gate.is_empty(),
        "an incomplete review must not enter gate"
    );
    assert_eq!(state.status, RunStatus::Blocked);
}

#[tokio::test]
async fn stale_review_shape_during_reconsideration_is_an_incomplete_panel() {
    let home = common::home_lock().await;
    let fx = fixture_with_stale_revote_reply(home);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let round = &runner.state.reviews[0];

    assert!(round.incomplete(), "{round:#?}");
    assert!(!round.clean, "{round:#?}");
    assert!(
        round.reconsideration[1].failed.is_some(),
        "{:#?}",
        round.reconsideration
    );
    assert_eq!(runner.state.status, RunStatus::Blocked);
}

#[tokio::test]
async fn actionable_non_blocking_reject_is_open_not_clean() {
    let home = common::home_lock().await;
    let fx = fixture_with_rejected_minor_review(home);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let round = &runner.state.reviews[0];

    assert_eq!(round.blocking, 0, "{round:#?}");
    assert_eq!(round.verdict, Some(ReviewVote::Reject));
    assert!(
        !round.clean,
        "a reject cannot be reported clean: {round:#?}"
    );
    assert!(
        round.fix.is_none(),
        "a non-blocking reject is not a fixer task"
    );
    assert_eq!(runner.state.status, RunStatus::Blocked);
}

#[tokio::test]
async fn empty_reason_reject_revote_is_not_clean_under_warn() {
    let home = common::home_lock().await;
    let mut fx = fixture_with_empty_reject_revote(home);
    fx.config.graph.incomplete_review = IncompleteReviewPolicy::Warn;
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let round = &runner.state.reviews[0];

    assert!(round.incomplete(), "{round:#?}");
    assert!(!round.clean, "{round:#?}");
    assert!(
        round.reconsideration[1]
            .failed
            .as_deref()
            .is_some_and(|why| why.contains("must include a reason")),
        "{:#?}",
        round.reconsideration
    );
    assert!(runner.state.gate.is_empty());
    assert_eq!(runner.state.status, RunStatus::Blocked);
}
