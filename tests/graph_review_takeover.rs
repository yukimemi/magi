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

/// An earlier stopped run whose candidate worktree holds `BRANCH` with one
/// commit on it. Returns the saved run and the worktree path.
fn old_run(fx: &common::Fixture) -> (RunState, PathBuf) {
    old_run_at(fx, false)
}

/// [`old_run`], with the worktree outside magi's worktree root when `outside`.
fn old_run_at(fx: &common::Fixture, outside: bool) -> (RunState, PathBuf) {
    let base = run_git(&fx.repo, &["rev-parse", "main"]);
    let mut state = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        base,
        "task".to_owned(),
        fx.config.clone(),
    );
    let wt = if outside {
        fx.tmp.path().join("old-wt").join("cand-A")
    } else {
        state.worktree_root().join("cand-A")
    };
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

    state.status = RunStatus::Gating;
    state.driver_exited = true;
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
        choice: None,
        task_instruction: None,
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
        Some(takeover(&[&old])), magi::run::Origin::operator(),
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
        Some(takeover(&[&old])), magi::run::Origin::operator(),
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
    let (old, wt) = old_run_at(&fx, true);

    // Not among the task's earlier attempts, e.g. another task's run.
    let result =
        Runner::review_taking_over(&fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[])), magi::run::Origin::operator()).await;

    let err = result.err().expect("git still refuses to share the branch");
    let text = format!("{err:#}");
    assert!(
        err.downcast_ref::<magi::handover::Refused>().is_some() && text.contains("by hand"),
        "the refusal says the path is not magi's: {text}"
    );
    assert!(wt.exists(), "a worktree magi did not make keeps its worktree");
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
    old.driver_exited = false;
    old.save().unwrap();

    let err = Runner::review_taking_over(
        &fx.repo,
        BRANCH,
        fx.config.clone(),
        Some(takeover(&[&old])), magi::run::Origin::operator(),
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
        Some(takeover(&[&old])), magi::run::Origin::operator(),
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

/// A run of *another* task, finished (blocked, no driver), whose candidate
/// worktree sits where magi lays them: `<worktree root>/<short>/cand-A`.
fn other_task_run(fx: &common::Fixture) -> (RunState, PathBuf) {
    let base = run_git(&fx.repo, &["rev-parse", "main"]);
    let mut state = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        base,
        "another task".to_owned(),
        fx.config.clone(),
    );
    let wt = state.worktree_root().join("cand-A");
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
    std::fs::write(wt.join("note.txt"), "by the other run\n").unwrap();
    run_git(&wt, &["add", "-A"]);
    run_git(&wt, &["commit", "-q", "-m", "other run work"]);
    state.status = RunStatus::Blocked;
    state.driver_exited = true;
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
    state.save().expect("save the other run");
    (state, wt)
}

common::e2e! {
async fn a_clean_dead_run_of_another_task_is_released_and_audited() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (old, wt) = other_task_run(&fx);
    let tip = run_git(&fx.repo, &["rev-parse", BRANCH]);

    let runner = Runner::review_taking_over(
        &fx.repo,
        BRANCH,
        fx.config.clone(),
        Some(takeover(&[])), magi::run::Origin::operator(),
    )
    .await
    .expect("a clean worktree of a dead run is released");

    assert!(!wt.exists());
    assert_eq!(run_git(&fx.repo, &["rev-parse", BRANCH]), tip, "the branch survives");
    let old = RunState::load_under(&old.id, &magi::run::home()).unwrap();
    assert_eq!(old.released_to.as_deref(), Some(runner.state.id.as_str()));
    let note = old.events.iter().find(|e| e.node == "release").expect("audited");
    assert!(note.message.contains("clean") && note.message.contains("blocked"), "{}", note.message);
    assert!(runner.state.events.iter().any(|e| e.node == "release" && e.message.contains("clean")));
}
}

