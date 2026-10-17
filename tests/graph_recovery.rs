//! A retried task whose earlier run stopped on a failed verification carries
//! that run's evidence into the run that takes its branch over (see
//! `magi::recovery`). Evidence is context for prompts only: verification
//! always runs again on the commit that is going to land.
mod common;

use std::path::{Path, PathBuf};

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::handover::Takeover;
use magi::recovery::{Mode, Repair};
use magi::run::{Candidate, CommandOutcome, ReviewRound, RunState, RunStatus};

const BRANCH: &str = "magi/0282/B";
const CRITERIA: &str = "TASK CRITERIA: every spawn must be quiet";
const FAILURE: &str = "every_spawn_in_the_crate_is_quiet_or_documented_as_exempt FAILED";

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

fn red(tail: &str) -> Vec<CommandOutcome> {
    vec![CommandOutcome {
        command: "cargo test --locked --all-targets".to_owned(),
        code: Some(101),
        output_tail: tail.to_owned(),
        duration_ms: 1,
        resource_blocked: false,
    }]
}

/// An earlier run that reviewed `BRANCH` at its tip with nothing blocking and
/// then failed `e2e`. `shape` edits the round before it is saved.
fn old_run(fx: &common::Fixture, shape: impl FnOnce(&mut ReviewRound, &mut RunState)) -> RunState {
    let base = run_git(&fx.repo, &["rev-parse", "main"]);
    let mut state = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        base,
        "task".to_owned(),
        fx.config.clone(),
    );
    let wt: PathBuf = state.worktree_root().join("cand-B");
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
    let tip = run_git(&wt, &["rev-parse", "HEAD"]);
    state.status = RunStatus::Blocked;
    state.driver_exited = true;
    state.candidates.push(Candidate {
        index: 0,
        label: 'B',
        agent: "(existing branch)".to_owned(),
        branch: BRANCH.to_owned(),
        worktree: wt,
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
    state.tally = Some(magi::run::Tally {
        first_choice: Default::default(),
        borda: Default::default(),
        winner: 'B',
        rankings: 0,
        unanimous_initial: false,
        deliberated: false,
        changed_votes: 0,
        unanimous_final: false,
        tie_break: None,
        judges: 0,
        present: 0,
        quorum: 0,
        met_quorum: true,
        uncontested: Some("review-only run: nothing competed".to_owned()),
    });
    let mut round = ReviewRound {
        round: 3,
        head: tip.clone(),
        verified_head: Some(tip),
        verified_at: None,
        reviews: Vec::new(),
        e2e: red(FAILURE),
        verify_retried: false,
        e2e_deferred: false,
        e2e_defer_reason: None,
        fix: None,
        blocking: 0,
        answered: 2,
        expected: 2,
        clean: false,
        progressed: false,
        vote_split: false,
        reconsideration: Vec::new(),
        verdict: Some(magi::verdict::ReviewVote::Approve),
    };
    shape(&mut round, &mut state);
    state.reviews.push(round);
    state.save().expect("save the old run");
    state
}

async fn take_over(fx: &common::Fixture, old: &RunState) -> Runner {
    Runner::review_taking_over(
        &fx.repo,
        BRANCH,
        fx.config.clone(),
        Some(Takeover {
            earlier: vec![old.id.clone()],
            home: magi::run::home(),
            choice: None,
            task_instruction: Some(CRITERIA.to_owned()),
        }),
        magi::run::Origin::operator(),
    )
    .await
    .expect("the review takes the branch over")
}

fn artifact(state: &RunState, name: &str) -> String {
    std::fs::read_to_string(state.dir().join("artifacts").join(name)).unwrap_or_default()
}

common::e2e! {
async fn an_unchanged_commit_gets_a_repair_then_a_scoped_review_and_fresh_verification() {
    let home = common::home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let old = old_run(&fx, |_, _| {});
    let mut runner = take_over(&fx, &old).await;

    let rec = runner.state.recovery.clone().expect("a recovery record");
    assert_eq!(rec.mode, Mode::VerifyRepair, "{:?}", rec.reasons);
    assert!(rec.criteria_inherited);
    assert!(runner.state.instruction.starts_with(CRITERIA));
    assert!(
        runner.state.instruction.contains("old run work"),
        "the commit subjects ride along"
    );

    runner.execute().await.expect("execute");
    let state = &runner.state;

    // Evidence reached the fixer, ahead of the task, with the criteria.
    let fix = artifact(state, "fix-recovery.prompt.md");
    assert!(fix.contains(FAILURE), "{fix}");
    assert!(fix.contains("Your patch was reviewed"));
    let evidence_at = fix.find("# Failed verification").expect("evidence section");
    assert!(evidence_at < fix.find("# The task").unwrap());
    assert!(fix.contains(CRITERIA));

    // The review saw the earlier context and the scoped diff.
    let review = artifact(state, "review-1-1.prompt.md");
    assert!(review.contains("# Earlier attempt at this task"), "{review}");
    assert!(review.contains("# Changes since the earlier reviewed commit"));
    assert!(review.contains("fixed.txt"), "the repair is in the scoped diff");
    assert!(review.contains(FAILURE));

    // The repair is recorded once and verification ran again on the commit
    // that is about to land; nothing was copied from the earlier run.
    let rec = state.recovery.as_ref().unwrap();
    assert!(matches!(rec.repair, Repair::Done { committed: true, failed: None, .. }), "{:?}", rec.repair);
    let first = &state.reviews[0];
    let head = run_git(&state.winner().unwrap().worktree, &["rev-parse", "HEAD"]);
    assert_eq!(first.verified_head.as_deref(), Some(head.as_str()));
    assert_eq!(first.head, head);
    assert!(first.e2e.iter().all(|o| o.ok()), "fresh e2e: {:?}", first.e2e);
    assert!(first.clean, "{}", magi::report::run(state));
    assert!(first.fix.is_none(), "the repair is not a review-round fix");
    assert_eq!(
        state.events.iter().filter(|e| e.node == "recovery" && e.message.contains("committed")).count(),
        1
    );
}
}

common::e2e! {
async fn a_commit_added_since_the_review_widens_it_and_repairs_nothing() {
    let home = common::home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let old = old_run(&fx, |_, _| {});
    let wt = &old.candidates[0].worktree;
    std::fs::write(wt.join("later.txt"), "added after the review\n").unwrap();
    run_git(wt, &["add", "-A"]);
    run_git(wt, &["commit", "-q", "-m", "later"]);

    let mut runner = take_over(&fx, &old).await;
    let rec = runner.state.recovery.clone().unwrap();
    assert_eq!(rec.mode, Mode::FullReview);
    assert!(rec.reasons.iter().any(|r| r.contains("never reviewed")), "{:?}", rec.reasons);
    runner.execute().await.expect("execute");
    let state = &runner.state;
    assert!(artifact(state, "fix-recovery.prompt.md").is_empty());
    let review = artifact(state, "review-1-1.prompt.md");
    assert!(review.contains("Review the whole patch as if for the first time"), "{review}");
    assert!(!review.contains("# Changes since the earlier reviewed commit"));
    assert!(state.events.iter().any(|e| e.node == "recovery" && e.message.contains("full review")));
}
}

common::e2e! {
async fn a_rebased_branch_or_a_moved_base_is_a_full_review_with_reasons() {
    let home = common::home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    // The earlier run reviewed a commit this branch no longer contains, on a
    // base that is no longer the base.
    let old = old_run(&fx, |round, state| {
        round.head = "0123456789abcdef0123456789abcdef01234567".to_owned();
        round.verified_head = Some(round.head.clone());
        state.base_commit = "fedcba9876543210fedcba9876543210fedcba98".to_owned();
    });
    let runner = take_over(&fx, &old).await;
    let rec = runner.state.recovery.as_ref().unwrap();
    assert_eq!(rec.mode, Mode::FullReview);
    assert!(rec.reasons.iter().any(|r| r.contains("rebased or rewritten")), "{:?}", rec.reasons);
    assert!(rec.reasons.iter().any(|r| r.contains("base moved")), "{:?}", rec.reasons);
    assert!(rec.evidence.is_some(), "the evidence still travels as context");
}
}

common::e2e! {
async fn missing_evidence_or_an_unreadable_record_falls_back_with_the_reason() {
    let home = common::home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let old = old_run(&fx, |round, _| round.e2e.clear());
    let runner = take_over(&fx, &old).await;
    let rec = runner.state.recovery.as_ref().unwrap();
    assert_eq!(rec.mode, Mode::FullReview);
    assert!(rec.reasons[0].contains("no failed verification"), "{:?}", rec.reasons);
    // Without a readable review the task's statement is not substituted.
    assert!(!rec.criteria_inherited || rec.reviewed_head.is_some());
}
}

common::e2e! {
async fn the_repair_is_bounded_to_one_and_a_resume_never_repeats_it() {
    let home = common::home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let old = old_run(&fx, |_, _| {});
    let mut runner = take_over(&fx, &old).await;
    // A start recorded but never settled, as after a crash mid-repair.
    let before = run_git(&runner.state.winner().unwrap().worktree, &["rev-parse", "HEAD"]);
    runner.state.recovery.as_mut().unwrap().repair = Repair::Started { before: before.clone() };
    runner.state.save().unwrap();
    let id = runner.state.id.clone();
    drop(runner);

    let mut resumed = Runner::resume(&id).expect("resume");
    resumed.execute().await.expect("execute");
    let state = &resumed.state;
    assert!(artifact(state, "fix-recovery.prompt.md").is_empty(), "no second repair");
    match &state.recovery.as_ref().unwrap().repair {
        Repair::Done { before: b, committed: false, failed: Some(why), .. } => {
            assert_eq!(b, &before);
            assert!(why.contains("interrupted"));
        }
        other => panic!("{other:?}"),
    }
    // Round 1 still ran its own verification on the exact head.
    assert!(state.reviews[0].verified_head.is_some());
}
}

common::e2e! {
async fn evidence_is_handed_over_a_bounded_number_of_times() {
    let home = common::home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let old = old_run(&fx, |_, state| {
        let mut rec = magi::recovery::assess("r0", None, "b", magi::recovery::Relation::Same);
        rec.generation = magi::recovery::MAX_GENERATION;
        state.recovery = Some(rec);
    });
    let runner = take_over(&fx, &old).await;
    let rec = runner.state.recovery.as_ref().unwrap();
    assert_eq!(rec.mode, Mode::FullReview);
    assert!(rec.reasons.iter().any(|r| r.contains("handed over")), "{:?}", rec.reasons);
}
}
