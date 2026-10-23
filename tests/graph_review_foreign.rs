//! Foreign worktrees remain operator-owned throughout isolated reviews.
mod common;

use common::{Judges, fixture};
use magi::{
    git,
    graph::Runner,
    run::{Origin, RunState, RunStatus},
};
use std::path::{Path, PathBuf};

const BRANCH: &str = "feat/by-hand";

async fn foreign(fx: &common::Fixture) -> PathBuf {
    let path = fx.tmp.path().join("foreign tree's checkout");
    git::worktree_add_branch(&fx.repo, &path, BRANCH, "main")
        .await
        .unwrap();
    std::fs::write(path.join("note.txt"), "hand-written\n").unwrap();
    commit(&path, "add hand-written note").await;
    path
}

async fn commit(path: &Path, subject: &str) {
    git::git(path, &["add", "-A"]).await.unwrap();
    git::git(path, &["commit", "-m", subject]).await.unwrap();
}

common::e2e! {
async fn foreign_clean_checkout_receives_review_fixes_without_being_removed() {
    let guard = common::home_lock().await;
    let fx = fixture(guard, Judges::Unanimous, true);
    let path = foreign(&fx).await;
    let mut runner = Runner::review(&fx.repo, BRANCH, fx.config.clone(), Origin::operator()).await.unwrap();
    let winner = runner.state.winner().unwrap();
    assert_ne!(winner.branch, BRANCH);
    assert_ne!(winner.worktree, path);
    assert_eq!(runner.state.pr_branch(), Some(BRANCH));
    runner.execute().await.unwrap();
    assert_eq!(runner.state.status, RunStatus::Ready);
    assert!(path.join("fixed.txt").exists());
    assert!(git::is_clean(&path).await.unwrap());
    assert_eq!(git::current_branch(&path).await.unwrap().as_deref(), Some(BRANCH));
    assert_eq!(git::rev_parse(&path, "HEAD").await.unwrap(),
        git::rev_parse(&fx.repo, &runner.state.winner().unwrap().branch).await.unwrap());
    let saved = RunState::load(&runner.state.id).unwrap();
    assert_eq!(saved.foreign_checkout.unwrap().synced_head, git::rev_parse(&path, "HEAD").await.unwrap());
    magi::graph::fold_run(&mut runner.state, true, &magi::run::home()).await.unwrap();
    assert!(path.join("fixed.txt").exists(), "fold retains the foreign checkout");
    assert!(git::branch_exists(&fx.repo, BRANCH).await.unwrap());
}
}

common::e2e! {
async fn foreign_dirty_checkout_is_refused_before_filing_and_untouched() {
    let guard = common::home_lock().await;
    let fx = fixture(guard, Judges::Unanimous, true);
    let path = foreign(&fx).await;
    // Even a user's status configuration cannot hide untracked work.
    git::git(&path, &["config", "status.showUntrackedFiles", "no"]).await.unwrap();
    std::fs::write(path.join("scratch.txt"), "keep this\n").unwrap();
    let before = git::git(&path, &["status", "--porcelain", "--untracked-files=all"]).await.unwrap();
    let config = fx.tmp.path().join("review.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    let cli_home = fx.tmp.path().join("cli-home");
    std::fs::create_dir_all(&cli_home).unwrap();
    let out = common::magi_cli(&cli_home).current_dir(&path)
        .args(["review", BRANCH, "--merge", "none", "--config"])
        .arg(&config).output().unwrap();
    assert!(!out.status.success());
    let message = String::from_utf8_lossy(&out.stderr);
    assert!(message.contains("dirty"), "{message}");
    let canonical = std::fs::canonicalize(&path).unwrap();
    let quoted = format!("'{}'", canonical.to_string_lossy().replace('\'', "'\\''"));
    assert!(message.contains(&format!("git worktree remove {quoted}")), "{message}");
    assert!(message.contains("renri remove 'feat/by-hand' -y --non-interactive"), "{message}");
    let queue = magi::queue::Queue::at(cli_home.join("queue"));
    assert!(queue.list().is_empty(), "refused review must not file a task");
    let mut task = magi::queue::Task::new("existing task".into(), "keep task".into(), fx.repo.clone(), magi::queue::Source::Human);
    queue.put(&mut task).unwrap();
    let before_task = serde_json::to_string(&queue.get(&task.id).unwrap()).unwrap();
    let out = common::magi_cli(&cli_home).current_dir(&path)
        .args(["review", BRANCH, "--task", &task.id, "--config"])
        .arg(&config).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("dirty"));
    assert_eq!(serde_json::to_string(&queue.get(&task.id).unwrap()).unwrap(), before_task);
    assert!(Runner::review(&fx.repo, BRANCH, fx.config.clone(), Origin::operator()).await.is_err());
    assert_eq!(std::fs::read_to_string(path.join("scratch.txt")).unwrap(), "keep this\n");
    assert_eq!(git::git(&path, &["status", "--porcelain", "--untracked-files=all"]).await.unwrap(), before);
}
}

