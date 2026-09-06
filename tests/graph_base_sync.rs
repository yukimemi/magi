//! End-to-end: the base moves while a run is still working.
//!
//! `verify.e2e`, `verify.gate` and every reviewer read whatever is checked out
//! in the winner's worktree. Left alone that stays rooted at the commit the
//! run branched from, and a run takes long enough that the base usually moves
//! before it gets to the gate - a green run whose merge would revert whatever
//! landed elsewhere while it was thinking. `Runner::sync_to_base` is what
//! rebases the winner onto the current tip of `<remote>/<base>` before
//! anything verifies it; these tests are the two ways that can go.
mod common;

use common::{Judges, fixture, home_lock};
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

fn is_ancestor(repo: &std::path::Path, ancestor: &str, of: &str) -> bool {
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, of])
        .current_dir(repo)
        .status()
        .expect("spawn git")
        .success()
}

/// A bare `origin` for `fx.repo`, plus a second clone that can push to it
/// independently of anything `fx.repo` or its worktrees are doing -
/// simulating another pull request landing while a run is still working.
struct Origin {
    sideline: std::path::PathBuf,
}

fn wire_origin(fx: &common::Fixture) -> Origin {
    let origin = fx.tmp.path().join("origin.git");
    run_git(
        fx.tmp.path(),
        &[
            "clone",
            "--bare",
            "-q",
            fx.repo.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    run_git(
        &fx.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    let sideline = fx.tmp.path().join("sideline");
    run_git(
        fx.tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            sideline.to_str().unwrap(),
        ],
    );
    run_git(&sideline, &["config", "user.name", "other pr"]);
    run_git(&sideline, &["config", "user.email", "other@example.com"]);
    Origin { sideline }
}

/// Land one more commit on `origin`'s `main`, from the sideline clone.
fn land_on_origin(sideline: &std::path::Path, file: &str, content: &str) {
    std::fs::write(sideline.join(file), content).unwrap();
    run_git(sideline, &["add", "-A"]);
    run_git(sideline, &["commit", "-q", "-m", "another PR landed"]);
    run_git(sideline, &["push", "-q", "origin", "main"]);
}

#[tokio::test]
async fn the_gate_runs_on_a_tree_that_contains_what_landed_while_the_run_was_thinking() {
    let _home = home_lock().await;
    let fx = fixture(Judges::Unanimous, false);
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");

    // The run has branched (`base_commit` is already fixed); now, while it
    // works, somebody else's pull request lands on the base.
    land_on_origin(
        &origin.sideline,
        "upstream.txt",
        "landed while the run was thinking\n",
    );

    runner.execute().await.expect("execute");
    let state = &runner.state;

    let sync = state.base_sync.as_ref().expect("base sync recorded");
    assert!(
        sync.conflict.is_none(),
        "unexpected conflict: {:?}",
        sync.conflict
    );
    assert_eq!(sync.behind, 0, "resynced before the gate ran");
    assert!(sync.attempts >= 1, "a rebase must have happened");

    let winner = state.winner().expect("a winner");
    assert!(
        winner.worktree.join("upstream.txt").is_file(),
        "the tree the gate ran on must contain what landed on the base"
    );
    assert!(
        winner.worktree.join("note.txt").is_file(),
        "and the candidate's own work must survive the rebase"
    );

    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
    assert_eq!(state.status, RunStatus::Ready);

    // The `merge = "none"` guidance names `winner.branch`; proving it now
    // descends from the landing base's tip is what makes that guidance safe -
    // merging it will not delete `upstream.txt`.
    let tip = std::process::Command::new("git")
        .args(["rev-parse", "origin/main"])
        .current_dir(&fx.repo)
        .output()
        .expect("rev-parse");
    let tip = String::from_utf8_lossy(&tip.stdout).trim().to_owned();
    assert!(
        is_ancestor(&fx.repo, &tip, &winner.branch),
        "the winner branch must descend from the current landing base"
    );
}

#[tokio::test]
async fn a_base_that_conflicts_stops_the_run_without_a_review_round_or_a_fixer() {
    let _home = home_lock().await;
    let fx = fixture(Judges::Unanimous, false);
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");

    // The base gains its own, different `note.txt` - the same path the
    // candidate creates - so replaying the candidate's commit cannot avoid a
    // conflict.
    land_on_origin(
        &origin.sideline,
        "note.txt",
        "a conflicting note from upstream\n",
    );

    runner.execute().await.expect("execute");
    let state = &runner.state;

    let sync = state.base_sync.as_ref().expect("base sync recorded");
    let why = sync.conflict.as_ref().expect("a conflict must be recorded");
    assert!(
        why.to_lowercase().contains("conflict"),
        "the reason is what git said: {why}"
    );
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(sync.behind > 0, "the lag was recorded before the attempt");

    // A conflict is a decision, not a fix round: nothing past it ran.
    assert!(state.reviews.is_empty(), "{:?}", state.reviews);
    assert!(state.gate.is_empty());
    let log = std::fs::read_to_string(state.dir().join("artifacts").join("attribution.log"))
        .unwrap_or_default();
    assert!(
        !log.lines()
            .any(|l| matches!(l.split_whitespace().nth(1), Some("review") | Some("fix"))),
        "reviewers and the fixer must not have run: {log}"
    );

    // The conflicted tree and branch are left for a person, not discarded.
    let winner = state.winner().expect("a winner");
    assert!(winner.worktree.exists(), "the worktree is kept");
    assert!(
        std::process::Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", winner.branch),
            ])
            .current_dir(&fx.repo)
            .status()
            .expect("show-ref")
            .success(),
        "the branch is kept"
    );
}
