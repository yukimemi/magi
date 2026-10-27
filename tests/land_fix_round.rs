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
    fx.config.graph.implementers = 1;

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

common::e2e! {
#[cfg(unix)]
async fn land_rescue_waits_for_the_real_repository_lock() {
    use std::os::fd::AsRawFd as _;
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.graph.implementers = 1;
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await.expect("start");
    runner.execute().await.expect("execute");
    let mut state = runner.state;
    let winner = state.winner().cloned().unwrap();
    let before = git(&winner.worktree, &["rev-parse", "HEAD"]);
    state.config.coordination = Some(toml::from_str(
        "contract_file = 'updater.contract'\ncontract_token = 'test-protocol-v1'\nlock_file = 'updater.lock'"
    ).unwrap());
    // Register after setup so this test isolates the gap after a land seat.
    std::fs::write(fx.repo.join(".git/updater.contract"), "test-protocol-v1\n").unwrap();
    let marker = fx.tmp.path().join("edited");
    let release = fx.tmp.path().join("release");
    let mut fixer = state.config.agents[0].clone();
    fixer.id = "land-lock-test".into();
    fixer.command = vec!["sh".into(), "-c".into(), format!(
        "echo fix > land-edit; touch '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; echo fixed",
        marker.display(), release.display())];
    state.config.roles.fixer = Some(fixer.id.as_str().into());
    state.config.agents.push(fixer);
    state.config.graph.timeout_resource_wait = 1;
    let pr = PrState {
        url: "https://example.invalid/pull/1".into(), number: 1,
        state: PrLifecycle::Open, checks: Checks::Red,
        failing: vec!["check".into()], review_comments: Vec::new(), blocking: Blocking::Yes,
    };
    let (out, external) = tokio::join!(
        fix_round(&mut state, &pr, 1, 2, "check failed", ""),
        async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !marker.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            let lock = std::fs::File::create(fx.repo.join(".git/updater.lock")).unwrap();
            assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
            std::fs::write(&release, "go").unwrap();
            lock
        }
    );
    assert!(out.unwrap_err().to_string().contains("repository coordination"));
    assert_eq!(git(&winner.worktree, &["rev-parse", "HEAD"]), before);
    assert!(git(&winner.worktree, &["status", "--porcelain"]).contains("land-edit"));
    drop(external);
}
}

async fn unavailable_landing_fix_is_refunded_and_retryable(authentication: bool) {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.graph.implementers = 1;
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".into(),
        fx.config.clone(),
        magi::run::Origin::operator(),
    )
    .await
    .unwrap();
    runner.execute().await.unwrap();
    let mut state = runner.state;
    let winner = state.winner().cloned().unwrap();
    let remote = state.config.merge.remote.clone();
    git(&winner.worktree, &["push", "-q", &remote, &winner.branch]);
    state.config.roles.fixer = Some(magi::config::AgentChoice::Chain(vec![
        "beta".to_owned(),
        "gamma".to_owned(),
    ]));
    let outage_script = fx.tmp.path().join("landing-outage.sh");
    // Command wrappers use the observed single-object error format. The
    // fixture's normal landing reply runs before its quota/auth knobs.
    let line = if authentication {
        r#"{"is_error":true,"terminal_reason":"api_error","result":"Failed to authenticate: OAuth session expired and could not be refreshed"}"#
    } else {
        r#"{"is_error":true,"terminal_reason":"api_error","result":"You've hit your session limit"}"#
    };
    std::fs::write(
        &outage_script,
        format!("cat <<'EVENTS'\n{line}\nEVENTS\nexit 1\n"),
    )
    .unwrap();
    for agent in &mut state.config.agents {
        agent.command = vec!["sh".into(), outage_script.to_string_lossy().into_owned()];
    }
    let pr = PrState {
        url: "https://example.invalid/pull/1".into(),
        number: 1,
        state: PrLifecycle::Open,
        checks: Checks::Red,
        failing: vec!["check".into()],
        review_comments: Vec::new(),
        blocking: Blocking::Yes,
    };
    state.pr = Some(magi::run::PrRecord {
        url: pr.url.clone(),
        number: 1,
        state: "open".into(),
        checks: "red".into(),
        round: 0,
        rounds: 1,
        red_at_merge: Vec::new(),
    });
    assert_eq!(
        fix_round(&mut state, &pr, 1, 1, "check failed", "")
            .await
            .unwrap(),
        Fixed::Deferred
    );
    assert_eq!(state.status, magi::run::RunStatus::Blocked);
    assert_eq!(
        state.quota.len(),
        1,
        "only the exhausted chain's final loss"
    );
    assert_eq!(state.quota[0].agent, "gamma");
    assert_eq!(state.quota[0].node, "land");
    assert_eq!(state.handovers.len(), 1);
    let cause = if authentication {
        magi::agent::QuotaCause::Authentication
    } else {
        magi::agent::QuotaCause::UsageLimit
    };
    assert_eq!(state.quota[0].cause, cause);
    let saved = magi::run::RunState::load(&state.id).unwrap();
    assert_eq!(saved.resource_block.as_ref().unwrap().node, "land");
    assert_eq!(saved.pr.as_ref().unwrap().round, 0);
    let queue = magi::queue::Queue::open();
    let mut task = magi::queue::Task::new(
        "fix check".into(),
        "fix check".into(),
        fx.repo.clone(),
        magi::queue::Source::Human,
    );
    task.start(state.id.clone());
    queue.put(&mut task).unwrap();
    magi::daemon::finish_attempt(2, &queue, &mut task, &state, &[], Ok(()));
    assert_eq!(task.attempts, 0, "an open PR must not suppress the refund");
    if authentication {
        assert_eq!(task.status, magi::queue::TaskStatus::Held);
        assert_eq!(task.hold_source, Some(magi::queue::HoldSource::Machine));
        assert!(task.hold_reason.as_ref().unwrap().contains("gamma"));
        assert!(
            task.hold_reason
                .as_ref()
                .unwrap()
                .contains("re-authenticate")
        );
    }
    state.config.agents = fx.config.agents.clone();
    state.resource_block = None; // Cleared by Runner::resume for a new pass.
    let retried = fix_round(&mut state, &pr, 1, 1, "check failed", "")
        .await
        .unwrap();
    assert!(matches!(retried, Fixed::Committed { .. }), "{retried:?}");
}

common::e2e! {
async fn landing_authentication_outage_refunds_the_attempt_and_retries_the_fix() {
    unavailable_landing_fix_is_refunded_and_retryable(true).await;
}
}

common::e2e! {
async fn landing_usage_outage_refunds_the_attempt_and_retries_the_fix() {
    unavailable_landing_fix_is_refunded_and_retryable(false).await;
}
}
