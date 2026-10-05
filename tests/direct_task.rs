//! `magi run` / `magi review` always file a task. With no loop alive the task
//! is claimed and executed in this process through the daemon's own attempt;
//! with a live loop it is filed urgent and left to that loop.

mod common;

use common::{Judges, fixture, home_lock};
use magi::config::MergeMode;
use magi::daemon::{self, Opts, Status};
use magi::direct::{self, Filed, Filing};
use magi::queue::{Queue, RunOverrides, Source, TaskStatus};
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

fn review_filing(fx: &common::Fixture, branch: &str, config: std::path::PathBuf) -> Filing {
    Filing {
        title: format!("review {branch}"),
        instruction: format!("Review the branch `{branch}`."),
        repo: fx.repo.clone(),
        source: Source::Human,
        solo: false,
        overrides: RunOverrides {
            merge: Some("none".to_owned()),
            config: Some(config),
            ..RunOverrides::default()
        },
        review_of: Some(branch.to_owned()),
    }
}

fn write_config(fx: &common::Fixture) -> std::path::PathBuf {
    let mut config = fx.config.clone();
    config.disk.min_free_bytes = 0;
    config.disk.auto_fold = false;
    config.disk.cache_limit_bytes = 0;
    config.merge.mode = MergeMode::Local;
    let path = fx.tmp.path().join("magi.toml");
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    path
}

common::e2e! {
async fn a_review_with_no_loop_runs_against_its_own_claimed_task() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    run_git(&fx.repo, &["checkout", "-q", "-b", "feat/by-hand"]);
    std::fs::write(fx.repo.join("note.txt"), "by hand\n").unwrap();
    run_git(&fx.repo, &["add", "-A"]);
    run_git(&fx.repo, &["commit", "-q", "-m", "add note"]);
    run_git(&fx.repo, &["checkout", "-q", "main"]);

    let config = write_config(&fx);
    let queue = Queue::open();
    let filed = direct::file(
        &queue,
        &magi::run::home(),
        jiff::Timestamp::now(),
        std::process::id(),
        review_filing(&fx, "feat/by-hand", config.clone()),
        false,
    )
    .expect("file");
    let Filed::Standalone { mut task, claim } = filed else {
        panic!("no loop is alive");
    };
    assert!(
        queue.claim(&task.id).is_err(),
        "a second process cannot claim a standalone run's task"
    );
    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config),
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    daemon::run_claimed(&opts, &queue, &mut task).await;
    drop(claim);

    let stored = queue.get(&task.id).unwrap();
    assert_eq!(stored.status, TaskStatus::Done, "{:?}", stored.last_error);
    assert_eq!(stored.attempts, 1, "counted as the daemon counts");
    assert_eq!(stored.runs.len(), 1, "the run is linked to its task");
    let state = magi::run::RunState::load(&stored.runs[0]).unwrap();
    assert_eq!(state.status, RunStatus::Ready);
    assert_eq!(
        state.config.merge.mode,
        MergeMode::None,
        "--merge none rode on the task and beat the config's mode"
    );
    assert!(state.judgements.is_empty(), "a review does not compete");
    assert_eq!(
        state.origin.as_ref().and_then(|o| o.task.clone()),
        Some(stored.id.clone())
    );
}
}

common::e2e! {
async fn a_review_of_a_missing_branch_fails_instead_of_competing() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let config = write_config(&fx);
    let queue = Queue::open();
    let filed = direct::file(
        &queue,
        &magi::run::home(),
        jiff::Timestamp::now(),
        std::process::id(),
        review_filing(&fx, "feat/nope", config.clone()),
        false,
    )
    .expect("file");
    let Filed::Standalone { mut task, claim } = filed else {
        panic!("no loop is alive");
    };
    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config),
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    daemon::run_claimed(&opts, &queue, &mut task).await;
    drop(claim);
    let stored = queue.get(&task.id).unwrap();
    assert!(stored.runs.is_empty(), "no competition was bought");
    assert_eq!(
        stored.status,
        TaskStatus::Held,
        "held, so a later loop does not retry it behind the operator's back"
    );
}
}

common::e2e! {
async fn a_live_loop_gets_an_urgent_task_and_nothing_runs_here() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let mut status = Status::new();
    status.pid = std::process::id().wrapping_add(1);
    daemon::write_status_to(&daemon::status_path(), &status).unwrap();

    let queue = Queue::open();
    let filed = direct::file(
        &queue,
        &magi::run::home(),
        jiff::Timestamp::now(),
        std::process::id(),
        review_filing(&fx, "feat/by-hand", fx.tmp.path().join("magi.toml")),
        false,
    )
    .expect("file");
    let Filed::Daemon { task, .. } = filed else {
        panic!("a loop is alive");
    };
    let stored = queue.get(&task.id).unwrap();
    assert!(stored.urgent);
    assert_eq!(stored.status, TaskStatus::Queued);
    assert!(stored.runs.is_empty());
    assert!(magi::run::list_ids().is_empty(), "no run was minted here");
    assert!(queue.claim(&task.id).is_ok(), "the loop's to claim");
}
}
