//! Process-level operator delivery, stopped-run receipts and same-run repair.
mod common;

use common::{Judges, fixture, home_lock, magi_cli};
use magi::{
    graph::Runner,
    queue::{Queue, Source, Task, TaskStatus},
    repair_budget::{Kind, Phase, Request},
    run::{Origin, RunState, RunStatus},
};
use std::process::Output;

const GATE: &str = "test -f gatefix.txt || { echo 'gatefix.txt is missing'; exit 1; }";

async fn stopped(review: bool) -> (common::Fixture, Task, Runner, Request) {
    let mut fx = fixture(home_lock().await, Judges::Unanimous, review);
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    fx.config.graph.review_rounds = 1;
    fx.config.graph.gate_fix_rounds = 0;
    fx.config.disk.min_free_bytes = 0;
    fx.config.disk.auto_fold = false;
    if !review {
        fx.config.verify.gate = vec![GATE.to_owned()];
    } else {
        fx.config.verify.e2e =
            vec!["test -f fixed.txt || { echo 'fixed.txt is missing'; exit 1; }".into()];
    }
    let mut task = Task::new(
        "repair fixture".into(),
        "create note.txt".into(),
        fx.repo.clone(),
        Source::Human,
    );
    Queue::open().put(&mut task).unwrap();
    let mut runner = Runner::start(
        &fx.repo,
        task.instruction.clone(),
        fx.config.clone(),
        Origin::queue(&task.id),
    )
    .await
    .unwrap();
    runner.execute().await.unwrap();
    let head = magi::git::rev_parse(&runner.state.winner().unwrap().worktree, "HEAD")
        .await
        .unwrap();
    task.start(runner.state.id.clone());
    task.status = TaskStatus::Failed;
    Queue::open().put(&mut task).unwrap();
    let request = Request {
        replace_request: None,
        evidence_digest: None,
        tip: None,
        instruction: None,
        task: task.id.clone(),
        run: runner.state.id.clone(),
        kind: if review { Kind::Review } else { Kind::Gate },
        rounds: 1,
        head,
        expect_pass: runner.state.passes.last().unwrap().index,
        request: "repair-1".into(),
        reason: "one additional repair after inspecting failure".into(),
    };
    (fx, task, runner, request)
}

fn command(request: &Request) -> std::process::Command {
    let mut command = magi_cli(&magi::run::home());
    command.args([
        "task",
        "grant-repair",
        &request.task,
        "--run",
        &request.run,
        "--kind",
        if request.kind == Kind::Gate {
            "gate"
        } else {
            "review"
        },
        "--rounds",
        &request.rounds.to_string(),
        "--head",
        &request.head,
        "--expect-pass",
        &request.expect_pass.to_string(),
        "--request",
        &request.request,
        "--reason",
        &request.reason,
    ]);
    command
}

fn cli(request: &Request) -> Output {
    command(request).output().unwrap()
}

