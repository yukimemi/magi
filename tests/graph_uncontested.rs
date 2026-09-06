//! End-to-end: `graph.candidates = 1`, the default this repo now ships.
//!
//! Judging is skipped because there is nothing to compare, and that must read
//! as "no panel was needed" — not as the same collapsed-panel report a real
//! stall produces. The two must stay distinguishable in the record itself,
//! not just in how the report happens to phrase things.
mod common;

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::report;
use magi::run::RunStatus;

#[tokio::test]
async fn a_single_candidate_run_is_uncontested_not_collapsed() {
    let mut fx = fixture(Judges::Unanimous, false);
    fx.config.graph.candidates = 1;

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.candidates.len(), 1);
    assert!(state.judgements.is_empty(), "no panel was asked");
    assert!(state.votes.is_empty());

    let tally = state.tally.as_ref().expect("a decided tally");
    assert_eq!(tally.winner, 'A');
    assert_eq!(
        tally.judges, 0,
        "no panel sat, so it must not carry the roster size"
    );
    assert!(tally.met_quorum);
    assert!(
        tally.uncontested.is_some(),
        "the record must say why no panel was asked"
    );

    assert_eq!(state.status, RunStatus::Ready);

    // The report must not describe this as a panel that fell apart.
    let text = report::run(state);
    assert!(!text.contains("0/3"), "{text}");
    assert!(!text.contains("no usable ranking"), "{text}");
    assert!(!text.contains("still split"), "{text}");
    assert!(!text.contains("BELOW QUORUM"), "{text}");
}
