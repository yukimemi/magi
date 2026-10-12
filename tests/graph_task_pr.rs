//! Task retries publish one stable head while candidates remain per run.
#![cfg(unix)]
mod common;

use common::{Judges, fixture, home_lock};
use magi::{
    config::MergeMode,
    git,
    graph::Runner,
    run::{Origin, RunState},
};
use std::path::Path;

fn forge(dir: &Path) -> std::path::PathBuf {
    let bin = dir.join("forge");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(&script, r#"#!/bin/sh
printf '%s\n' "$1 $2" >> "$TASK_PR_FORGE/calls"
case "$1 $2" in
  'pr list')
    if test -f "$TASK_PR_FORGE/open"; then
      echo '[{"number":1,"url":"https://github.com/example/repo/pull/1","title":"Human title","baseRefName":"main"}]'
    else echo '[]'; fi ;;
  'pr view')
    sha=$(git ls-remote --refs origin refs/heads/magi/task-abcd-1234 2>/dev/null | cut -f1)
    printf '{"headRefName":"magi/task-abcd-1234","baseRefName":"main","isCrossRepository":false,"headRefOid":"%s","state":"OPEN"}\n' "$sha" ;;
  'pr create') touch "$TASK_PR_FORGE/open"; echo 'https://github.com/example/repo/pull/1' ;;
  'pr edit') test ! -f "$TASK_PR_FORGE/fail-edit" || exit 1; printf '%s\n' "$@" >> "$TASK_PR_FORGE/edits" ;;
  'pr merge'|'pr comment') : ;;
  *) exit 1 ;;
esac
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    // SAFETY: e2e starts each test in a separate process before other work.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("TASK_PR_FORGE", &bin);
    }
    bin
}

async fn attempt(repo: &Path, config: &magi::config::Config) -> RunState {
    let mut runner = Runner::start(
        repo,
        "create note.txt".into(),
        config.clone(),
        Origin::queue("abcd-1234"),
    )
    .await
    .unwrap();
    runner.execute().await.unwrap();
    assert!(
        runner.state.merge.as_ref().unwrap().ok,
        "{:?}",
        runner.state.events
    );
    runner.state
}

common::e2e! {
async fn retry_public_head_updates_the_existing_open_pull_request() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    let mut stale = Runner::resume(&first.id).unwrap();
    let second = attempt(&fx.repo, &fx.config).await;
    assert!(stale.execute().await.is_err(), "even an in-memory predecessor cannot resume");
    assert_ne!(first.winner().unwrap().branch, second.winner().unwrap().branch);
    assert_eq!(second.pr_branch(), Some("magi/task-abcd-1234"));
    assert_eq!(first.pr.as_ref().unwrap().number, second.pr.as_ref().unwrap().number);
    let old = RunState::load(&first.id).unwrap();
    assert_eq!(old.pr_taken_over_by.as_deref(), Some(second.id.as_str()));
    let mut resumed = Runner::resume(&old.id).unwrap();
    assert!(resumed.execute().await.is_err());
    let calls = std::fs::read_to_string(forge.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| *l == "pr create").count(), 1);
    assert_eq!(calls.lines().filter(|l| *l == "pr edit").count(), 1);
    assert_eq!(calls.lines().filter(|l| *l == "pr comment").count(), 1);
    let edits = std::fs::read_to_string(forge.join("edits")).unwrap();
    assert!(edits.lines().any(|l| l == "--body"));
    assert!(!edits.lines().any(|l| l == "--title"), "preserve human title");
    assert_eq!(git::remote_head(&fx.repo, "origin", second.pr_branch().unwrap()).await.unwrap(), second.pr_pushed_head);
    for candidate in &second.candidates {
        assert!(!second.pr_branch().unwrap().contains(&candidate.agent));
        assert!(!second.pr_branch().unwrap().ends_with(&format!("/{}", candidate.label)));
    }
}
}

common::e2e! {
async fn retry_public_head_creates_a_pull_request_when_none_is_open() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    std::fs::remove_file(forge.join("open")).unwrap();
    let second = attempt(&fx.repo, &fx.config).await;
    assert_eq!(first.pr_branch(), second.pr_branch());
    let calls = std::fs::read_to_string(forge.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| *l == "pr create").count(), 2);
    assert_eq!(calls.lines().filter(|l| *l == "pr edit").count(), 0);
}
}