fn accepted(request: &Request) -> serde_json::Value {
    let out = cli(request);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn refused(request: &Request, reason: &str) {
    let before = std::fs::read(Queue::open().path_of(&request.task)).unwrap();
    let out = cli(request);
    assert!(
        !out.status.success(),
        "expected refusal: {reason}; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(reason),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(Queue::open().path_of(&request.task)).unwrap(),
        before
    );
}

common::e2e! {
async fn exhausted_gate_grant_is_idempotent_and_repairs_the_exact_queued_run() {
    let (fx, task, runner, request) = stopped(false).await;
    assert_eq!(runner.state.status, RunStatus::Blocked);
    let snapshot = serde_json::to_value(&runner.state.config).unwrap();
    let receipt = accepted(&request);
    assert_eq!(accepted(&request), receipt, "a separate CLI process returns the same receipt");
    let mut conflict = request.clone(); conflict.rounds = 2;
    refused(&conflict, "different arguments");
    assert_eq!(RunState::load(&request.run).unwrap().repair_grants.len(), 0, "acceptance only changes task record");
    assert_eq!(Queue::open().get(&task.id).unwrap().repair_grants.len(), 1);
    assert_eq!(Queue::open().get(&task.id).unwrap().attempts, task.attempts);
    let mut old_snapshot = task.clone();
    Queue::open().put(&mut old_snapshot).unwrap();
    let preserved = Queue::open().get(&task.id).unwrap();
    assert_eq!(preserved.repair_grants.len(), 1, "old queue writers cannot erase receipt");
    assert_eq!(preserved.status, TaskStatus::Queued);
    assert_eq!(preserved.resume_override.as_ref().unwrap().pinned_run.as_ref(), Some(&request.run));

    // Exercise dispatch, execution lease acquisition and receipt import in a
    // real worker process, not merely Runner::resume in this test host.
    let config = fx.tmp.path().join("daemon.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    let opts = magi::daemon::Opts { repo: fx.repo.clone(), config: Some(config), once: true,
        poll: std::time::Duration::from_millis(20), ..Default::default() };
    tokio::time::timeout(std::time::Duration::from_secs(120), magi::daemon::serve(opts)).await.unwrap().unwrap();
    let after = RunState::load(&request.run).unwrap();
    let queued = Queue::open().get(&task.id).unwrap();
    assert_eq!(queued.runs, vec![request.run.clone()]);
    assert_eq!(queued.status, TaskStatus::Done, "{:?}", queued.last_error);
    assert_eq!(serde_json::to_value(&after.config).unwrap(), snapshot);
    assert_eq!(after.repair_grants.len(), 1);
    assert_eq!(after.repair_grants[0].spent, 1);
    assert_eq!(after.repair_grants[0].phase, Phase::Complete);
    assert!(after.repair_grants[0].succeeded);
    assert_eq!(after.gate_fixes.len(), 1);
    assert!(after.reviews.len() > runner.state.reviews.len());
    assert!(after.reviews.last().unwrap().clean);
    let head = magi::git::rev_parse(&after.winner().unwrap().worktree, "HEAD").await.unwrap();
    assert_ne!(head, request.head);
    assert_eq!(after.gate_head.as_ref(), Some(&head));
    assert!(after.gate.iter().all(|o| o.ok()));
    assert_eq!(accepted(&request), receipt, "settled retries do not queue another pass");
    assert_eq!(Queue::open().get(&task.id).unwrap().status, TaskStatus::Done);
}
}

common::e2e! {
async fn grant_refuses_stale_head_pass_ownership_holds_and_live_driver_without_mutation() {
    let (_fx, mut task, mut runner, request) = stopped(false).await;
    let mut invalid = request.clone(); invalid.rounds = 0;
    refused(&invalid, "invalid value"); invalid.rounds = 4;
    refused(&invalid, "invalid value");
    let out = command(&request).env("MAGI_RUN", &request.run).env("MAGI_NODE", "implement").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("operator terminal"));
    let mut stale = request.clone(); stale.head = "0".repeat(40);
    refused(&stale, "expected head");
    stale = request.clone(); stale.expect_pass += 1;
    refused(&stale, "pass changed");
    task.hold_manual(Some("owner hold".into())); Queue::open().put(&mut task).unwrap();
    refused(&request, "owner hold");
    task.release(); task.status = TaskStatus::Failed; Queue::open().put(&mut task).unwrap();
    let original = runner.state.origin.clone();
    runner.state.origin = Some(Origin::queue("different-task")); runner.state.save().unwrap();
    refused(&request, "ownership changed");
    runner.state.origin = original;
    runner.state.save().unwrap();
    let lease = std::fs::OpenOptions::new().read(true).write(true).open(runner.state.dir().join("driver.lock")).unwrap();
    #[cfg(unix)] {
        use std::os::fd::AsRawFd;
        // SAFETY: the live file descriptor stays owned until the lease drops.
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    }
    #[cfg(windows)] {
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn LockFile(file: *mut std::ffi::c_void, offset_low: u32, offset_high: u32,
                length_low: u32, length_high: u32) -> i32;
        }
        // SAFETY: live handle and the same nonblocking Win32 lease as the
        // driver uses; closing the file below releases the locked byte.
        assert_ne!(unsafe { LockFile(lease.as_raw_handle(), 0, 0, 1, 0) }, 0);
    }
    refused(&request, "execution lease unavailable");
    drop(lease);
    let queue = Queue::open(); let _claim = queue.claim(&task.id).unwrap();
    refused(&request, "queue lease is held");
}
}

common::e2e! {
async fn exhausted_review_grant_fixes_saved_findings_and_requires_a_new_clean_review() {
    let (_fx, _task, runner, request) = stopped(true).await;
    assert_eq!(runner.state.reviews.len(), 1);
    assert!(!runner.state.reviews[0].clean);
    accepted(&request);
    let mut resumed = Runner::resume(&request.run).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    assert!(resumed.state.repair_grants[0].succeeded);
    assert!(resumed.state.reviews[0].fix.is_some(), "last unfixed review must reach fixer first");
    assert_eq!(resumed.state.reviews.len(), 2);
    assert!(resumed.state.reviews[1].clean);
    assert!(resumed.state.gate_allows_progress());
}
}

common::e2e! {
async fn accepted_grant_survives_restart_but_changed_head_or_decision_prevents_consumption() {
    let (_fx, mut task, _runner, request) = stopped(false).await;
    accepted(&request);
    task = Queue::open().get(&task.id).unwrap();
    task.hold_manual(Some("new owner decision".into())); Queue::open().put(&mut task).unwrap();
    let mut resumed = Runner::resume(&request.run).unwrap();
    let error = resumed.execute().await.unwrap_err();
    assert!(error.to_string().contains("owner hold"), "{error:#}");
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    assert_eq!(Queue::open().get(&task.id).unwrap().repair_grants.len(), 1);
}
}

