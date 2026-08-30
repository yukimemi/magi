//! End-to-end: three candidates, three judges that agree, a clean review.
mod common;

use common::{Judges, fixture};
use magi::graph::{Runner, fold_run};
use magi::run::RunStatus;

#[tokio::test]
async fn a_unanimous_run_reaches_the_gate_without_deliberating() {
    let fx = fixture(Judges::Unanimous, false);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // Every candidate produced a commit on its own label-named branch.
    assert_eq!(state.candidates.len(), 3);
    for c in &state.candidates {
        assert!(c.viable(), "candidate {} was not viable: {c:?}", c.label);
        assert_eq!(c.commits, 1, "candidate {}", c.label);
        assert_eq!(c.branch, state.branch_for(c.label));
        assert!(
            !c.branch.contains(&c.agent),
            "branch {} names its author",
            c.branch
        );
    }
    // Labels are a permutation, so the agent behind a label is not positional.
    let mut labels: Vec<char> = state.candidates.iter().map(|c| c.label).collect();
    labels.sort_unstable();
    assert_eq!(labels, ['A', 'B', 'C']);

    // Judging: three independent rankings, each judge given its own order.
    assert_eq!(state.judgements.len(), 3);
    for j in &state.judgements {
        assert!(j.failed.is_none(), "judge {} failed: {j:?}", j.judge);
        assert_eq!(j.ranking.len(), 3);
        assert_eq!(j.order.len(), 3);
    }
    assert!(
        state
            .judgements
            .iter()
            .any(|j| j.order != state.judgements[0].order),
        "all three judges saw the candidates in the same order"
    );

    // Unanimous, so no deliberation was opened.
    let tally = state.tally.as_ref().expect("tally");
    assert!(tally.unanimous_initial);
    assert!(!tally.deliberated);
    assert!(state.deliberation.is_empty());
    assert_eq!(tally.winner, 'A');
    assert_eq!(tally.changed_votes, 0);
    assert_eq!(state.votes.len(), 3);
    assert!(state.votes.iter().all(|v| v.vote == Some('A')));

    // Losers folded, winner kept.
    for c in &state.candidates {
        if c.label == 'A' {
            assert!(!c.folded);
            assert!(c.worktree.is_dir(), "winner worktree is gone");
        } else {
            assert!(c.folded, "candidate {} was not folded", c.label);
            assert!(!c.worktree.exists(), "candidate {} still on disk", c.label);
        }
    }

    // One clean review round, green e2e, green gate, and no merge requested.
    assert_eq!(state.reviews.len(), 1);
    let round = &state.reviews[0];
    assert!(round.clean);
    assert_eq!(round.blocking, 0);
    assert_eq!(round.reviews.len(), 2);
    assert!(round.reviews.iter().all(|r| r.failed.is_none()));
    assert!(round.fix.is_none(), "a clean round must not run the fixer");
    assert!(round.e2e.iter().all(|o| o.ok()));

    assert_eq!(state.gate.len(), 1);
    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(state.status, RunStatus::Ready);
    let merge = state.merge.as_ref().expect("merge outcome");
    assert!(merge.ok);
    assert!(
        merge.detail.contains("merge --no-ff"),
        "mode none must print the command: {}",
        merge.detail
    );

    // Blindness: nothing shown to a judge named an author.
    let judge_prompt = std::fs::read_to_string(state.dir().join("artifacts/judge-1.prompt.md"))
        .expect("judge prompt artifact");
    for agent in ["alpha", "beta", "gamma"] {
        assert!(
            !judge_prompt.contains(agent),
            "judge prompt leaked agent id `{agent}`"
        );
    }

    // The run is resumable and idempotent: re-executing changes nothing.
    let mut again = Runner::resume(&state.id).expect("resume");
    again.execute().await.expect("re-execute");
    assert_eq!(again.state.reviews.len(), 1);
    assert_eq!(again.state.judgements.len(), 3);
    assert_eq!(again.state.votes.len(), 3);

    // Folding cleans up the winner too, on request.
    let mut state = again.state;
    let removed = fold_run(&mut state, true).await.expect("fold");
    assert!(!removed.is_empty());
    assert!(state.candidates.iter().all(|c| !c.worktree.exists()));
}