common::e2e! {
async fn a_moved_public_head_is_not_overwritten_even_after_fetch() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let head = "magi/task-abcd-1234";
    let initial = git::rev_parse(&fx.repo, "HEAD").await.unwrap();
    assert!(git::push_as(&fx.repo, "origin", "main", head, "").await.unwrap().ok());
    std::fs::write(fx.repo.join("human.txt"), "human change").unwrap();
    git::git(&fx.repo, &["add", "human.txt"]).await.unwrap();
    git::git(&fx.repo, &["commit", "-m", "test: move public head"]).await.unwrap();
    let moved = git::rev_parse(&fx.repo, "HEAD").await.unwrap();
    assert!(git::push_as(&fx.repo, "origin", "main", head, &initial).await.unwrap().ok());
    git::fetch(&fx.repo, "origin", head).await.unwrap();
    git::git(&fx.repo, &["branch", "replacement", &initial]).await.unwrap();
    assert!(!git::push_as(&fx.repo, "origin", "replacement", head, &initial).await.unwrap().ok());
    assert_eq!(git::remote_head(&fx.repo, "origin", head).await.unwrap().as_deref(), Some(moved.as_str()));
    assert!(!git::push_as(&fx.repo, "origin", "replacement", head, "").await.unwrap().ok());
}
}

common::e2e! {
async fn public_head_metadata_failure_resumes_without_competing_again() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    std::fs::write(forge.join("fail-edit"), "").unwrap();
    let mut second = Runner::start(&fx.repo, "create note.txt".into(), fx.config.clone(), Origin::queue("abcd-1234")).await.unwrap();
    second.execute().await.unwrap();
    assert!(!second.state.merge.as_ref().unwrap().ok);
    assert!(second.state.pr_publication_pending);
    assert!(RunState::load(&first.id).unwrap().pr_taken_over_by.is_none());
    let branch = second.state.winner().unwrap().branch.clone();
    let reviews = second.state.reviews.len();
    let verified_tip = git::rev_parse(&fx.repo, &branch).await.unwrap();
    let synced_base = second.state.base_sync.clone();
    // Advance the remote base after verification and the failed metadata call.
    // Publication retry must not rebase the candidate under its old checks.
    std::fs::write(fx.repo.join("base-update.txt"), "new base").unwrap();
    git::git(&fx.repo, &["add", "base-update.txt"]).await.unwrap();
    git::git(&fx.repo, &["commit", "-m", "test: advance base during publication retry"]).await.unwrap();
    git::git(&fx.repo, &["push", "origin", "main"]).await.unwrap();
    std::fs::remove_file(forge.join("fail-edit")).unwrap();
    let mut resumed = Runner::resume(&second.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.merge.as_ref().unwrap().ok, "{:?}", resumed.state.events);
    assert!(!resumed.state.pr_publication_pending);
    assert_eq!(resumed.state.winner().unwrap().branch, branch);
    assert_eq!(resumed.state.reviews.len(), reviews);
    assert_eq!(git::rev_parse(&fx.repo, &branch).await.unwrap(), verified_tip);
    assert_eq!(resumed.state.pr_pushed_head.as_deref(), Some(verified_tip.as_str()));
    assert_eq!(serde_json::to_value(&resumed.state.base_sync).unwrap(), serde_json::to_value(&synced_base).unwrap());
    let calls = std::fs::read_to_string(forge.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| *l == "pr create").count(), 1);
    assert_eq!(RunState::load(&first.id).unwrap().pr_taken_over_by.as_deref(), Some(resumed.state.id.as_str()));
}
}