common::e2e! {
async fn reserved_attempt_after_crash_preserves_work_and_is_never_dispatched_twice() {
    let (fx, mut task, mut runner, request) = stopped(false).await;
    let config = fx.tmp.path().join("resume.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    task.overrides = Some(magi::queue::RunOverrides { config: Some(config), ..Default::default() });
    Queue::open().put(&mut task).unwrap();
    accepted(&request);
    let receipt = Queue::open().get(&request.task).unwrap().repair_grants[0].clone();
    // Simulate a killed driver after reservation and a fixer writing a file.
    // The reservation is consumed even if its result was never persisted.
    runner.state.repair_grants.push(magi::repair_budget::Applied {
        receipt, spent: 1, phase: Phase::Fixing, review_start: runner.state.reviews.len(),
        head: request.head.clone(), succeeded: false, conflict: None,
    });
    let worktree = runner.state.winner().unwrap().worktree.clone();
    std::fs::write(worktree.join("gatefix.txt"), "work retained across driver exit\n").unwrap();
    runner.state.save().unwrap();
    drop(runner);
    let output = magi_cli(&magi::run::home()).current_dir(&fx.repo)
        .args(["run", "--resume", &request.run]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let resumed = Runner::resume(&request.run).unwrap();
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    assert!(resumed.state.repair_grants[0].succeeded);
    assert!(resumed.state.gate_fixes.is_empty(), "no second fixer call was made");
    assert_eq!(std::fs::read_to_string(worktree.join("gatefix.txt")).unwrap(), "work retained across driver exit\n");
    let head = magi::git::rev_parse(&worktree, "HEAD").await.unwrap();
    assert!(magi::git::git_raw(&worktree, &["merge-base", "--is-ancestor", &request.head, &head]).await.unwrap().ok(), "unpushed implementation commits were preserved");
    assert_eq!(resumed.state.config.graph.gate_fix_rounds, 0);
    let mut again = Runner::resume(&request.run).unwrap(); again.execute().await.unwrap();
    assert_eq!(again.state.repair_grants[0].spent, 1);
    assert!(again.state.gate_fixes.is_empty());
}
}

common::e2e! {
async fn moved_head_after_acceptance_keeps_the_receipt_but_applies_no_budget() {
    let (_fx, _task, runner, request) = stopped(false).await;
    accepted(&request);
    let worktree = &runner.state.winner().unwrap().worktree;
    assert!(magi::git::git_raw(worktree, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-m", "another retained commit"]).await.unwrap().ok());
    let mut resumed = Runner::resume(&request.run).unwrap();
    let error = resumed.execute().await.unwrap_err();
    assert!(error.to_string().contains("head changed"), "{error:#}");
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    assert_eq!(Queue::open().get(&request.task).unwrap().repair_grants.len(), 1);
}
}

common::e2e! {
async fn spent_red_grant_never_publishes_or_restarts_its_fixer_on_resume() {
    let (_fx, _task, mut runner, request) = stopped(false).await;
    // Keep the original configured gate but make the fixer a no-op.
    for agent in &mut runner.state.config.agents {
        agent.env.insert("MOCK_GATE_FIX_NOOP".into(), "1".into());
    }
    runner.state.save().unwrap();
    accepted(&request);
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.gate_fixes.len(), 1);
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    assert!(!resumed.state.repair_grants[0].succeeded);
    assert!(resumed.state.merge.is_none());
    assert!(!resumed.state.verification_history.is_empty());
    let mut again = Runner::resume(&request.run).unwrap(); again.execute().await.unwrap();
    assert_eq!(again.state.status, RunStatus::Blocked);
    assert_eq!(again.state.gate_fixes.len(), 1);
    assert!(again.state.merge.is_none());
}
}

#[cfg(unix)]
common::e2e! {
async fn descendant_output_lease_refuses_a_grant_even_after_driver_exit() {
    use std::os::fd::AsRawFd;
    let (_fx, _task, runner, request) = stopped(false).await;
    let worktree = &runner.state.winner().unwrap().worktree;
    let dir = magi::git::git_raw(worktree, &["rev-parse", "--absolute-git-dir"]).await.unwrap().stdout;
    let lease = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(std::path::Path::new(dir.trim()).join("magi-output.lock")).unwrap();
    // SAFETY: the descriptor remains owned until this guard is dropped.
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) }, 0);
    refused(&request, "descendant still owns");
    drop(lease);
    accepted(&request);
}
}

#[cfg(unix)]
common::e2e! {
async fn exhausted_landing_gate_repairs_in_place_and_still_requires_owner_merge_approval() {
    use std::os::unix::fs::PermissionsExt;
    use magi::{config::MergeMode, run::{MergeOutcome, PrRecord}};
    let (fx, _task, mut runner, request) = stopped(false).await;
    let winner = runner.state.winner().unwrap().clone();
    let public = format!("magi/task-{}", request.task);
    assert!(magi::git::git_raw(&winner.worktree, &["push", "origin", &format!("HEAD:refs/heads/{public}")]).await.unwrap().ok());
    let bin = fx.tmp.path().join("forge"); std::fs::create_dir(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(&script, r#"#!/bin/sh
printf '%s\n' "$*" >> "$GRANT_FORGE/calls"
sha=$(git ls-remote --refs origin "refs/heads/$GRANT_BRANCH" | cut -f1)
if test -f "$GRANT_FORGE/moved"; then sha=0000000000000000000000000000000000000000; fi
case "$1 $2" in
 'pr view') printf '{"url":"https://github.com/example/repo/pull/42","number":42,"state":"OPEN","title":"feat: existing work","reviews":[],"comments":[],"mergeStateStatus":"CLEAN","headRefOid":"%s","baseRefName":"main","autoMergeRequest":null,"headRefName":"%s","isCrossRepository":false}\n' "$sha" "$GRANT_BRANCH" ;;
 'pr list') printf '[{"url":"https://github.com/example/repo/pull/42","title":"feat: existing work","headRefName":"%s","baseRefName":"main"}]\n' "$GRANT_BRANCH" ;;
 'api graphql') printf '{"data":{"repository":{"pullRequest":{"commits":{"nodes":[{"commit":{"oid":"%s","statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]}}}}}\n' "$sha" ;;
 'api repos/{owner}/{repo}/pulls/42/comments') echo '[]' ;;
 'pr edit'|'pr comment') exit 0 ;;
 'pr merge') case "$*" in *--disable-auto*) exit 0 ;; *) exit 99 ;; esac ;;
 *) exit 98 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin.clone()]; paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    // SAFETY: isolated test process, between graph executions.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("GRANT_FORGE", &bin);
        std::env::set_var("GRANT_BRANCH", &public);
    }
    runner.state.config.merge.mode = MergeMode::Pr;
    runner.state.config.graph.land = true;
    runner.state.config.graph.land_approval = false;
    runner.state.pr_head = Some(public.clone());
    runner.state.pr_pushed_head = Some(request.head.clone());
    runner.state.pr = Some(PrRecord { url: "https://github.com/example/repo/pull/42".into(), number: 42,
        state: "open".into(), checks: "green".into(), round: 0, rounds: 1, red_at_merge: vec![] });
    runner.state.merge = Some(MergeOutcome { mode: MergeMode::Pr, ok: false,
        detail: "landing gate failed".into(), empty: false });
    runner.state.status = RunStatus::Landing;
    runner.state.save().unwrap();
    std::fs::write(bin.join("moved"), "").unwrap();
    refused(&request, "public head changed");
    std::fs::remove_file(bin.join("moved")).unwrap();
    accepted(&request);
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert_eq!(resumed.state.gate_fixes.len(), 1, "landing shortcut must reach the actual fixer");
    assert!(resumed.state.repair_grants[0].succeeded);
    assert_eq!(resumed.state.pr_head.as_ref(), Some(&public));
    assert_eq!(resumed.state.pr.as_ref().unwrap().number, 42);
    assert!(resumed.state.land_approval.is_some(), "grant never approves merge");
    assert!(resumed.state.parked);
    let head = magi::git::rev_parse(&winner.worktree, "HEAD").await.unwrap();
    assert_eq!(resumed.state.pr_pushed_head.as_ref(), Some(&head));
    assert_eq!(resumed.state.gate_head.as_ref(), Some(&head));
    let calls = std::fs::read_to_string(bin.join("calls")).unwrap();
    assert!(!calls.contains("pr create"));
    assert!(!calls.contains("pr close"));
    assert!(!calls.lines().any(|line| line.starts_with("pr merge") && !line.contains("--disable-auto")), "{calls}");
}
}