common::e2e! {
async fn foreign_checkout_moved_during_review_is_held_with_fixes_preserved() {
    let guard = common::home_lock().await;
    let fx = fixture(guard, Judges::Unanimous, true);
    let path = foreign(&fx).await;
    let mut runner = Runner::review(&fx.repo, BRANCH, fx.config.clone(), Origin::operator()).await.unwrap();
    std::fs::write(path.join("later.txt"), "concurrent work\n").unwrap();
    commit(&path, "later operator work").await;
    let moved = git::rev_parse(&path, "HEAD").await.unwrap();
    runner.execute().await.unwrap();
    assert_eq!(runner.state.status, RunStatus::Blocked);
    assert!(runner.state.merge.is_none());
    assert_eq!(git::rev_parse(&path, "HEAD").await.unwrap(), moved);
    assert!(git::is_clean(&path).await.unwrap());
    assert!(!path.join("fixed.txt").exists());
    let winner = runner.state.winner().unwrap();
    assert!(winner.worktree.join("fixed.txt").exists());
    assert!(runner.state.events.iter().any(|e| e.node == "foreign-checkout"
        && e.message.contains("branch moved") && e.message.contains("merge --ff-only")
        && e.message.contains(&winner.branch)));
    assert!(magi::notices::Notices::open().list().iter().any(|n|
        n.key == format!("foreign-checkout:{}", runner.state.id)
            && n.message.contains("merge --ff-only")));
}
}

#[cfg(unix)]
common::e2e! {
async fn foreign_review_publishes_the_original_branch_in_pr_mode() {
    use std::os::unix::fs::PermissionsExt;
    let guard = common::home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, true);
    let path = foreign(&fx).await;
    let bin = fx.tmp.path().join("forge");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(&script, r#"#!/bin/sh
case "$1 $2" in
  'pr list') echo '[]' ;;
  'pr create') printf '%s\n' "$@" > "$FOREIGN_FORGE_ARGS"; echo 'https://github.com/example/repo/pull/1' ;;
  *) exit 1 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let args_file = fx.tmp.path().join("forge-args");
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    // SAFETY: the e2e harness gives this test its own process.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("FOREIGN_FORGE_ARGS", &args_file);
    }
    fx.config.merge.mode = magi::config::MergeMode::Pr;
    fx.config.graph.land = false;
    let mut runner = Runner::review(&fx.repo, BRANCH, fx.config.clone(), Origin::operator()).await.unwrap();
    runner.execute().await.unwrap();
    assert!(runner.state.merge.as_ref().unwrap().ok, "{:?}", runner.state.events);
    let args = std::fs::read_to_string(args_file).unwrap();
    assert!(args.contains("--head\nfeat/by-hand\n"), "{args}");
    assert_eq!(git::remote_head(&fx.repo, "origin", BRANCH).await.unwrap().unwrap(),
        git::rev_parse(&path, "HEAD").await.unwrap());
    assert!(git::remote_head(&fx.repo, "origin", &runner.state.winner().unwrap().branch).await.unwrap().is_none());
    assert!(git::is_clean(&path).await.unwrap());
}
}