common::e2e! {
async fn driver_claim_and_publication_takeover_share_an_ownership_lock() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    let mut stale = Runner::resume(&first.id).unwrap();
    let lock = std::fs::OpenOptions::new().read(true).write(true)
        .open(first.dir().join("pr-owner.lock")).unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: lock owns a live descriptor; closing it releases the lock.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    // Model the takeover's critical section: a stale driver cannot claim or
    // save while publication owns the old record.
    let error = stale.execute().await.unwrap_err();
    assert!(format!("{error:#}").contains("ownership is being updated"));
    assert!(RunState::load(&first.id).unwrap().driver_exited);
    let mut second = Runner::start(&fx.repo, "create note.txt".into(), fx.config.clone(), Origin::queue("abcd-1234")).await.unwrap();
    second.execute().await.unwrap();
    assert!(!second.state.merge.as_ref().unwrap().ok);
    assert_eq!(git::remote_head(&fx.repo, "origin", first.pr_branch().unwrap()).await.unwrap(), first.pr_pushed_head);
    drop(lock);
    let mut resumed = Runner::resume(&second.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.merge.as_ref().unwrap().ok);
    // Even the runner whose first check raced publication must now reject
    // the disk takeover without saving its old in-memory state over it.
    assert!(stale.execute().await.is_err());
    assert_eq!(RunState::load(&first.id).unwrap().pr_taken_over_by.as_deref(), Some(resumed.state.id.as_str()));
}
}

fn pr_config(fx: &mut common::Fixture) {
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
}

fn comments(forge: &Path) -> usize {
    std::fs::read_to_string(forge.join("calls")).unwrap().lines().filter(|l| *l == "pr comment").count()
}

common::e2e! {
async fn a_merged_predecessor_is_history_and_does_not_block_a_new_pull_request() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    pr_config(&mut fx);
    let first = attempt(&fx.repo, &fx.config).await;
    let mut merged = RunState::load(&first.id).unwrap();
    merged.pr.as_mut().unwrap().state = "merged".into();
    merged.save().unwrap();
    std::fs::remove_file(forge.join("open")).unwrap();
    let second = attempt(&fx.repo, &fx.config).await;
    let old = RunState::load(&first.id).unwrap();
    assert!(old.pr_taken_over_by.is_none());
    assert_eq!(old.pr.as_ref().unwrap().state, "merged");
    assert_eq!(old.status, first.status);
    assert_eq!(comments(&forge), 0);
    assert_eq!(git::remote_head(&fx.repo, "origin", second.pr_branch().unwrap()).await.unwrap(), second.pr_pushed_head);
}
}

common::e2e! {
async fn a_predecessor_with_another_pull_request_is_not_superseded() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    pr_config(&mut fx);
    let first = attempt(&fx.repo, &fx.config).await;
    let mut other = RunState::load(&first.id).unwrap();
    other.pr.as_mut().unwrap().url = "https://github.com/example/repo/pull/99".into();
    other.pr.as_mut().unwrap().number = 99;
    other.save().unwrap();
    attempt(&fx.repo, &fx.config).await;
    let old = RunState::load(&first.id).unwrap();
    assert!(old.pr_taken_over_by.is_none());
    assert_eq!(old.pr.as_ref().unwrap().number, 99);
    assert_eq!(comments(&forge), 0);
}
}

common::e2e! {
async fn lease_survives_a_merged_run_when_a_later_run_takes_over_the_open_pull_request() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    pr_config(&mut fx);
    let first = attempt(&fx.repo, &fx.config).await;
    let mut merged = RunState::load(&first.id).unwrap();
    merged.pr.as_mut().unwrap().state = "merged".into();
    merged.save().unwrap();
    std::fs::remove_file(forge.join("open")).unwrap();
    // A new pull request on the existing head: the lease comes from the merged run's push.
    let second = attempt(&fx.repo, &fx.config).await;
    // The merged record is touched later (write-through / repair) and must not matter.
    let mut touched = RunState::load(&first.id).unwrap();
    touched.event("merge", "repaired");
    touched.save().unwrap();
    let third = attempt(&fx.repo, &fx.config).await;
    assert!(RunState::load(&first.id).unwrap().pr_taken_over_by.is_none());
    assert_eq!(RunState::load(&second.id).unwrap().pr_taken_over_by.as_deref(), Some(third.id.as_str()));
    assert_eq!(comments(&forge), 1);
    assert_eq!(git::remote_head(&fx.repo, "origin", third.pr_branch().unwrap()).await.unwrap(), third.pr_pushed_head);
}
}
