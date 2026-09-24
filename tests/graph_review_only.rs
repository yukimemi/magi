//! End-to-end: `magi review <branch>` on work that already exists.
//!
//! The point of the test is that no competition happens — no implementation, no
//! judging, no vote — and yet the review + verification + gate loop runs to a
//! decision. That property is not enforced by a flag anywhere; it falls out of
//! a run having a single viable candidate and a tally that is already decided,
//! so it is exactly the kind of thing that would rot silently.
mod common;

use common::{
    Judges, fixture, fixture_with_quota, fixture_with_review_seat_that_recovers_on_retry,
    fixture_with_silent_review_seat,
};
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

common::e2e! {
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
}

common::e2e! {
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
        let missing = round
            .reviews
            .iter()
            .find(|r| r.reviewer == 2)
            .expect("review-2's record is still in the round");
        assert!(
            missing.failed.is_some(),
            "a seat that never produced a usable answer is `failed`, whatever \
             `attempts` says: {missing:?}"
        );
    }
    // Nothing was ever raised to fix, so the fixer never ran.
    assert!(state.reviews.iter().all(|r| r.fix.is_none()));
    assert_eq!(state.status, RunStatus::Blocked);
}
}

common::e2e! {
/// The addendum's third gap: a seat that times out and then answers on
/// `ask_json_wave`'s nudge must read as recovered, not as silent — the two
/// looked identical in history (a retry event and nothing else) before
/// `ReviewRecord::attempts` existed.
async fn a_reviewer_that_only_answers_on_retry_is_recorded_as_recovered_not_silent() {
    let _home = common::home_lock().await;
    let mut fx = fixture_with_review_seat_that_recovers_on_retry(_home, &["review-2"]);
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
    let round = &state.reviews[0];
    assert_eq!(round.answered, 2, "both seats answered, one on a retry");
    assert!(!round.incomplete());
    let recovered = round
        .reviews
        .iter()
        .find(|r| r.reviewer == 2)
        .expect("review-2's record is still in the round");
    assert!(
        recovered.failed.is_none(),
        "a seat that did eventually answer is not `failed`: {recovered:?}"
    );
    assert!(
        recovered.attempts > 0,
        "recorded attempts must show the retry actually happened: {recovered:?}"
    );
    assert!(
        state
            .events
            .iter()
            .any(|e| e.node == "review" && e.message.contains("retry 1")),
        "the retry itself is also visible in the event log: {:?}",
        state.events
    );
}
}

common::e2e! {
async fn a_reviewer_rate_limited_by_quota_gates_clean_on_the_answered_panel() {
    // The field report this closes: a rate-limited reviewer must not be
    // waited on round after round hoping its session limit lifts — the
    // limit will not lift by the next round any more than it lifted between
    // this round's attempt and the retry `ask_json_wave` already refuses to
    // send. The round decides on whoever did answer instead.
    let _home = common::home_lock().await;
    let mut fx = fixture_with_quota(_home, &["review-2"]);
    // A generous budget: if the fix regresses to waiting for full quorum,
    // this would still resolve eventually and the `reviews.len() == 1`
    // assertion below is what would catch it, not a timeout.
    fx.config.graph.review_rounds = 5;

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

    // Decided in the very first round, not after exhausting the budget
    // hoping review-2 comes back.
    assert_eq!(state.reviews.len(), 1, "{:?}", state.reviews);
    let round = &state.reviews[0];
    assert_eq!(round.answered, 1);
    assert_eq!(round.expected, 2);
    assert!(round.incomplete(), "review-2 never answered");
    assert!(
        round.clean,
        "a quota-lost seat must not block a decision the rest of the panel already made: \
         {round:?}"
    );

    // The exclusion is still on the record, not swallowed: `magi show`
    // reads both of these to report who was missing and why.
    let missing = round
        .reviews
        .iter()
        .find(|r| r.reviewer == 2)
        .expect("review-2's own record");
    assert!(
        missing
            .failed
            .as_deref()
            .is_some_and(|why| why.contains("rate limited")),
        "{missing:?}"
    );
    assert_eq!(state.quota.len(), 1, "{:?}", state.quota);
    assert_eq!(state.quota[0].seat, "review-2");

    assert_eq!(state.status, RunStatus::Ready);
}
}

common::e2e! {
async fn a_panel_lost_entirely_to_quota_still_waits_instead_of_deciding_on_nobody() {
    // The other half of the same fix: excluding a rate-limited seat from
    // quorum must never go so far as calling a round clean with nobody left
    // to have reviewed it. With every seat quota'd this falls back to the
    // pre-existing behaviour — wait out the round budget, then `Blocked` —
    // exactly as an ordinary (non-quota) silent panel already does.
    let _home = common::home_lock().await;
    let mut fx = fixture_with_quota(_home, &["review-1", "review-2"]);
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
    let round = &state.reviews[0];
    assert_eq!(round.answered, 0);
    assert_eq!(round.expected, 2);
    assert!(
        !round.clean,
        "a panel with nobody left to review must never gate clean: {round:?}"
    );
    assert_eq!(state.quota.len(), 2, "{:?}", state.quota);
    assert_eq!(state.status, RunStatus::Blocked);
}
}

common::e2e! {
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
}
