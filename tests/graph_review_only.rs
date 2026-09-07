//! End-to-end: `magi review <branch>` on work that already exists.
//!
//! The point of the test is that no competition happens — no implementation, no
//! judging, no vote — and yet the review + verification + gate loop runs to a
//! decision. That property is not enforced by a flag anywhere; it falls out of
//! a run having a single viable candidate and a tally that is already decided,
//! so it is exactly the kind of thing that would rot silently.
mod common;

use common::{Judges, fixture, fixture_with_silent_review_seat};
use magi::graph::Runner;
use magi::run::RunStatus;

fn run_git(repo: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn a_review_only_run_reviews_an_existing_branch_without_competing() {
    // `fixture()` sets a process-wide home directory, so any two tests in
    // this binary that build one must not run concurrently — every test here
    // must take this lock, not just the slow one, or the two that don't will
    // still race each other underneath it.
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);

    // Hand-written work on a branch: the thing `magi run` would never produce.
    run_git(&fx.repo, &["checkout", "-q", "-b", "feat/by-hand"]);
    std::fs::write(fx.repo.join("note.txt"), "written by a human\n").unwrap();
    run_git(&fx.repo, &["add", "-A"]);
    run_git(&fx.repo, &["commit", "-q", "-m", "add note.txt by hand"]);
    run_git(&fx.repo, &["checkout", "-q", "main"]);

    let mut runner = Runner::review(&fx.repo, "feat/by-hand", fx.config.clone())
        .await
        .expect("open a review-only run");

    // Before anything runs: one candidate, credited to nobody, already the
    // winner.
    assert_eq!(runner.state.candidates.len(), 1);
    let c = &runner.state.candidates[0];
    assert_eq!(c.label, 'A');
    assert_eq!(c.branch, "feat/by-hand");
    assert_eq!(c.commits, 1);
    assert!(
        !fx.config.agents.iter().any(|a| a.id == c.agent),
        "the candidate must not be attributed to a roster agent, got {}",
        c.agent
    );
    let tally = runner.state.tally.as_ref().expect("a decided tally");
    assert_eq!(tally.winner, 'A');
    assert_eq!(tally.rankings, 0, "nothing was ranked");

    runner.execute().await.expect("execute");
    let state = &runner.state;

    // No competition took place.
    assert!(state.judgements.is_empty(), "nobody judged");
    assert!(state.deliberation.is_empty(), "nobody deliberated");
    assert!(state.votes.is_empty(), "nobody voted");

    // The cheap half did: the fixture's reviewers raise one blocker on round 1,
    // the fixer commits, round 2 is clean.
    assert_eq!(state.reviews.len(), 2, "{:?}", state.reviews);
    let first = &state.reviews[0];
    assert_eq!(first.reviews.len(), 2);
    assert!(!first.clean);
    let fix = first.fix.as_ref().expect("the fixer ran");
    assert!(fix.committed, "fixes must land on the branch");
    assert!(state.reviews[1].clean);

    assert!(state.gate.iter().all(|o| o.ok()));
    assert_eq!(state.status, RunStatus::Ready);

    // The fix landed on the branch under review, not on a detached head.
    let winner = state.winner().expect("winner");
    assert!(winner.worktree.join("fixed.txt").is_file());
    let head = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&winner.worktree)
        .output()
        .expect("spawn git");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "feat/by-hand",
        "the review worktree must stay attached to the branch"
    );
}

#[tokio::test]
async fn a_reviewer_that_never_answered_is_never_reported_as_a_clean_round() {
    let _home = common::home_lock().await;
    let mut fx = fixture_with_silent_review_seat(_home, &["review-2"]);
    // One round is enough to exercise "budget exhausted while incomplete".
    fx.config.graph.review_rounds = 1;

    run_git(&fx.repo, &["checkout", "-q", "-b", "feat/by-hand"]);
    std::fs::write(fx.repo.join("note.txt"), "written by a human\n").unwrap();
    run_git(&fx.repo, &["add", "-A"]);
    run_git(&fx.repo, &["commit", "-q", "-m", "add note.txt by hand"]);
    run_git(&fx.repo, &["checkout", "-q", "main"]);

    let mut runner = Runner::review(&fx.repo, "feat/by-hand", fx.config.clone())
        .await
        .expect("open a review-only run");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.reviews.len(), 1, "{:?}", state.reviews);
    for round in &state.reviews {
        // review-1 answered and found nothing; review-2 never came back. That
        // must never look the same as a panel that read the patch and passed
        // it: half the panel is not evidence of anything.
        assert_eq!(round.answered, 1);
        assert_eq!(round.expected, 2);
        assert!(round.incomplete());
        assert_eq!(round.blocking, 0, "the seat that did answer found nothing");
        assert!(
            !round.clean,
            "a round missing half its panel must never be reported clean: {round:?}"
        );
    }
    // Nothing was ever raised to fix, so the fixer never ran.
    assert!(state.reviews.iter().all(|r| r.fix.is_none()));
    assert_eq!(state.status, RunStatus::Blocked);
}

#[tokio::test]
async fn review_refuses_the_cases_that_cannot_mean_anything() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, false);

    let missing = Runner::review(&fx.repo, "no/such/branch", fx.config.clone()).await;
    assert!(missing.is_err(), "a branch that does not exist");

    // The base branch itself has nothing to review against.
    let base = Runner::review(&fx.repo, "main", fx.config.clone()).await;
    assert!(base.is_err(), "reviewing the base branch");

    // A branch with no commits beyond base is not a change.
    run_git(&fx.repo, &["branch", "feat/empty"]);
    let empty = Runner::review(&fx.repo, "feat/empty", fx.config.clone()).await;
    assert!(empty.is_err(), "a branch with no commits of its own");
}
