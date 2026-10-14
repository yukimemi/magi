//! `[worktree] setup` runs in every worktree magi creates, its products stay
//! out of candidate commits, and a failing step blocks the run loudly.
mod common;

use common::{Judges, fixture};
use magi::config::SetupStep;
use magi::graph::Runner;
use magi::run::{PrepFailure, RunStatus};
use magi::worktree_setup::{SetupFailureEvidence, SetupFailureTarget};

fn copy(spec: &str) -> SetupStep {
    SetupStep {
        copy: Some(spec.to_owned()),
        ..Default::default()
    }
}

fn run(cmd: &str) -> SetupStep {
    SetupStep {
        run: Some(cmd.to_owned()),
        ..Default::default()
    }
}

common::e2e! {
async fn setup_reaches_every_seat_and_stays_out_of_the_candidate() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, true);
    fx.config.disk.min_free_bytes = 0;
    std::fs::write(fx.repo.join("secret.src"), "token\n").unwrap();
    let marker = fx.tmp.path().join("setup.log");
    fx.config.worktree.setup = vec![
        copy("secret.src -> .env"),
        run(&format!("cat .env >> '{}'", marker.display())),
    ];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;
    assert_ne!(state.status, RunStatus::Blocked);

    // 3 candidates + 3 judges, and at least one review round of 2 reviewers.
    // `require_fix` forces a second round, so reviewers are set up again.
    let runs = std::fs::read_to_string(&marker).unwrap().lines().count();
    assert!(runs >= 3 + 3 + 2 + 2, "setup ran {runs} times");

    let winner = state.winner().expect("winner");
    let tree = magi::git::git(&state.repo, &["ls-tree", "-r", "--name-only", winner.branch.as_str()])
        .await
        .unwrap();
    assert!(tree.lines().any(|l| l == "note.txt"), "{tree}");
    assert!(!tree.lines().any(|l| l == ".env"), "setup product was committed: {tree}");
}
}

common::e2e! {
async fn a_failing_step_blocks_the_run_and_names_the_step() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.worktree.setup = vec![run("echo no-bootstrap >&2; exit 7")];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    let err = runner.execute().await.expect_err("setup must fail the run");
    assert!(format!("{err:#}").contains("no-bootstrap"), "{err:#}");
    assert_eq!(runner.state.status, RunStatus::Blocked);
    assert_eq!(runner.state.prep_failure_evidence, Some(PrepFailure::WorktreeSetup {
        target: Some(SetupFailureTarget::Run { step: 1, command: "echo no-bootstrap >&2; exit 7".into() }),
        evidence: Some(SetupFailureEvidence::Exit { code: Some(7), output: "no-bootstrap".into() }),
    }));
    assert!(
        runner.state.events.iter().any(|e| e.message.contains("worktree setup failed")
            && e.message.contains("step 1/1")),
        "{:?}",
        runner.state.events
    );
    // Nothing of the failed prep is left to be resumed into.
    assert!(runner.state.candidates.is_empty());
}
}

common::e2e! {
async fn an_empty_setup_changes_nothing() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    assert!(fx.config.worktree.setup.is_empty());
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    assert!(runner.state.events.iter().all(|e| !e.message.contains("worktree setup")));
}
}

common::e2e! {
async fn setup_then_cherry_pick_failures_record_distinct_durable_targets() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    std::fs::write(fx.repo.join("conflict.txt"), "base\n").unwrap();
    magi::git::git(&fx.repo, &["add", "conflict.txt"]).await.unwrap();
    magi::git::git(&fx.repo, &["commit", "-m", "add shared base"]).await.unwrap();
    magi::git::git(&fx.repo, &["switch", "-c", "referenced-change"]).await.unwrap();
    std::fs::write(fx.repo.join("conflict.txt"), "referenced change\n").unwrap();
    magi::git::git(&fx.repo, &["commit", "-am", "change referenced content"]).await.unwrap();
    let pick = magi::git::git(&fx.repo, &["rev-parse", "HEAD"]).await.unwrap().trim().to_owned();
    magi::git::git(&fx.repo, &["switch", "main"]).await.unwrap();
    std::fs::write(fx.repo.join("conflict.txt"), "base branch change\n").unwrap();
    magi::git::git(&fx.repo, &["commit", "-am", "change base content"]).await.unwrap();
    magi::git::git(&fx.repo, &["push", "origin", "main"]).await.unwrap();
    magi::git::git(&fx.repo, &["fetch", "origin"]).await.unwrap();
    fx.config.worktree.setup = vec![copy("fixture-gate.txt -> .setup-gate")];
    let instruction = format!("Apply the referenced commit {pick}");
    let mut first = Runner::start(&fx.repo, instruction.clone(), fx.config.clone(), magi::run::Origin::operator())
        .await.unwrap();
    first.execute().await.expect_err("missing setup source");
    assert!(matches!(&first.state.prep_failure_evidence, Some(PrepFailure::WorktreeSetup {
        target: Some(SetupFailureTarget::Copy { step: 1, spec }), evidence: Some(_),
    }) if spec == "fixture-gate.txt -> .setup-gate"));
    std::fs::write(fx.repo.join("fixture-gate.txt"), "ready\n").unwrap();
    let mut second = Runner::start(&fx.repo, instruction, fx.config.clone(), magi::run::Origin::operator())
        .await.unwrap();
    second.execute().await.expect_err("conflicting referenced commit");
    assert_eq!(first.state.base_commit, second.state.base_commit);
    assert_eq!(first.state.instruction, second.state.instruction);
    assert_eq!(second.state.status, RunStatus::Blocked);
    let persisted = magi::run::RunState::load(&second.state.id).unwrap();
    assert_eq!(persisted.prep_failure_evidence, Some(PrepFailure::CherryPick {
        commit: pick, start: second.state.base_commit.clone(), applied: Vec::new(),
    }));
    assert_ne!(first.state.prep_failure_evidence, persisted.prep_failure_evidence);
}
}