common::e2e! {
async fn concurrent_delivery_and_old_queue_writes_cannot_multiply_grants() {
    let (_fx, mut task, _runner, request) = stopped(false).await;
    let first = request.clone(); let second = request.clone();
    let a = std::thread::spawn(move || cli(&first));
    let b = std::thread::spawn(move || cli(&second));
    let results = [a.join().unwrap(), b.join().unwrap()];
    assert!(results.iter().any(|o| o.status.success()));
    for output in results.iter().filter(|o| !o.status.success()) {
        assert!(String::from_utf8_lossy(&output.stderr).contains("queue lease is held"));
    }
    accepted(&request);
    Queue::open().put(&mut task).unwrap();
    let saved = Queue::open().get(&task.id).unwrap();
    assert_eq!(saved.repair_grants.len(), 1);
    assert_eq!(saved.status, TaskStatus::Queued);
    assert_eq!(saved.resume_override.unwrap().pinned_run, Some(request.run));
}
}

common::e2e! {
async fn additional_gate_attempt_preserves_exhausted_history_and_counters() {
    let (_fx, _task, mut runner, mut request) = stopped(false).await;
    runner.state.config.graph.gate_fix_rounds = 1;
    runner.state.config.verify.gate = vec!["test -f gatefix.txt && test $(wc -l < gatefix.txt) -ge 2 || { echo 'gatefix.txt needs two lines'; exit 1; }".into()];
    runner.state.gate_ran = false;
    runner.state.gate.clear();
    runner.state.save().unwrap();
    runner.execute().await.unwrap();
    assert_eq!(runner.state.status, RunStatus::Blocked);
    assert_eq!(runner.state.gate_fixes.len(), 1);
    let historical = serde_json::to_value(&runner.state.gate_fixes[0]).unwrap();
    request.head = magi::git::rev_parse(&runner.state.winner().unwrap().worktree, "HEAD").await.unwrap();
    request.expect_pass = runner.state.passes.last().unwrap().index;
    accepted(&request);
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert!(resumed.state.repair_grants[0].succeeded);
    assert_eq!(resumed.state.gate_fixes.len(), 2);
    assert_eq!(serde_json::to_value(&resumed.state.gate_fixes[0]).unwrap(), historical);
    assert_eq!(resumed.state.config.graph.gate_fix_rounds, 1);
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
}
}

common::e2e! {
async fn review_grant_cannot_bypass_an_unresolved_fixer_continuation() {
    let (_fx, _task, mut runner, request) = stopped(true).await;
    for agent in &mut runner.state.config.agents {
        agent.env.insert("MOCK_FIX_ALWAYS_INCOMPLETE_SEAT".into(), "impl-A".into());
    }
    runner.state.save().unwrap();
    accepted(&request);
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.reviews.len(), 1, "no new review while continuation is unresolved");
    assert!(resumed.state.reviews[0].fix.as_ref().unwrap().failed.is_some());
    assert_eq!(resumed.state.repair_grants[0].phase, Phase::Complete);
    assert!(!resumed.state.repair_grants[0].succeeded);
    assert!(resumed.state.merge.is_none());
}
}

