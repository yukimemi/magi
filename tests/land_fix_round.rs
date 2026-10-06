//! A land fix round whose fixer commits for itself.
//!
//! The fixer normally runs `git commit` before it returns, so HEAD has already
//! moved when the round looks at it. Reading HEAD afterwards saw no progress,
//! returned `Declined`, and the caller stopped with "the fixer produced no
//! commit" on a pull request whose head had in fact moved.
mod common;

use std::path::Path;

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::land::{Blocking, Checks, Fixed, PrLifecycle, PrState, fix_round};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

common::e2e! {
async fn a_fixer_that_commits_for_itself_counts_as_progress() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let mut state = runner.state;
    let winner = state.winner().cloned().expect("a winner");

    let remote = fx.tmp.path().join("remote.git");
    git(fx.tmp.path(), &["init", "-q", "--bare", remote.to_str().unwrap()]);
    let remote_name = state.config.merge.remote.clone();
    git(&winner.worktree, &["remote", "set-url", &remote_name, remote.to_str().unwrap()]);
    git(&winner.worktree, &["push", "-q", &remote_name, &winner.branch]);

    let pr = PrState {
        url: "https://example.invalid/pull/1".to_owned(),
        number: 1,
        state: PrLifecycle::Open,
        checks: Checks::Red,
        failing: vec!["clippy (windows)".to_owned()],
        review_comments: Vec::new(),
        blocking: Blocking::Yes,
    };
    let out = fix_round(&mut state, &pr, 1, 2, "a check is failing", "")
        .await
        .expect("fix round");
    let head = git(&winner.worktree, &["rev-parse", "HEAD"]);
    assert_eq!(out, Fixed::Committed { head: head.clone() });

    let pushed = git(&remote, &["rev-parse", &format!("refs/heads/{}", winner.branch)]);
    assert_eq!(pushed, head, "the fixer's own commit must reach the remote");
    assert_eq!(
        git(&winner.worktree, &["log", "-1", "--format=%s"]),
        "fix the failing check",
        "the fixer's commit, not a rescue commit"
    );
    assert_eq!(git(&winner.worktree, &["status", "--porcelain"]), "");
    assert!(
        !state.events.iter().any(|e| e.message.contains("fixer produced no commit")),
        "{:?}",
        state.events
    );
    assert!(state.events.iter().any(|e| e.message.contains("pushed a fix")));
}
}