common::e2e! {
async fn a_dirty_worktree_of_another_task_names_the_files() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (old, wt) = other_task_run(&fx);
    std::fs::write(wt.join("scratch.txt"), "unsaved\n").unwrap();

    let err = Runner::review_taking_over(&fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[])), magi::run::Origin::operator())
        .await
        .err()
        .expect("dirty must be refused");
    assert!(err.downcast_ref::<magi::handover::Refused>().is_some());
    let text = format!("{err:#}");
    assert!(text.contains("scratch.txt") && text.contains("uncommitted"), "{text}");
    assert!(wt.join("scratch.txt").exists());
    assert!(!RunState::load_under(&old.id, &magi::run::home()).unwrap().released());
}
}

common::e2e! {
async fn a_live_run_of_another_task_is_left_alone_and_named() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (mut old, wt) = other_task_run(&fx);
    let pid = std::process::id();
    old.status = RunStatus::Reviewing;
    old.driver_pid = Some(pid);
    old.driver_started_at = magi::proc::process_started_at(pid);
    old.driver_exited = false;
    old.save().unwrap();

    let err = Runner::review_taking_over(&fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[])), magi::run::Origin::operator())
        .await
        .err()
        .expect("a live run must be refused");
    let text = format!("{err:#}");
    assert!(
        text.contains("right now") && text.contains(magi::run::short_of(&old.id)),
        "{text}"
    );
    assert!(wt.exists());
}
}

common::e2e! {
async fn an_earlier_attempts_worktree_outside_the_worktree_root_is_left_alone() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (old, wt) = old_run_at(&fx, true);

    let err = Runner::review_taking_over(&fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[&old])), magi::run::Origin::operator())
        .await
        .err()
        .expect("a path magi did not lay must be refused");
    assert!(err.downcast_ref::<magi::handover::Refused>().is_some());
    assert!(format!("{err:#}").contains("worktree root"), "{err:#}");
    assert!(wt.exists());
    assert!(!RunState::load_under(&old.id, &magi::run::home()).unwrap().released());
}
}

common::e2e! {
async fn an_unknown_legacy_driver_is_refused_without_treating_missing_pid_as_proof() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (mut old, wt) = old_run(&fx);
    old.driver_exited = false;
    old.save().unwrap();
    let error = Runner::review_taking_over(
        &fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[&old])), magi::run::Origin::operator(),
    ).await.err().expect("unknown ownership must not be released");
    assert!(format!("{error:#}").contains("could not be shown"));
    assert!(wt.exists());
    assert!(!RunState::load(&old.id).unwrap().released());
}
}

common::e2e! {
async fn a_legacy_unclaimed_run_can_execute_park_and_then_hand_over() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (mut old, wt) = old_run(&fx);
    old.driver_exited = false;
    old.driver_pid = None;
    old.driver_started_at = None;
    // Like a review-only run before its first graph claim, the candidate
    // checkout is already prepared. Resuming must not rebuild that checkout.
    old.prep_complete = Some(true);
    old.save().unwrap();
    assert_eq!(old.liveness(false), magi::run::Liveness::Unknown);
    let mut resumed = Runner::resume(&old.id).expect("legacy unclaimed runs can resume");
    let pause = magi::graph::Pause::new();
    pause.park();
    resumed.on_pause(pause);
    resumed.execute().await.unwrap();
    let stopped = RunState::load(&old.id).unwrap();
    assert!(stopped.parked && stopped.driver_exited);
    assert_eq!(stopped.liveness(false), magi::run::Liveness::Dead);
    let successor = Runner::review_taking_over(
        &fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[&stopped])), magi::run::Origin::operator(),
    ).await.expect("an actual execution exit establishes proof for handover");
    assert!(!wt.exists());
    assert_eq!(RunState::load(&old.id).unwrap().released_to.as_deref(), Some(successor.state.id.as_str()));
}
}

common::e2e! {
async fn a_failed_successor_can_restore_its_handover_under_the_owner_lock() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (old, wt) = old_run(&fx);
    let released = magi::handover::release(&fx.repo, BRANCH, "successor", &takeover(&[&old]))
        .await.unwrap().expect("the stopped worktree is released");
    assert!(!wt.exists());
    released.restore(&fx.repo, BRANCH).await;
    assert!(wt.exists());
    let restored = RunState::load(&old.id).unwrap();
    assert!(!restored.released());
    assert!(!restored.candidates[0].folded);
    assert!(restored.driver_exited);
    assert!(Runner::resume(&old.id).is_ok());
}
}

