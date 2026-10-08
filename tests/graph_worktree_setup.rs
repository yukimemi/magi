//! `[worktree] setup` runs in every worktree magi creates, its products stay
//! out of candidate commits, and a failing step blocks the run loudly.
mod common;

use common::{Judges, fixture};
use magi::config::SetupStep;
use magi::graph::Runner;
use magi::run::RunStatus;

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
