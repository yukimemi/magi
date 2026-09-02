//! A run must not care what the operator's working copy looks like.
//!
//! `magi serve` exists to drain a queue unattended. While it did that by
//! branching candidates off `HEAD`, it had to refuse to start whenever the
//! repository had uncommitted work - which is most of the time someone is
//! using it. The loop was therefore autonomous only in a repository nobody was
//! touching, which is not a useful kind of autonomous.

mod common;

use magi::graph::Runner;

/// Leave an uncommitted file, and a staged one, in the fixture repository.
fn dirty(repo: &std::path::Path) {
    std::fs::write(repo.join("scratch.txt"), "work in progress\n").expect("write scratch");
    std::process::Command::new("git")
        .args(["add", "scratch.txt"])
        .current_dir(repo)
        .status()
        .expect("git add");
    std::fs::write(repo.join("unstaged.txt"), "more\n").expect("write unstaged");
}

#[tokio::test]
async fn a_run_starts_on_a_dirty_tree_and_branches_off_the_base() {
    let _home = common::home_lock().await;
    let fx = common::fixture(common::Judges::Unanimous, false);
    dirty(&fx.repo);

    // The precondition this test exists for: the tree really is dirty.
    let porcelain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&fx.repo)
        .output()
        .expect("git status");
    let porcelain = String::from_utf8_lossy(&porcelain.stdout);
    assert!(
        porcelain.contains("scratch.txt") && porcelain.contains("unstaged.txt"),
        "the fixture should be dirty, got {porcelain:?}"
    );

    let runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("a dirty working copy must not stop a run");

    // And the run is anchored to a commit that exists in the repository, not to
    // whatever the working copy happened to be sitting on.
    let base = &runner.state.base_branch;
    let tip = std::process::Command::new("git")
        .args(["rev-parse", base])
        .current_dir(&fx.repo)
        .output()
        .expect("git rev-parse");
    let tip = String::from_utf8_lossy(&tip.stdout).trim().to_owned();
    assert_eq!(
        runner.state.base_commit, tip,
        "candidates must branch off {base}, not off HEAD"
    );

    // The uncommitted files are not reachable from the run's base, which is
    // what makes "your work in progress is not part of this run" a fact rather
    // than a warning.
    let listed = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", &runner.state.base_commit])
        .current_dir(&fx.repo)
        .output()
        .expect("git ls-tree");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        !listed.contains("scratch.txt"),
        "a staged-but-uncommitted file must not be in the run's base: {listed}"
    );
}

#[tokio::test]
async fn a_run_branches_off_what_the_remote_has_not_a_stale_local_ref() {
    // The failure this defends: `land` merges the winner on GitHub, nothing
    // updates the local branch, and the next run branches off a base missing
    // everything the previous runs landed. Two tasks in a row from a phone
    // would have had the second reverting the first.
    let _home = common::home_lock().await;
    let fx = common::fixture(common::Judges::Unanimous, false);

    let base = {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&fx.repo)
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };

    // A bare "remote" that is AHEAD of the local branch, which is exactly the
    // state a merged pull request leaves behind.
    let remote_dir = fx.tmp.path().join("origin.git");
    let git = |args: &[&str], cwd: &std::path::Path| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(
        &["init", "--bare", "--quiet", remote_dir.to_str().unwrap()],
        fx.tmp.path(),
    );
    git(
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        &fx.repo,
    );
    git(&["push", "--quiet", "origin", &base], &fx.repo);

    // Land the equivalent of a merged pull request straight onto the remote,
    // through a scratch clone, leaving the fixture's local ref behind.
    let clone = fx.tmp.path().join("elsewhere");
    git(
        &[
            "clone",
            "--quiet",
            remote_dir.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
        fx.tmp.path(),
    );
    std::fs::write(clone.join("landed.txt"), "from a merged pr\n").expect("write");
    git(&["add", "landed.txt"], &clone);
    git(
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--quiet",
            "-m",
            "landed",
        ],
        &clone,
    );
    git(&["push", "--quiet", "origin", "HEAD"], &clone);
    let remote_tip = git(&["rev-parse", "HEAD"], &clone);

    let local_tip = git(&["rev-parse", &base], &fx.repo);
    assert_ne!(local_tip, remote_tip, "the local ref must start out behind");

    let runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");

    // Diagnose in the assertion rather than in a later debugging session: this
    // passed locally and failed on all three CI runners, and "left != right"
    // says nothing about which of fetch, the refspec or the ref name gave way.
    let fetched = std::process::Command::new("git")
        .args([
            "fetch",
            "origin",
            &format!("+refs/heads/{base}:refs/remotes/origin/{base}"),
        ])
        .current_dir(&fx.repo)
        .output()
        .expect("git fetch");
    let tracking = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("origin/{base}"),
        ])
        .current_dir(&fx.repo)
        .output()
        .expect("git rev-parse tracking");
    let refspec = std::process::Command::new("git")
        .args(["config", "--get-all", "remote.origin.fetch"])
        .current_dir(&fx.repo)
        .output()
        .expect("git config");
    assert_eq!(
        runner.state.base_commit,
        remote_tip,
        "a run must branch off what the remote has, not a stale local ref.\n\
         base branch: {base}\n\
         local tip:   {local_tip}\n\
         fetch: status={:?} stderr={:?}\n\
         origin/{base} = {:?}\n\
         refspec = {:?}",
        fetched.status.code(),
        String::from_utf8_lossy(&fetched.stderr).trim(),
        String::from_utf8_lossy(&tracking.stdout).trim(),
        String::from_utf8_lossy(&refspec.stdout).trim(),
    );
    // And the landed file is reachable from the run's base, which is the whole
    // point: the next task builds on the last one.
    let listed = git(
        &["ls-tree", "-r", "--name-only", &runner.state.base_commit],
        &fx.repo,
    );
    assert!(listed.contains("landed.txt"), "got {listed}");
}
