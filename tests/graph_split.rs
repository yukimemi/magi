//! End-to-end: judges disagree, deliberate, then vote privately; the winner
//! goes through a real review + fix round before the gate.
mod common;

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::run::RunStatus;

#[tokio::test]
async fn a_split_run_deliberates_then_collects_private_votes() {
    let fx = fixture(Judges::Split, true);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // Three different first choices.
    let tops: Vec<char> = state.judgements.iter().map(|j| j.ranking[0]).collect();
    assert_eq!(tops.len(), 3);
    assert!(!tops.iter().all(|t| *t == tops[0]), "expected a split");

    let tally = state.tally.as_ref().expect("tally");
    assert!(!tally.unanimous_initial);
    assert!(tally.deliberated, "a split must open deliberation");

    // One deliberation round, one turn per judge, each with a position.
    assert_eq!(state.deliberation.len(), 1);
    let round = &state.deliberation[0];
    assert_eq!(round.turns.len(), 3);
    assert!(round.turns.iter().all(|t| t.tentative == Some('B')));
    assert!(round.turns.iter().all(|t| !t.body.trim().is_empty()));

    // Deliberation transcripts must be anonymous: a judge sees "Judge 2", not
    // an agent id.
    let delib_prompt =
        std::fs::read_to_string(state.dir().join("artifacts/delib-1-judge-2.prompt.md"))
            .expect("deliberation prompt artifact");
    assert!(delib_prompt.contains("Judge 1"));
    assert!(delib_prompt.contains("(you)"));
    for agent in ["alpha", "beta", "gamma"] {
        assert!(
            !delib_prompt.contains(agent),
            "deliberation prompt leaked `{agent}`"
        );
    }

    // Final votes are collected per judge, privately, and the count is
    // mechanical.
    let vote_prompt = std::fs::read_to_string(state.dir().join("artifacts/vote-judge-1.prompt.md"))
        .expect("vote prompt artifact");
    assert!(vote_prompt.contains("not shown to the other judges"));
    assert_eq!(state.votes.len(), 3);
    assert!(state.votes.iter().all(|v| v.vote == Some('B')));
    assert_eq!(tally.winner, 'B');
    assert_eq!(tally.first_choice[&'B'], 3);
    assert!(tally.unanimous_final);
    // Judges 1 and 3 moved off their initial first choice; judge 2 did not.
    assert_eq!(tally.changed_votes, 2);
    assert!(tally.tie_break.is_none());

    // Review: round 1 raises a blocker, the fixer commits, round 2 is clean.
    assert_eq!(state.reviews.len(), 2, "{:?}", state.reviews);
    let first = &state.reviews[0];
    assert_eq!(first.reviews.len(), 2);
    assert_eq!(first.blocking, 2, "both reviewers raised the blocker");
    assert!(!first.clean);
    let fix = first.fix.as_ref().expect("fixer ran");
    assert!(fix.committed, "the fixer must land a commit");
    assert_eq!(fix.addressed.len(), 1);
    assert!(
        fix.addressed[0].starts_with("R1-"),
        "finding ids are magi's: {:?}",
        fix.addressed
    );

    let second = &state.reviews[1];
    assert!(second.clean);
    assert_eq!(second.blocking, 0);
    assert_ne!(
        second.head, first.head,
        "round 2 must review the fixed tree"
    );

    // Findings carry ids assigned by magi, not by the agent.
    let ids: Vec<&str> = first
        .reviews
        .iter()
        .flat_map(|r| r.findings.iter())
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(ids, ["R1-1-1", "R1-2-1"]);

    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(state.status, RunStatus::Ready);

    // The winner's worktree really contains both the implementation and the fix.
    let winner = state.winner().expect("winner");
    assert!(winner.worktree.join("note.txt").is_file());
    assert!(winner.worktree.join("fixed.txt").is_file());
    assert!(winner.commits >= 1);
}