common::e2e! {
async fn granted_confirmation_review_recovers_cleanliness_after_unresolved_e2e_on_restart() {
    for deferred in [false, true] {
    let (_fx, _task, mut runner, request) = stopped(false).await;
    runner.state.config.verify.e2e = vec!["test -f gatefix.txt".into()];
    runner.state.save().unwrap();
    accepted(&request);
    let receipt = Queue::open().get(&request.task).unwrap().repair_grants[0].clone();
    let worktree = runner.state.winner().unwrap().worktree.clone();
    std::fs::write(worktree.join("gatefix.txt"), "retained fixer output\n").unwrap();
    assert!(magi::git::git_raw(&worktree, &["add", "gatefix.txt"]).await.unwrap().ok());
    assert!(magi::git::git_raw(&worktree, &["commit", "-m", "repair gate fixture"]).await.unwrap().ok());
    let head = magi::git::rev_parse(&worktree, "HEAD").await.unwrap();
    let review_start = runner.state.reviews.len();
    let mut round = runner.state.reviews.last().unwrap().clone();
    round.round += 1;
    round.head = head.clone();
    round.verified_head = Some(head.clone());
    round.clean = false;
    round.e2e = vec![magi::run::CommandOutcome {
        command: "test -f gatefix.txt".into(), code: None,
        output_tail: "shared build cache is busy".into(), duration_ms: 0,
        resource_blocked: true,
    }];
    if deferred {
        round.e2e.clear();
        round.e2e_deferred = true;
        round.verified_head = None;
    }
    assert_eq!(round.answered, round.expected);
    assert_eq!(round.blocking, 0);
    runner.state.reviews.push(round);
    runner.state.repair_grants.push(magi::repair_budget::Applied {
        receipt, spent: 1, phase: Phase::Reviewing, review_start,
        head: head.clone(), succeeded: false, conflict: None,
    });
    runner.state.status = RunStatus::Reviewing;
    runner.state.save().unwrap();
    drop(runner);

    let mut resumed = Runner::resume(&request.run).unwrap();
    resumed.execute().await.unwrap();
    let last = resumed.state.reviews.last().unwrap();
    assert!(last.clean, "successful catch-up must refresh the saved verdict");
    assert_eq!(last.verified_head.as_ref(), Some(&head));
    assert_eq!(last.e2e_status(), magi::run::E2eStatus::Passed);
    assert_eq!(resumed.state.reviews.len(), review_start + 1, "reuse the answered panel");
    assert!(resumed.state.repair_grants[0].succeeded);
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    assert_eq!(resumed.state.gate_head.as_ref(), Some(&head));
    assert!(resumed.state.gate_allows_progress());
    let mut again = Runner::resume(&request.run).unwrap();
    again.execute().await.unwrap();
    assert!(again.state.repair_grants[0].succeeded);
    assert_eq!(again.state.repair_grants[0].spent, 1);
    }
}
}

#[cfg(unix)]
common::e2e! {
async fn retained_reviewer_descendant_refuses_grant_and_resume_without_mutation() {
    use std::os::fd::AsRawFd;
    let (_fx, _task, runner, request) = stopped(false).await;
    // This seat is intentionally absent from candidates, just like the
    // detached confirmation-review trees retained by the graph.
    let review = runner.state.worktree_root().join("review-retained");
    assert!(magi::git::git_raw(&runner.state.repo, &["worktree", "add", "--detach", review.to_str().unwrap(), &request.head]).await.unwrap().ok());
    let dir = magi::git::git_raw(&review, &["rev-parse", "--absolute-git-dir"]).await.unwrap().stdout;
    let lock_path = std::path::Path::new(dir.trim()).join("magi-output.lock");
    let take = || {
        let lease = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(&lock_path).unwrap();
        // SAFETY: the descriptor remains owned until the guard is dropped.
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) }, 0);
        lease
    };
    let lease = take();
    refused(&request, "descendant still owns");
    drop(lease);
    accepted(&request);
    let lease = take();
    let before = RunState::load(&request.run).unwrap();
    let mut resumed = Runner::resume(&request.run).unwrap();
    let error = resumed.execute().await.unwrap_err();
    assert!(error.to_string().contains("descendant still owns"), "{error:#}");
    let mut after = RunState::load(&request.run).unwrap();
    // Driver cleanup may refresh the timestamp while releasing admission.
    after.updated_at = before.updated_at;
    // Driver completion is execution activity, so its marker advances too.
    after.output_activity_at = before.output_activity_at;
    assert_eq!(serde_json::to_value(after).unwrap(), serde_json::to_value(before).unwrap());
    drop(lease);
    let mut resumed = Runner::resume(&request.run).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.repair_grants[0].succeeded);
}
}

common::e2e! {
async fn ordinary_gate_failure_with_unused_budget_still_refuses_an_extra_grant() {
    let (_fx, _task, mut runner, request) = stopped(false).await;
    runner.state.config.graph.gate_fix_rounds = 2;
    runner.state.save().unwrap();
    assert!(runner.state.gate_fixes.is_empty());
    refused(&request, "gate repair budget is not exhausted");
}
}

fn dependency_question(task: &Task) -> magi::ask::Question {
    let mut q = magi::ask::Question::new(
        task.id.clone(),
        magi::triage::DEPS_NODE.into(),
        "triage".into(),
        "Release the dependency?".into(),
        "Retained work is waiting".into(),
        vec![
            "release the dependency".into(),
            "discard the dependency".into(),
            "detach the dependants".into(),
        ],
    );
    q.triage_generation = Some(task.resumption.as_ref().map_or(0, |r| r.generation));
    magi::ask::Questions::open().put_executable(&mut q).unwrap();
    q
}

