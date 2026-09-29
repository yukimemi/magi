//! A retried task reopens a branch an earlier attempt still has checked out.
//!
//! Git holds a branch in one worktree at a time, so the review could not start
//! and an agent asked the operator whether the old worktree could go. These
//! tests pin when magi releases that worktree itself (a clean, superseded,
//! not-running run) and when it must not (dirty, live, not superseded).
mod common;

use std::path::{Path, PathBuf};

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::handover::Takeover;
use magi::run::{Candidate, RunState, RunStatus};

const BRANCH: &str = "magi/f82f/A";

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// An earlier run, mid-flight, whose candidate worktree holds `BRANCH` with one
/// commit on it. Returns the saved run and the worktree path.
fn old_run(fx: &common::Fixture) -> (RunState, PathBuf) {
    let wt = fx.tmp.path().join("old-wt").join("cand-A");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    run_git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            BRANCH,
            &wt.to_string_lossy(),
            "main",
        ],
    );
    std::fs::write(wt.join("note.txt"), "by the old run\n").unwrap();
    run_git(&wt, &["add", "-A"]);
    run_git(&wt, &["commit", "-q", "-m", "old run work"]);

    let base = run_git(&fx.repo, &["rev-parse", "main"]);
    let mut state = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        base,
        "task".to_owned(),
        fx.config.clone(),
    );
    state.status = RunStatus::Gating;
    state.candidates.push(Candidate {
        index: 0,
        label: 'A',
        agent: "(existing branch)".to_owned(),
        branch: BRANCH.to_owned(),
        worktree: wt.clone(),
        summary: String::new(),
        stat: String::new(),
        files: 1,
        commits: 1,
        empty: false,
        failed: None,
        verified_noop: None,
        duration_ms: 0,
        folded: false,
    });
    state.save().expect("save the old run");
    (state, wt)
}

fn takeover(earlier: &[&RunState]) -> Takeover {
    Takeover {
        earlier: earlier.iter().map(|s| s.id.clone()).collect(),
        home: magi::run::home(),
    }
}

common::e2e! {
async fn a_clean_superseded_run_has_its_worktree_released() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (old, wt) = old_run(&fx);
    let tip = run_git(&fx.repo, &["rev-parse", BRANCH]);

    let runner = Runner::review_taking_over(
        &fx.repo,
        BRANCH,
        fx.config.clone(),
        Some(takeover(&[&old])),
    )
    .await
    .expect("the review takes the branch over");

    assert!(!wt.exists(), "the old worktree is gone");
    assert_eq!(
        run_git(&fx.repo, &["rev-parse", BRANCH]),
        tip,
        "the branch itself is untouched"
    );
    let old = RunState::load_under(&old.id, &magi::run::home()).unwrap();
    assert_eq!(old.released_to.as_deref(), Some(runner.state.id.as_str()));
    assert!(old.released());
    assert!(old.candidates[0].folded);
    assert_eq!(old.status, RunStatus::Gating, "its status is not rewritten");
    assert!(
        Runner::resume(&old.id).is_err(),
        "a released run cannot be resumed"
    );
    assert!(
        runner.state.candidates[0].worktree.exists(),
        "the new run holds the branch now"
    );
}
}

common::e2e! {
async fn a_dirty_worktree_is_refused_and_left_alone() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (old, wt) = old_run(&fx);
    std::fs::write(wt.join("scratch.txt"), "unsaved thoughts\n").unwrap();

    let err = Runner::review_taking_over(
        &fx.repo,
        BRANCH,
        fx.config.clone(),
        Some(takeover(&[&old])),
    )
    .await
    .err()
    .expect("a dirty worktree must be refused");

    assert!(
        err.downcast_ref::<magi::handover::Refused>().is_some(),
        "a refusal is typed so the queue holds the task instead of failing it"
    );
    let text = format!("{err:#}");
    assert!(text.contains("uncommitted"), "{text}");
    assert!(text.contains("dirty") && text.contains("gating"), "{text}");
    assert!(wt.join("scratch.txt").exists(), "the worktree is untouched");
    let old = RunState::load_under(&old.id, &magi::run::home()).unwrap();
    assert!(!old.released() && !old.candidates[0].folded);
}
}

common::e2e! {
async fn a_run_that_is_not_superseded_is_left_alone() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (old, wt) = old_run(&fx);

    // Not among the task's earlier attempts, e.g. another task's run.
    let result =
        Runner::review_taking_over(&fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[]))).await;

    assert!(result.is_err(), "git still refuses to share the branch");
    assert!(wt.exists(), "a run nobody superseded keeps its worktree");
    let old = RunState::load_under(&old.id, &magi::run::home()).unwrap();
    assert!(!old.released());
}
}

common::e2e! {
async fn a_running_run_is_refused_and_left_alone() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (mut old, wt) = old_run(&fx);
    // This very process is the "driver", identity and all.
    let pid = std::process::id();
    old.driver_pid = Some(pid);
    old.driver_started_at = magi::proc::process_started_at(pid);
    old.save().unwrap();

    let err = Runner::review_taking_over(
        &fx.repo,
        BRANCH,
        fx.config.clone(),
        Some(takeover(&[&old])),
    )
    .await
    .err()
    .expect("a running run must be refused");

    assert!(format!("{err:#}").contains("right now"), "{err:#}");
    assert!(wt.exists());
    let old = RunState::load_under(&old.id, &magi::run::home()).unwrap();
    assert!(!old.released());
}
}

common::e2e! {
async fn a_blocked_run_whose_driver_stopped_is_released_though_its_pid_lives() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (mut old, wt) = old_run(&fx);
    // The daemon that drove the run is still up (this very process), but the
    // run's walk ended: it is blocked, not running.
    let pid = std::process::id();
    old.status = RunStatus::Blocked;
    old.driver_pid = Some(pid);
    old.driver_started_at = magi::proc::process_started_at(pid);
    old.driver_exited = true;
    old.save().unwrap();
    let tip = run_git(&fx.repo, &["rev-parse", BRANCH]);

    let runner = Runner::review_taking_over(
        &fx.repo,
        BRANCH,
        fx.config.clone(),
        Some(takeover(&[&old])),
    )
    .await
    .expect("a stopped driver does not hold the branch");

    assert!(!wt.exists());
    assert_eq!(run_git(&fx.repo, &["rev-parse", BRANCH]), tip, "the branch survives");
    let old = RunState::load_under(&old.id, &magi::run::home()).unwrap();
    assert_eq!(old.released_to.as_deref(), Some(runner.state.id.as_str()));
    assert!(old.events.iter().any(|e| e.node == "release"));
    assert!(Runner::resume(&old.id).is_err());
}
}
