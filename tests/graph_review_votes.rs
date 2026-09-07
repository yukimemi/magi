//! End-to-end: reviewer votes split, one round of reconsideration runs, and
//! the round's verdict lands in `run.json` even though nothing about the
//! split was ever blocking.
mod common;

use common::fixture_with_split_review_vote;
use magi::graph::Runner;
use magi::run::RunStatus;
use magi::verdict::ReviewVote;

#[tokio::test]
async fn a_split_vote_earns_one_round_of_reconsideration_and_a_recorded_verdict() {
    let home = common::home_lock().await;
    let fx = fixture_with_split_review_vote(home, "review-2");
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // The dissent was never blocking, so the round still gates clean on its
    // own — the vote machinery is additive, not a second gate.
    assert_eq!(state.reviews.len(), 1, "{:?}", state.reviews);
    let round = &state.reviews[0];
    assert!(round.clean, "a non-blocking dissent must not stop the gate");
    assert_eq!(round.blocking, 0);

    // Both seats cast an initial vote, and they disagreed.
    assert_eq!(round.reviews.len(), 2);
    let votes: Vec<ReviewVote> = round.reviews.iter().filter_map(|r| r.vote).collect();
    assert_eq!(votes.len(), 2, "both seats must have voted: {round:?}");
    assert!(
        round.vote_split,
        "approve and approve_with_findings must read as a split"
    );

    // The split earned exactly one round of reconsideration, from every seat
    // that had a vote to reconsider — not just the dissenter.
    assert_eq!(
        round.reconsideration.len(),
        2,
        "every seat with an initial vote reconsiders: {round:?}"
    );
    assert!(
        round
            .reconsideration
            .iter()
            .all(|rv| rv.vote.is_some() && rv.failed.is_none()),
        "{:?}",
        round.reconsideration
    );

    // The mock holds every seat's vote through reconsideration, so the
    // verdict is deterministic: the more cautious of the two.
    assert_eq!(round.verdict, Some(ReviewVote::ApproveWithFindings));

    // A split vote must never block a run that gate and e2e call green.
    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(state.status, RunStatus::Ready);
}
