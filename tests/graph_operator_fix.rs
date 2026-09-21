//! End-to-end: `graph::Runner::fix_selected` — an operator routing specific,
//! already-recorded review findings to a fixer on the same branch, outside
//! the normal review round sequence.
mod common;

use common::fixture_with_split_review_vote;
use magi::graph::Runner;
use magi::run::{OperatorFixOutcome, RunState, RunStatus};

/// Both reviewer seats agree `approve_with_findings` with one non-blocking
/// finding each — a clean round (nothing blocked merge) that still leaves
/// two minor findings nobody acted on, exactly the "8b21" scenario this
/// feature exists for.
///
/// Returns the fixture alongside the runner: the fixture's `TempDir` deletes
/// the repository on drop, so a caller that let it go out of scope here
/// would have `fix_selected` running `git` against a directory that no
/// longer exists.
async fn ready_run_with_two_minor_findings(home: common::HomeGuard) -> (common::Fixture, Runner) {
    let fx = fixture_with_split_review_vote(home, "review-1,review-2");
    let mut runner = Runner::start(&fx.repo, "add retries".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    assert_eq!(runner.state.status, RunStatus::Ready, "{:?}", runner.state);
    assert_eq!(runner.state.reviews.len(), 1);
    assert!(runner.state.reviews[0].clean);
    assert_eq!(runner.state.last_round_findings().len(), 2);
    (fx, runner)
}

fn git(repo: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn a_selected_minor_finding_is_fixed_and_reverified_leaving_the_other_untouched() {
    let home = common::home_lock().await;
    let (_fx, mut runner) = ready_run_with_two_minor_findings(home).await;
    let ids: Vec<String> = runner
        .state
        .last_round_findings()
        .iter()
        .map(|f| f.id.clone())
        .collect();
    let (picked, left) = (ids[0].clone(), ids[1].clone());

    runner
        .fix_selected(
            std::slice::from_ref(&picked),
            "operator: worth fixing before the next release",
            false,
        )
        .await
        .expect("fix_selected");

    // The original round is untouched — no fix folded into it, no rewritten
    // severity or vote.
    assert!(runner.state.reviews[0].fix.is_none());
    assert!(runner.state.reviews[0].clean);

    assert_eq!(runner.state.operator_fixes.len(), 1);
    let request = &runner.state.operator_fixes[0];
    assert_eq!(
        request.reason,
        "operator: worth fixing before the next release"
    );
    assert!(!request.stale);
    assert_eq!(
        request.findings.len(),
        1,
        "only the picked id, never the other"
    );
    assert_eq!(request.findings[0].id, picked);
    assert_eq!(request.findings[0].outcome, OperatorFixOutcome::Addressed);

    let fix = request.fix.as_ref().expect("fix record");
    assert!(fix.committed, "the fixer must have produced a real commit");
    assert_eq!(fix.addressed, vec![picked.clone()]);
    assert!(
        !fix.addressed.contains(&left),
        "the unselected finding must never be reported as addressed"
    );

    let follow_up_id = request
        .follow_up_review_run
        .clone()
        .expect("a real change opens a follow-up review");
    let follow_up = RunState::load(&follow_up_id).expect("load follow-up run");
    assert_eq!(follow_up.status, RunStatus::Ready, "{follow_up:?}");
    assert!(
        follow_up.gate.iter().all(|o| o.ok()),
        "the follow-up run must have actually gated: {:?}",
        follow_up.gate
    );
}

#[tokio::test]
async fn an_unknown_finding_id_refuses_the_whole_request() {
    let home = common::home_lock().await;
    let (_fx, mut runner) = ready_run_with_two_minor_findings(home).await;
    let before = runner.state.operator_fixes.len();

    let err = runner
        .fix_selected(&["R99-9-9".to_owned()], "bogus id", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("R99-9-9"), "{err}");
    assert_eq!(
        runner.state.operator_fixes.len(),
        before,
        "a refused request must not be recorded"
    );
}

#[tokio::test]
async fn an_empty_reason_is_refused() {
    let home = common::home_lock().await;
    let (_fx, mut runner) = ready_run_with_two_minor_findings(home).await;
    let id = runner.state.last_round_findings()[0].id.clone();

    let err = runner.fix_selected(&[id], "   ", false).await.unwrap_err();
    assert!(err.to_string().contains("reason"), "{err}");
    assert!(runner.state.operator_fixes.is_empty());
}

#[tokio::test]
async fn a_run_that_has_not_concluded_review_refuses_a_targeted_fix() {
    let home = common::home_lock().await;
    let (_fx, mut runner) = ready_run_with_two_minor_findings(home).await;
    let id = runner.state.last_round_findings()[0].id.clone();

    runner.state.status = RunStatus::Reviewing;
    let err = runner
        .fix_selected(std::slice::from_ref(&id), "too early", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("reviewing"), "{err}");

    runner.state.status = RunStatus::Merged;
    let err = runner
        .fix_selected(&[id], "already landed", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("merged"), "{err}");
    assert!(err.to_string().contains("magi review"), "{err}");
}

#[tokio::test]
async fn a_stale_finding_is_refused_unless_the_operator_allows_it() {
    let home = common::home_lock().await;
    let (_fx, mut runner) = ready_run_with_two_minor_findings(home).await;
    let id = runner.state.last_round_findings()[0].id.clone();
    let winner = runner.state.winner().cloned().expect("winner");

    // Move the branch out from under the recorded finding, without going
    // through magi at all — exactly what a person pushing by hand would do.
    git(
        &winner.worktree,
        &["commit", "--allow-empty", "-m", "unrelated later work"],
    );

    let err = runner
        .fix_selected(std::slice::from_ref(&id), "stale attempt", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("moved"), "{err}");
    assert!(runner.state.operator_fixes.is_empty());

    runner
        .fix_selected(std::slice::from_ref(&id), "stale attempt, allowed", true)
        .await
        .expect("allow_stale must let it through");
    let request = &runner.state.operator_fixes[0];
    assert!(request.stale);
    assert!(request.allow_stale);
    assert_eq!(request.findings[0].id, id);
}