fn decide_dependency(fx: &common::Fixture, q: &magi::ask::Question) -> String {
    let talks = magi::talk::Talks::open();
    let talk = magi::talk::begin(&talks, &fx.config, fx.repo.clone(), None).unwrap();
    let out = magi_cli(&magi::run::home())
        .args(["task", "control", "decide", &q.id, &q.choices[0]])
        .env("MAGI_NODE", "chat")
        .env("MAGI_RUN", &talk.id)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let message = String::from_utf8(out.stdout).unwrap();
    assert!(message.contains("effect pending"), "{message}");
    talk.id
}

fn settle_worker_projection() {
    magi::task_chat::sweep(
        &Queue::open(),
        &magi::talk::Talks::open(),
        &magi::ask::Questions::open(),
        &magi::run::home(),
    );
}

common::e2e! {
async fn conversation_decision_barriers_fence_acceptance_until_worker_application_survives_restart() {
    let (fx, task, _runner, request) = stopped(true).await;
    let queue = Queue::open();
    let questions = magi::ask::Questions::open();
    let q = dependency_question(&task);
    refused(&request, "unresolved question");
    // The task claim is a deterministic barrier before answer recording. No
    // worker, transport timing or wall-clock sleep is needed to expose the race.
    let talks = magi::talk::Talks::open();
    let talk = magi::talk::begin(&talks, &fx.config, fx.repo.clone(), None).unwrap();
    let barrier = queue.claim(&task.id).unwrap();
    assert!(magi::chat_control::decide(&talks, &magi::run::home(), &talk.id, &q.id, &q.choices[0]).is_err());
    assert!(questions.get(&q.id).unwrap().status.open());
    drop(barrier);
    decide_dependency(&fx, &q);
    // A separate grant process observes the recorded response before the
    // asynchronous worker runs, including after stores are reopened.
    refused(&request, "owner effect");
    let outbox = magi::task_inquiry::Outbox::at(&magi::run::home());
    assert_eq!(outbox.get(&q.id).unwrap().decision.unwrap().status, magi::task_decision::Status::Queued);
    let barrier = queue.claim(&task.id).unwrap();
    assert!(magi::triage::apply_answers(&queue, &questions).is_empty());
    drop(barrier);
    assert_eq!(magi::triage::apply_answers(&Queue::open(), &magi::ask::Questions::open()), std::slice::from_ref(&task.id));
    // Application is atomic in the task; projection can be repaired after a
    // crash without executing the answer again.
    assert!(magi::triage::apply_answers(&Queue::open(), &questions).is_empty());
    settle_worker_projection();
    assert_eq!(outbox.get(&q.id).unwrap().decision.unwrap().status, magi::task_decision::Status::Resolved);
    let receipt = accepted(&request);
    assert!(receipt["decision"].is_string());
    assert_eq!(accepted(&request), receipt);
    let mut resumed = Runner::resume(&request.run).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.repair_grants.len(), 1);
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    assert!(resumed.state.repair_grants[0].succeeded);
}
}