common::e2e! {
async fn checkpointed_predecessors_keep_their_worktree_and_remain_resumable() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (base, wt) = old_run(&fx);
    let tip = run_git(&fx.repo, &["rev-parse", BRANCH]);
    for checkpoint in ["publication", "blocked-publication", "approval", "landing", "armed", "decision", "repair", "question"] {
        let mut old = base.clone();
        match checkpoint {
            "publication" => old.pr_publication_pending = true,
            "blocked-publication" => {
                old.status = RunStatus::Blocked;
                old.pr_publication_pending = true;
            }
            "approval" => {
                old.status = RunStatus::Landing;
                old.land_approval = Some(magi::run::LandApproval { question: "approval".into(), head: tip.clone() });
            }
            "landing" => old.status = RunStatus::Landing,
            "armed" => old.land_armed_head = Some(tip.clone()),
            "decision" => old.merge_decision = Some(magi::merge_decision::Decision {
                question: "decision".into(),
                target: magi::merge_decision::Target { pr: "https://github.com/o/r/pull/1".into(), head: tip.clone(), language: "en".into() },
                choice: "hold".into(), closed: false, replacement_run: None, replacement_pr: None,
            }),
            "repair" => old.merge_repairs.push(magi::run::MergeRepair {
                question: "repair".into(), head: tip.clone(), pr: "https://github.com/o/r/pull/1".into(),
                reviews: Vec::new(), gate: Vec::new(), gate_head: None, gate_fixes: Vec::new(), pre_gate: Vec::new(),
                phase: magi::run::MergeRepairPhase::Requested, failure: None,
            }),
            "question" => {
                let mut question = magi::ask::Question::new(old.id.clone(), "review".into(), "reviewer".into(),
                    "Continue?".into(), String::new(), Vec::new());
                magi::ask::Questions::open().put(&mut question).unwrap();
            }
            _ => unreachable!(),
        }
        old.save().unwrap();
        let before = std::fs::read(old.dir().join("run.json")).unwrap();
        let error = Runner::review_taking_over(
            &fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[&old])), magi::run::Origin::operator(),
        ).await.err().expect("checkpointed work must stay with its original run");
        assert!(matches!(error.downcast_ref::<magi::handover::Refused>(), Some(magi::handover::Refused::Unsafe { .. })), "{checkpoint}: {error:#}");
        assert!(format!("{error:#}").contains("resume that run"), "{checkpoint}: {error:#}");
        assert!(wt.exists(), "{checkpoint}");
        assert_eq!(run_git(&fx.repo, &["rev-parse", BRANCH]), tip, "{checkpoint}");
        assert_eq!(std::fs::read(old.dir().join("run.json")).unwrap(), before, "{checkpoint}");
        let resumed = Runner::resume(&old.id).expect("refusal must preserve resumption");
        assert!(!resumed.state.released(), "{checkpoint}");
        assert!(!resumed.state.candidates[0].folded, "{checkpoint}");
    }
}
}

common::e2e! {
async fn historical_approval_on_a_completed_run_does_not_block_handover() {
    let _home = common::home_lock().await;
    let fx = fixture(_home, Judges::Unanimous, true);
    let (mut old, wt) = old_run(&fx);
    old.status = RunStatus::Ready;
    old.land_approval = Some(magi::run::LandApproval { question: "historical".into(), head: "head".into() });
    old.save().unwrap();
    let successor = Runner::review_taking_over(
        &fx.repo, BRANCH, fx.config.clone(), Some(takeover(&[&old])), magi::run::Origin::operator(),
    ).await.expect("completed-run evidence is historical");
    assert!(!wt.exists());
    assert_eq!(RunState::load(&old.id).unwrap().released_to.as_deref(), Some(successor.state.id.as_str()));
}
}
