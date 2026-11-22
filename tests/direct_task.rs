//! `magi run` / `magi review` always file a task. With no loop alive the task
//! is claimed and executed in this process through the daemon's own attempt;
//! with a live loop it is filed urgent and left to that loop.

mod common;

use common::{Judges, fixture, home_lock};
use magi::config::{BuildCache, MergeMode};
use magi::daemon::{self, Opts, Status};
use magi::direct::{self, Filed, Filing};
use magi::queue::{Queue, RunOverrides, Source, Task, TaskStatus};
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
        sources: Default::default(),
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

#[test]
fn standalone_attempt_future_leaves_room_on_the_default_thread_stack() {
    let opts = Opts::default();
    let queue = Queue::at(std::path::PathBuf::from("unused-queue"));
    let mut task = magi::queue::Task::new(
        "review".into(),
        "review".into(),
        std::path::PathBuf::from("unused-repo"),
        Source::Human,
    );
    // Construction does no I/O. Bound the frame rather than increasing the
    // Windows test thread's stack to accommodate duplicated attempt futures.
    let future = daemon::run_claimed(&opts, &queue, &mut task);
    let bytes = std::mem::size_of_val(&future);
    assert!(
        bytes < 64 * 1024,
        "standalone attempt future occupies {bytes} bytes"
    );
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

common::e2e! {
async fn cli_persists_build_cache_choices_and_reports_the_effective_dry_run_mode() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    run_git(&fx.repo, &["remote", "set-head", "origin", "main"]);
    let home = magi::run::home();
    let queue = Queue::open();
    let cli = || {
        let mut command = common::magi_cli(&home);
        command.current_dir(&fx.repo);
        command
    };

    let added = cli()
        .args([
            "task", "add", "--force", "--json", "--build-cache", "isolated",
            "Create the example",
        ])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let task: Task = serde_json::from_slice(&added.stdout).unwrap();
    let stored = queue.get(&task.id).unwrap();
    assert_eq!(
        stored.overrides.unwrap().build_cache,
        Some(BuildCache::Isolated)
    );

    let show = |json: bool| {
        let mut command = cli();
        command.args(["task", "show", &task.id]);
        if json { command.arg("--json"); }
        let out = command.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    };
    let effective = || {
        let view: serde_json::Value = serde_json::from_slice(&show(true)).unwrap();
        view["effective_build_cache"].clone()
    };
    assert_eq!(effective(), serde_json::json!({"mode": "isolated", "source": "task-override"}));
    let change = |mode| cli().args(["task", "build-cache", &task.id, mode]).output().unwrap();
    let claim = queue.claim(&task.id).unwrap();
    let refused = change("shared");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("claimed"));
    drop(claim);
    let changed = change("shared");
    assert!(changed.status.success(), "{}", String::from_utf8_lossy(&changed.stderr));
    let mut stored = queue.get(&task.id).unwrap();
    assert_eq!(stored.overrides.as_ref().unwrap().build_cache, Some(BuildCache::Shared));
    stored.set_urgent(true).unwrap();
    stored.hold_manual(Some("waiting for operator".into()));
    queue.put(&mut stored).unwrap();
    assert_eq!(effective(), serde_json::json!({"mode": "shared", "source": "task-override"}));
    let cleared = change("default");
    assert!(cleared.status.success(), "{}", String::from_utf8_lossy(&cleared.stderr));
    let mut stored = queue.get(&task.id).unwrap();
    assert_eq!(stored.overrides.as_ref().and_then(|o| o.build_cache), None);
    assert_eq!(stored.status, TaskStatus::Held);
    // Still urgent, but this fixture configures no shared build cache, so
    // there is nothing to isolate from and the configured mode stands. The
    // urgent -> isolated path is covered by `isolated_build_cache` and by
    // `urgent_build_cache_precedes_config_but_not_an_explicit_choice`.
    assert_eq!(effective(), serde_json::json!({"mode": "shared", "source": "config"}));
    assert!(String::from_utf8_lossy(&show(false)).contains("shared (config)"));
    stored.set_urgent(false).unwrap();
    queue.put(&mut stored).unwrap();
    assert_eq!(effective(), serde_json::json!({"mode": "shared", "source": "config"}));
    stored.succeed();
    queue.put(&mut stored).unwrap();
    let refused = change("isolated");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("only queued or held tasks"));

    let mut cfg = fx.config.clone();
    cfg.verify.build_cache = BuildCache::Isolated;
    let config = fx.tmp.path().join("isolated.toml");
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    let planned = cli()
        .args(["run", "--force", "--dry-run", "--build-cache", "shared", "--config"])
        .arg(&config)
        .arg("Prepare the sample patch")
        .output()
        .unwrap();
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    assert!(String::from_utf8_lossy(&planned.stdout).contains("build cache: shared"));

    let planned_task = queue
        .list()
        .into_iter()
        .find(|task| task.instruction == "Prepare the sample patch")
        .unwrap();
    assert_eq!(
        planned_task.overrides.unwrap().build_cache,
        Some(BuildCache::Shared)
    );
    assert_eq!(planned_task.attempts, 0);
    let run = &planned_task.runs[0];
    let state = magi::run::RunState::load(run).unwrap();
    assert_eq!(state.config.verify.build_cache, BuildCache::Shared);

    let resumed = cli()
        .args(["run", "--resume", run, "--dry-run"])
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("build cache: shared"));

    run_git(&fx.repo, &["checkout", "-q", "-b", "feat/build-cache"]);
    std::fs::write(fx.repo.join("note.txt"), "review this\n").unwrap();
    run_git(&fx.repo, &["add", "note.txt"]);
    run_git(&fx.repo, &["commit", "-q", "-m", "add note"]);
    run_git(&fx.repo, &["checkout", "-q", "main"]);
    let reviewed = cli()
        .args([
            "review", "feat/build-cache", "--force", "--build-cache", "shared", "--config",
        ])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        reviewed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&reviewed.stdout),
        String::from_utf8_lossy(&reviewed.stderr)
    );
    let review = queue
        .list()
        .into_iter()
        .find(|task| task.review_of.as_deref() == Some("feat/build-cache"))
        .unwrap();
    assert_eq!(
        review.overrides.unwrap().build_cache,
        Some(BuildCache::Shared)
    );
    let state = magi::run::RunState::load(&review.runs[0]).unwrap();
    assert_eq!(state.config.verify.build_cache, BuildCache::Shared);
}
}