common::e2e! {
async fn historical_unspent_receipt_recovers_only_the_exact_original_answer_once() {
    let (fx, task, runner, request) = stopped(true).await;
    // Capture a historical acceptance snapshot. Reconstitute only the old
    // admission ordering in this fixture; production acceptance now refuses it.
    accepted(&request);
    let queue = Queue::open();
    let mut historical = queue.get(&task.id).unwrap();
    let q = dependency_question(&historical);
    let talk = decide_dependency(&fx, &q);
    let q = magi::ask::Questions::open().get(&q.id).unwrap();
    historical.repair_grants[0].at = jiff::Timestamp::now();
    queue.put(&mut historical).unwrap();
    let original = historical.repair_grants[0].clone();
    assert_eq!(magi::triage::apply_answers(&queue, &magi::ask::Questions::open()), std::slice::from_ref(&task.id));
    settle_worker_projection();
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("owner decision changed"));
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    let config_before = serde_json::to_value(&runner.state.config).unwrap();
    let reviews_before = serde_json::to_value(&runner.state.reviews).unwrap();
    let mut recovery = magi::repair_budget::reconcile::Request {
        task: task.id.clone(), run: request.run.clone(), head: request.head.clone(), expect_pass: request.expect_pass,
        receipt: request.request.clone(), question: q.id.clone(), answer_at: q.answered_at.unwrap(),
        request: "recover-original-answer".into(), reason: "owner approved reconciliation of the original accepted repair".into(),
    };
    // An unrelated pending decision is not an admission blocker.
    let mut other = Task::new("unrelated".into(), "wait".into(), fx.repo.clone(), Source::Human);
    queue.put(&mut other).unwrap();
    let unrelated = dependency_question(&other);
    decide_dependency(&fx, &unrelated);
    let mut wrong = recovery.clone(); wrong.answer_at = q.asked_at;
    assert!(magi::repair_budget::reconcile::apply(&queue, &magi::run::home(), wrong).await.is_err());
    recovery.expect_pass += 1;
    assert!(magi::repair_budget::reconcile::apply(&queue, &magi::run::home(), recovery.clone()).await.is_err());
    recovery.expect_pass -= 1;
    // Existing generic dependency questions are superseded by the explicit
    // recovery, without turning them into another owner release.
    let saved = queue.get(&task.id).unwrap();
    let generic_resume = dependency_question(&saved);
    decide_dependency(&fx, &generic_resume);
    magi::triage::apply_answers(&queue, &magi::ask::Questions::open());
    settle_worker_projection();
    let after_generic = queue.get(&task.id).unwrap();
    assert_eq!(after_generic.resumption, saved.resumption, "generic resume must not replace the original application");
    assert!(after_generic.triage_applied(&generic_resume.id));
    let generic_effect = magi::task_inquiry::Outbox::at(&magi::run::home()).get(&generic_resume.id).unwrap().decision.unwrap();
    assert_eq!(generic_effect.status, magi::task_decision::Status::Resolved);
    assert!(generic_effect.history.last().unwrap().evidence.contains("without release"));
    let generic = dependency_question(&after_generic);
    let dependent = {
        let mut t = Task::new("dependent".into(), "wait".into(), fx.repo.clone(), Source::Human);
        t.block(vec![task.id.clone()], Some("dependency".into())); queue.put(&mut t).unwrap(); t
    };
    let mut held = queue.get(&task.id).unwrap(); held.hold_machine(Some("repair grant held: owner decision changed after acceptance".into())); queue.put(&mut held).unwrap();
    magi::triage::run_once(&queue, &magi::ask::Questions::open(), None, jiff::Timestamp::now());
    let notices = magi::task_inquiry::Outbox::at(&magi::run::home()).list().unwrap();
    assert!(notices.iter().any(|i| i.task == task.id && i.display.contains("reconcile-repair-grant") && i.display.contains(&q.answered_at.unwrap().to_string())));
    assert_eq!(queue.get(&dependent.id).unwrap().blocked_by, std::slice::from_ref(&task.id));
    let receipt = magi::repair_budget::reconcile::apply(&queue, &magi::run::home(), recovery.clone()).await.unwrap();
    assert_eq!(receipt.grant, original);
    assert!(receipt.superseded_questions.iter().any(|(id, _)| id == &generic.id));
    // A writer loaded before reconciliation must retain the recovery's queue
    // state and exact-run pin, even though it already knew the original grant.
    let recovered = queue.get(&task.id).unwrap();
    assert_eq!(recovered.status, TaskStatus::Queued);
    assert_eq!(recovered.resume_override.as_ref().unwrap().pinned_run.as_deref(), Some(request.run.as_str()));
    queue.put(&mut held).unwrap();
    let after_stale_write = queue.get(&task.id).unwrap();
    assert_eq!(after_stale_write.repair_grants, recovered.repair_grants);
    assert_eq!(after_stale_write.repair_reconciliations, recovered.repair_reconciliations);
    assert_eq!(after_stale_write.resume_override, recovered.resume_override);
    assert_eq!(after_stale_write.status, recovered.status);
    assert_eq!(after_stale_write.hold_source, recovered.hold_source);
    assert_eq!(after_stale_write.hold_reason, recovered.hold_reason);
    assert_eq!(after_stale_write.held_at, recovered.held_at);
    // Terminal/chat use the same operation; a lost reply is safely replayed.
    let output = magi_cli(&magi::run::home()).args(["task", "control", "reconcile-repair-grant", &task.id,
        "--run", &request.run, "--head", &request.head, "--expect-pass", &request.expect_pass.to_string(),
        "--receipt", &request.request, "--question", &q.id, "--answer-at", &q.answered_at.unwrap().to_string(),
        "--request", &recovery.request, "--reason", &recovery.reason])
        .env("MAGI_NODE", "chat").env("MAGI_RUN", talk).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let replay: magi::repair_budget::reconcile::Receipt = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(replay, receipt);
    let saved = queue.get(&task.id).unwrap();
    assert_eq!(saved.repair_grants, [original]);
    assert_eq!(saved.repair_reconciliations.len(), 1);
    assert_eq!(saved.operator_decided_at, historical.operator_decided_at.or(Some(receipt.decision)));
    // A later owner hold wins even over an idempotent recovery replay. The
    // replay returns its old receipt, never queues or releases the task again.
    magi::chat_control::hold(&queue, &task.id, "owner now refuses repair").unwrap();
    let held_before = std::fs::read(queue.path_of(&task.id)).unwrap();
    assert_eq!(magi::repair_budget::reconcile::apply(&queue, &magi::run::home(), recovery.clone()).await.unwrap(), receipt);
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), held_before);
    let mut later = recovery.clone(); later.request = "another-recovery".into();
    assert!(magi::repair_budget::reconcile::apply(&queue, &magi::run::home(), later).await.is_err());
    let mut blocked = Runner::resume(&request.run).unwrap();
    assert!(blocked.execute().await.is_err());
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    // Restore the fixture's original application state solely to test the
    // independent source/contract fences; no production recovery does this.
    let mut restore = saved.clone(); queue.put(&mut restore).unwrap();
    let mut changed = magi::ask::Questions::open().get(&q.id).unwrap();
    changed.summary.push_str(" changed source");
    magi::ask::Questions::open().put(&mut changed).unwrap();
    let mut blocked = Runner::resume(&request.run).unwrap();
    assert!(blocked.execute().await.is_err());
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    let mut original_q = q.clone(); magi::ask::Questions::open().put(&mut original_q).unwrap();
    let mut changed = saved.clone(); changed.instruction.push_str(" changed contract"); queue.put(&mut changed).unwrap();
    let mut blocked = Runner::resume(&request.run).unwrap();
    assert!(blocked.execute().await.is_err());
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    let mut restore = saved.clone(); queue.put(&mut restore).unwrap();
    assert_eq!(serde_json::to_value(&RunState::load(&request.run).unwrap().reviews).unwrap(), reviews_before);
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    assert!(resumed.state.repair_grants[0].succeeded);
    assert_eq!(serde_json::to_value(&resumed.state.config).unwrap(), config_before);
    assert_eq!(serde_json::to_value(&resumed.state.reviews[0].reviews).unwrap(), reviews_before[0]["reviews"]);
    assert_eq!(serde_json::to_value(&resumed.state.reviews[0].e2e).unwrap(), reviews_before[0]["e2e"]);
    let mut again = Runner::resume(&request.run).unwrap(); again.execute().await.unwrap();
    assert_eq!(again.state.repair_grants[0].spent, 1);
    assert_eq!(magi::repair_budget::reconcile::apply(&queue, &magi::run::home(), recovery).await.unwrap(), receipt);
}
}

common::e2e! {
async fn superseded_dependency_question_does_not_deadlock_triage_after_repair_fails_again() {
    let (fx, task, mut runner, request) = stopped(false).await;
    // Make the authorized attempt fail its existing gate without changing the
    // retained failure/head/pass that the original receipt authorizes.
    for agent in &mut runner.state.config.agents {
        agent.env.insert("MOCK_GATE_FIX_NOOP".into(), "1".into());
    }
    runner.state.save().unwrap();
    accepted(&request);
    let queue = Queue::open();
    let questions = magi::ask::Questions::open();
    let mut historical = queue.get(&task.id).unwrap();
    let original = dependency_question(&historical);
    let talk = decide_dependency(&fx, &original);
    let original = questions.get(&original.id).unwrap();
    // Recreate only the old answer-before-acceptance/effect-after-acceptance
    // ordering in this isolated fixture, as in the successful recovery test.
    historical.repair_grants[0].at = jiff::Timestamp::now();
    queue.put(&mut historical).unwrap();
    magi::triage::apply_answers(&queue, &questions);
    settle_worker_projection();
    let generic = dependency_question(&queue.get(&task.id).unwrap());
    let mut dependent = Task::new("dependent".into(), "wait".into(), fx.repo.clone(), Source::Human);
    dependent.block(vec![task.id.clone()], Some("retained dependency".into()));
    queue.put(&mut dependent).unwrap();
    let recovery = magi::repair_budget::reconcile::Request {
        task: task.id.clone(), run: request.run.clone(), head: request.head.clone(), expect_pass: request.expect_pass,
        receipt: request.request.clone(), question: original.id.clone(), answer_at: original.answered_at.unwrap(),
        request: "recover-before-red-repair".into(), reason: "owner authorized the original receipt reconciliation".into(),
    };
    let receipt = magi::repair_budget::reconcile::apply(&queue, &magi::run::home(), recovery).await.unwrap();
    assert!(receipt.superseded_questions.iter().any(|(id, _)| id == &generic.id));
    // The source stays Open for audit, but neither a restarted conductor's
    // pending gate nor task-list recovery indicators may call it actionable.
    let recovered = Queue::open().get(&task.id).unwrap();
    assert!(questions.get(&generic.id).unwrap().status.open());
    assert!(!magi::triage::pending_for(&magi::ask::Questions::open(), &recovered));
    assert!(!magi::triage::open_task_ids(&questions).contains(&task.id));
    let out = magi_cli(&magi::run::home()).args(["task", "control", "decide", &generic.id, &generic.choices[0]])
        .env("MAGI_NODE", "chat").env("MAGI_RUN", &talk).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("superseded"));
    // Supersession is exact, not a blanket exclusion for this task or id.
    let mut changed = questions.get(&generic.id).unwrap(); changed.detail.push_str(" changed source");
    questions.put(&mut changed).unwrap();
    assert!(magi::triage::pending_for(&questions, &recovered));
    assert!(magi::triage::open_task_ids(&questions).contains(&task.id));
    let mut restore = generic.clone(); questions.put(&mut restore).unwrap();

    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.repair_grants[0].phase, Phase::Complete);
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    assert!(!resumed.state.repair_grants[0].succeeded);
    let mut held = queue.get(&task.id).unwrap();
    held.hold_machine(Some("authorized repair failed its gate".into())); queue.put(&mut held).unwrap();
    assert_eq!(queue.get(&dependent.id).unwrap().blocked_by, std::slice::from_ref(&task.id));
    assert!(!magi::triage::pending_for(&questions, &held));
    let report = magi::triage::run_once(&queue, &questions, None, jiff::Timestamp::now());
    assert!(report.asked.contains(&task.id), "{report:?}");
    let fresh = questions.list().into_iter().find(|q| q.run == task.id && q.node == magi::triage::DEPS_NODE
        && q.id != generic.id && q.id != original.id && q.status.open()).unwrap();
    assert!(magi::triage::pending_for(&questions, &queue.get(&task.id).unwrap()));
    assert!(magi::triage::open_task_ids(&questions).contains(&task.id));
    assert_eq!(queue.get(&dependent.id).unwrap().blocked_by, std::slice::from_ref(&task.id));
    // The replacement question can be answered through the real control route.
    decide_dependency(&fx, &fresh);
    magi::triage::apply_answers(&queue, &questions);
    settle_worker_projection();
    let after = queue.get(&task.id).unwrap();
    assert!(!magi::triage::pending_for(&questions, &after));
    assert_eq!(after.repair_grants.len(), 1);
    assert_eq!(RunState::load(&request.run).unwrap().repair_grants[0].spent, 1);
    assert_eq!(questions.get(&generic.id).unwrap().answer, None);
}
}
