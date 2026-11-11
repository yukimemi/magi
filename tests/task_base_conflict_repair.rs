//! Owner-authorized textual repair keeps the exact review run and both histories.
mod common;
use common::{Judges, fixture, home_lock, magi_cli};
use magi::{
    git,
    graph::Runner,
    queue::{Queue, Source, Task, TaskStatus},
    repair_budget::{Kind, Phase, Request, Stop, StopKind},
    run::{BaseSync, Origin, RunState, RunStatus},
};

async fn git_ok(path: &std::path::Path, args: &[&str]) -> String {
    git::git(path, args).await.unwrap()
}

async fn stopped() -> (common::Fixture, Task, Runner, Request) {
    let mut fx = fixture(home_lock().await, Judges::Unanimous, false);
    for spec in &mut fx.config.agents {
        spec.env.insert("MOCK_CONDUCT_NOOP".into(), "1".into());
    }
    fx.config.graph.reviewers = 1;
    fx.config.graph.review_rounds = 1;
    fx.config.disk.min_free_bytes = 0;
    fx.config.disk.auto_fold = false;
    fx.config.verify.e2e = vec!["test -f README.md".into()];
    fx.config.verify.gate = vec!["test -f README.md".into()];
    git_ok(&fx.repo, &["checkout", "-b", "fix/timeout"]).await;
    std::fs::write(fx.repo.join("README.md"), "timeout = 7200\n").unwrap();
    git_ok(&fx.repo, &["commit", "-am", "Extend timeout"]).await;
    let mut task = Task::new(
        "Review timeout".into(),
        "Review the timeout extension".into(),
        fx.repo.clone(),
        Source::Human,
    );
    task.review_of = Some("fix/timeout".into());
    Queue::open().put(&mut task).unwrap();
    let takeover = magi::handover::Takeover {
        earlier: vec![],
        home: magi::run::home(),
        choice: None,
        task_instruction: Some(task.instruction.clone()),
    };
    let mut runner = Runner::review_taking_over(
        &fx.repo,
        "fix/timeout",
        fx.config.clone(),
        Some(takeover),
        Origin::queue(&task.id),
    )
    .await
    .unwrap();
    runner.execute().await.unwrap();
    assert_eq!(runner.state.status, RunStatus::Ready);
    assert!(runner.state.foreign_checkout.is_some());
    let head = git_ok(
        &runner.state.winner().unwrap().worktree,
        &["rev-parse", "HEAD"],
    )
    .await
    .trim()
    .to_owned();
    let upstream = fx.tmp.path().join("upstream");
    git_ok(
        fx.tmp.path(),
        &[
            "clone",
            fx.repo.join("../fixture-origin.git").to_str().unwrap(),
            upstream.to_str().unwrap(),
        ],
    )
    .await;
    git_ok(&upstream, &["config", "user.name", "Test"]).await;
    git_ok(&upstream, &["config", "user.email", "test@example.invalid"]).await;
    std::fs::write(upstream.join("README.md"), "timeout = 3600\n").unwrap();
    std::fs::write(
        upstream.join("upstream.txt"),
        "preserve clean upstream changes\n",
    )
    .unwrap();
    git_ok(&upstream, &["add", "-A"]).await;
    git_ok(&upstream, &["commit", "-m", "Update upstream timeout"]).await;
    git_ok(&upstream, &["push", "origin", "main"]).await;
    git_ok(&fx.repo, &["fetch", "origin"]).await;
    let tip = git_ok(&fx.repo, &["rev-parse", "origin/main"])
        .await
        .trim()
        .to_owned();
    let stop = Stop {
        kind: StopKind::Conflict,
        head: head.clone(),
        tip: tip.clone(),
        attempts: 3,
        detail: "retained branch conflicts with origin/main and did not rebase: timeout conflict"
            .into(),
        at: jiff::Timestamp::now(),
        unverified_sync: false,
    };
    runner.state.base_sync = Some(BaseSync {
        tip: tip.clone(),
        behind: 1,
        attempts: 3,
        conflict: Some(stop.detail.clone()),
        already_in: None,
    });
    runner.state.base_sync_recovery.operations.push(
        serde_json::from_value(serde_json::json!({
            "id": 0, "episode": 0, "head": head, "tip": tip, "finished": true,
        }))
        .unwrap(),
    );
    runner.state.base_sync_recovery.stop = Some(stop.clone());
    runner.state.base_sync_recovery.stops.push(stop);
    runner.state.status = RunStatus::Blocked;
    runner.state.save().unwrap();
    task.start(runner.state.id.clone());
    task.hold_machine(Some("retained textual conflict".into()));
    // Retained review selection must neither veto this grant nor select a successor.
    task.review_branch = task.review_of.clone();
    task.review_instruction = Some(task.instruction.clone());
    Queue::open().put(&mut task).unwrap();
    let request = Request {
        replace_request: None,
        evidence_digest: None,
        task: task.id.clone(),
        run: runner.state.id.clone(),
        kind: Kind::BaseConflict,
        rounds: 1,
        head,
        expect_pass: runner.state.passes.last().unwrap().index,
        request: "timeout-resolution".into(),
        reason: "Resolve the inspected conflict".into(),
        tip: Some(tip),
        instruction: Some("Choose 7200 and preserve history".into()),
    };
    (fx, task, runner, request)
}

fn grant_output(request: &Request) -> std::process::Output {
    let mut command = magi_cli(&magi::run::home());
    command.args([
        "task",
        "grant-repair",
        &request.task,
        "--run",
        &request.run,
        "--kind",
        match request.kind {
            Kind::BaseConflict => "base-conflict",
            Kind::Gate => "gate",
            Kind::Review => "review",
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
    if let Some(digest) = &request.evidence_digest {
        command.args(["--evidence-digest", digest]);
    }
    if let Some(tip) = &request.tip {
        command.args(["--tip", tip]);
    }
    if let Some(instruction) = &request.instruction {
        command.args(["--instruction", instruction]);
    }
    if let Some(replaced) = &request.replace_request {
        command.args(["--replace-request", replaced]);
    }
    command.output().unwrap()
}

async fn grant(request: &Request) -> magi::repair_budget::Receipt {
    let out = grant_output(request);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

common::e2e! {
async fn explicit_conflict_grant_resumes_review_only_run_and_requires_fresh_verification() {
    let (fx, task, runner, request) = stopped().await;
    let old_instruction = runner.state.instruction.clone();
    let old_review = serde_json::to_value(&runner.state.reviews).unwrap();
    let old_episodes = serde_json::to_value(&runner.state.base_sync_recovery.episodes).unwrap();
    let receipt = grant(&request).await;
    assert_eq!(grant(&request).await, receipt);
    let mut changed = request.clone(); changed.instruction = Some("choose another value".into());
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), changed).await.unwrap_err().to_string().contains("different arguments"));
    let config = fx.tmp.path().join("daemon.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    let opts = magi::daemon::Opts { repo: fx.repo.clone(), config: Some(config), once: true,
        poll: std::time::Duration::from_millis(20), ..Default::default() };
    tokio::time::timeout(std::time::Duration::from_secs(120), magi::daemon::serve(opts)).await.unwrap().unwrap();
    let after = RunState::load(&request.run).unwrap();
    let queued = Queue::open().get(&task.id).unwrap();
    assert_eq!(queued.runs, vec![request.run.clone()]);
    assert_eq!(queued.status, TaskStatus::Done, "{:?}", queued.last_error);
    assert_eq!(queued.instruction, task.instruction);
    assert_eq!(queued.review_branch, task.review_branch);
    assert_eq!(after.instruction, old_instruction);
    assert_eq!(serde_json::to_value(&after.reviews[..runner.state.reviews.len()]).unwrap(), old_review);
    assert_eq!(serde_json::to_value(&after.base_sync_recovery.episodes).unwrap(), old_episodes);
    assert_eq!(after.base_sync.as_ref().unwrap().attempts, 3);
    assert!(after.base_sync_recovery.stop.is_none());
    assert_eq!(after.repair_grants[0].spent, 1);
    assert!(after.repair_grants[0].succeeded);
    assert_eq!(after.repair_grants[0].phase, Phase::Complete);
    let result = after.repair_grants[0].conflict.as_ref().unwrap().result.as_ref().unwrap();
    assert_eq!(git_ok(&fx.repo, &["show", "-s", "--format=%P", result]).await.trim(), format!("{} {}", request.head, request.tip.as_ref().unwrap()));
    assert_eq!(git_ok(&fx.repo, &["rev-parse", "HEAD"]).await.trim(), result);
    assert_eq!(std::fs::read_to_string(fx.repo.join("README.md")).unwrap(), "timeout = 7200\n");
    assert!(fx.repo.join("upstream.txt").is_file());
    assert!(after.reviews.len() > runner.state.reviews.len());
    assert_eq!(after.reviews.last().unwrap().head, *result);
    assert_eq!(after.gate_head.as_ref(), Some(result));
    assert!(!after.verification_history.is_empty());
    assert_eq!(grant(&request).await, receipt);
    assert_eq!(Queue::open().get(&task.id).unwrap().status, TaskStatus::Done);
}
}

common::e2e! {
async fn conflict_retry_and_generic_release_refuse_before_editing_or_queueing() {
    let (_fx, task, _runner, _request) = stopped().await;
    let before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
    let error = magi::chat_control::retry(&Queue::open(), &task.id, None, "Choose 7200").unwrap_err();
    assert!(error.to_string().contains("--kind base-conflict"), "{error:#}");
    let error = magi::chat_control::release(&Queue::open(), &task.id).unwrap_err();
    assert!(error.to_string().contains("generic release"));
    let out = magi_cli(&magi::run::home()).args(["task", "release", &task.id]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--kind base-conflict"));
    assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), before);
}
}

common::e2e! {
async fn conflict_grant_refuses_updated_criteria_stale_targets_holds_and_liveness() {
    let (_fx, mut task, mut runner, request) = stopped().await;
    task.instruction.push_str("; implement a broader change");
    Queue::open().put(&mut task).unwrap();
    let error = magi::repair_budget::grant(&Queue::open(), &magi::run::home(), request.clone()).await.unwrap_err();
    assert!(error.to_string().contains("restore those criteria"), "{error:#}");
    task.instruction = runner.state.task_instruction.clone().unwrap();
    Queue::open().put(&mut task).unwrap();
    for (mut invalid, expected) in [(request.clone(), "base tip moved"), (request.clone(), "one attempt"), (request.clone(), "requires --instruction")] {
        match expected { "base tip moved" => invalid.tip = Some("0".repeat(40)), "one attempt" => invalid.rounds = 2, _ => invalid.instruction = None }
        let error = magi::repair_budget::grant(&Queue::open(), &magi::run::home(), invalid).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
    }
    task.hold_manual(Some("owner hold".into())); Queue::open().put(&mut task).unwrap();
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), request.clone()).await.unwrap_err().to_string().contains("owner hold"));
    task.release(); task.hold_machine(Some("retained textual conflict".into())); Queue::open().put(&mut task).unwrap();
    runner.state.driver_pid = Some(std::process::id()); runner.state.driver_started_at = magi::proc::process_started_at(std::process::id());
    runner.state.driver_exited = false; runner.state.driver_claimed = Some(true);
    // A mismatched durable identity must not masquerade as a free current-pass lease.
    std::fs::remove_file(runner.state.dir().join("driver.json")).unwrap();
    runner.state.driver_started_at = magi::proc::process_started_at(std::process::id());
    runner.state.save().unwrap();
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), request).await.unwrap_err().to_string().contains("live or unknown"));
    assert!(Queue::open().get(&task.id).unwrap().repair_grants.is_empty());
}
}

common::e2e! {
async fn conflict_reservation_never_redispatches_unfinished_or_out_of_scope_repairs() {
    let (_fx, task, mut runner, request) = stopped().await;
    for spec in &mut runner.state.config.agents { spec.env.insert("MOCK_BASE_CONFLICT_STRAY".into(), "1".into()); }
    runner.state.save().unwrap();
    grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    let error = resumed.execute().await.unwrap_err();
    assert!(error.to_string().contains("unapproved path"), "{error:#}");
    let first = RunState::load(&request.run).unwrap();
    assert_eq!(first.repair_grants[0].spent, 1);
    assert_eq!(first.repair_grants[0].phase, Phase::Fixing);
    assert_eq!(first.reviews.len(), runner.state.reviews.len());
    assert_eq!(git_ok(&first.winner().unwrap().worktree, &["rev-parse", "HEAD"]).await.trim(), request.head);
    let mut again = Runner::resume(&request.run).unwrap();
    assert!(again.execute().await.unwrap_err().to_string().contains("unapproved path"));
    let calls = std::fs::read_to_string(first.dir().join("artifacts/base-conflict.log")).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert_eq!(Queue::open().get(&task.id).unwrap().runs.len(), 1);
}
}

common::e2e! {
async fn conflict_execution_rechecks_tip_checkout_owner_and_head_before_consumption() {
    let (fx, mut task, runner, request) = stopped().await;
    grant(&request).await;
    let original_tip = request.tip.as_ref().unwrap();
    git_ok(&fx.repo, &["update-ref", "refs/remotes/origin/main", &request.head]).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("base tip moved"));
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    git_ok(&fx.repo, &["update-ref", "refs/remotes/origin/main", original_tip]).await;
    std::fs::write(fx.repo.join("README.md"), "owner work in progress\n").unwrap();
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("checkout is dirty"));
    std::fs::write(fx.repo.join("README.md"), "timeout = 7200\n").unwrap();
    task = Queue::open().get(&task.id).unwrap();
    task.review_branch = Some("fix/different-review".into()); Queue::open().put(&mut task).unwrap();
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("different continuation"));
    task.review_branch = Some("fix/timeout".into()); Queue::open().put(&mut task).unwrap();
    git_ok(&runner.state.winner().unwrap().worktree, &["commit", "--allow-empty", "-m", "Retained branch moved"]).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("head changed"));
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    assert_eq!(Queue::open().get(&task.id).unwrap().repair_grants.len(), 1);
}
}

common::e2e! {
async fn resolved_reserved_merge_continues_without_redispatching_the_editing_seat() {
    let (_fx, task, mut runner, request) = stopped().await;
    for spec in &mut runner.state.config.agents { spec.env.insert("MOCK_BASE_CONFLICT_STRAY".into(), "1".into()); }
    runner.state.save().unwrap(); grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("unapproved path"));
    let saved = RunState::load(&request.run).unwrap();
    let scratch = saved.worktree_root().join(format!("base-conflict-{}", request.request));
    // Reconcile the retained scope violation, then continue only this reservation.
    std::fs::remove_file(scratch.join("stray.txt")).unwrap();
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert!(resumed.state.repair_grants[0].succeeded);
    assert_eq!(resumed.state.repair_grants[0].spent, 1);
    let count = resumed.state.reviews.len();
    let calls = std::fs::read_to_string(saved.dir().join("artifacts/base-conflict.log")).unwrap();
    assert_eq!(calls.lines().count(), 1);
    let mut again = Runner::resume(&request.run).unwrap(); again.execute().await.unwrap();
    assert_eq!(again.state.reviews.len(), count);
    assert_eq!(again.state.repair_grants[0].spent, 1);
    assert_eq!(Queue::open().get(&task.id).unwrap().runs.len(), 1);
}
}

common::e2e! {
async fn validated_conflict_result_finishes_compare_update_interruption_without_a_new_seat() {
    let (fx, _task, mut runner, request) = stopped().await;
    for spec in &mut runner.state.config.agents { spec.env.insert("MOCK_BASE_CONFLICT_STRAY".into(), "1".into()); }
    runner.state.save().unwrap(); grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("unapproved path"));
    let mut saved = RunState::load(&request.run).unwrap();
    let scratch = saved.worktree_root().join(format!("base-conflict-{}", request.request));
    std::fs::remove_file(scratch.join("stray.txt")).unwrap();
    git_ok(&scratch, &["add", "README.md"]).await;
    let tree = git_ok(&scratch, &["write-tree"]).await.trim().to_owned();
    let result = git_ok(&scratch, &["commit-tree", &tree, "-p", &request.head, "-p", request.tip.as_deref().unwrap(), "-m", "Approved merge result"]).await.trim().to_owned();
    let operation = saved.repair_grants[0].conflict.as_mut().unwrap();
    operation.tree = Some(tree); operation.result = Some(result.clone()); saved.save().unwrap();
    let winner = saved.winner().unwrap();
    git_ok(&winner.worktree, &["update-ref", &format!("refs/heads/{}", winner.branch), &result, &request.head]).await;
    assert!(!git::is_clean(&winner.worktree).await.unwrap());
    let mut again = Runner::resume(&request.run).unwrap(); again.execute().await.unwrap();
    assert!(again.state.repair_grants[0].succeeded);
    assert_eq!(again.state.repair_grants[0].conflict.as_ref().unwrap().result.as_ref(), Some(&result));
    assert_eq!(git_ok(&fx.repo, &["rev-parse", "HEAD"]).await.trim(), result);
    assert_eq!(std::fs::read_to_string(saved.dir().join("artifacts/base-conflict.log")).unwrap().lines().count(), 1);
}
}

common::e2e! {
async fn interrupted_conflict_without_checkpoint_or_with_markers_never_restarts_the_seat() {
    let (_fx, _task, mut runner, request) = stopped().await;
    for spec in &mut runner.state.config.agents { spec.env.insert("MOCK_BASE_CONFLICT_NOOP".into(), "1".into()); }
    runner.state.save().unwrap(); grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.is_err());
    let mut again = Runner::resume(&request.run).unwrap();
    assert!(again.execute().await.unwrap_err().to_string().contains("markers remain"));
    let mut saved = RunState::load(&request.run).unwrap();
    assert_eq!(saved.repair_grants[0].spent, 1);
    assert_eq!(saved.reviews.len(), runner.state.reviews.len());
    saved.repair_grants[0].conflict = None; saved.save().unwrap();
    let mut again = Runner::resume(&request.run).unwrap();
    assert!(again.execute().await.unwrap_err().to_string().contains("interrupted before conflict checkpoint"));
    let stopped = RunState::load(&request.run).unwrap();
    assert_eq!(stopped.repair_grants[0].phase, Phase::Complete);
    let mut fresh = request.clone();
    fresh.request = "new-explicit-generation".into();
    fresh.expect_pass = stopped.passes.last().unwrap().index;
    grant(&fresh).await;
    assert_eq!(std::fs::read_to_string(saved.dir().join("artifacts/base-conflict.log")).unwrap().lines().count(), 1);
}
}

common::e2e! {
async fn conflict_repair_cannot_reuse_the_old_clean_review_or_grant_a_review_fixer() {
    let (fx, task, mut runner, request) = stopped().await;
    for spec in &mut runner.state.config.agents { spec.env.insert("MOCK_ALWAYS_FINDING".into(), "1".into()); }
    runner.state.save().unwrap(); grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.reviews.len(), runner.state.reviews.len() + 1);
    assert!(resumed.state.reviews[0].clean);
    let fresh = resumed.state.reviews.last().unwrap();
    assert!(!fresh.clean);
    assert!(fresh.fix.is_none(), "confirmation creates no broader editing allowance");
    assert!(!resumed.state.gate_ran, "old successful gate cannot clear the repaired head");
    assert!(resumed.state.verification_history.iter().any(|v| v.gate_head.as_ref() == Some(&request.head)));
    assert!(!resumed.state.repair_grants[0].succeeded);
    assert_eq!(resumed.state.repair_grants[0].phase, Phase::Complete);
    assert_eq!(git_ok(&fx.repo, &["rev-parse", "HEAD"]).await.trim(), request.head, "unverified result is not delivered to the owner checkout");
    let mut again = Runner::resume(&request.run).unwrap(); again.execute().await.unwrap();
    assert_eq!(again.state.status, RunStatus::Blocked);
    assert_eq!(again.state.reviews.len(), resumed.state.reviews.len());
    assert!(magi::chat_control::release(&Queue::open(), &task.id).unwrap_err().to_string().contains("cannot select a successor"));
    assert!(magi::chat_control::retry(&Queue::open(), &task.id, None, "Choose 7200").unwrap_err().to_string().contains("cannot select a successor"));
}
}

common::e2e! {
async fn conflict_grant_is_bound_to_the_owner_generation_and_unique_task_owner() {
    let (_fx, task, runner, request) = stopped().await;
    grant(&request).await;
    let mut owner = Queue::open().get(&task.id).unwrap();
    let generation = owner.resumption.clone();
    owner.release_operator("new owner decision", None); Queue::open().put(&mut owner).unwrap();
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("owner decision changed"));
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    // Restore the exact acceptance generation to independently exercise the
    // execution-time unique-owner fence without spending its reservation.
    owner.resumption = generation;
    owner.operator_decided_at = task.operator_decided_at;
    Queue::open().put(&mut owner).unwrap();
    let mut duplicate = Task::new("Conflicting owner".into(), task.instruction, task.repo, Source::Human);
    duplicate.runs.push(request.run.clone()); Queue::open().put(&mut duplicate).unwrap();
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("multiple tasks claim"));
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    assert_eq!(git_ok(&runner.state.winner().unwrap().worktree, &["rev-parse", "HEAD"]).await.trim(), request.head);
}
}

common::e2e! {
async fn explicit_followup_review_retains_selection_and_rejects_changed_scope() {
    let (fx, task, mut runner, request) = stopped().await;
    // The confirmation can fail before the ordinary review budget is spent.
    runner.state.config.graph.review_rounds = 4;
    for spec in &mut runner.state.config.agents { spec.env.insert("MOCK_FINDING".into(), "1".into()); }
    runner.state.save().unwrap(); grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert!(!resumed.state.repair_grants[0].succeeded);
    assert!(resumed.state.reviews.len() < resumed.state.config.graph.review_rounds);
    let mut followup = request.clone();
    followup.kind = Kind::Review; followup.tip = None; followup.instruction = None;
    followup.request = "explicit-review-followup".into();
    followup.head = git_ok(&resumed.state.winner().unwrap().worktree, &["rev-parse", "HEAD"]).await.trim().to_owned();
    followup.expect_pass = resumed.state.passes.last().unwrap().index;
    let queue = Queue::open();
    let original = queue.get(&task.id).unwrap();
    let mut changed = original.clone(); changed.review_branch = Some("fix/another-selection".into()); queue.put(&mut changed).unwrap();
    assert!(magi::repair_budget::grant(&queue, &magi::run::home(), followup.clone()).await.unwrap_err().to_string().contains("different continuation"));
    changed = original.clone(); changed.instruction.push_str(" and implement unrelated changes"); queue.put(&mut changed).unwrap();
    assert!(magi::repair_budget::grant(&queue, &magi::run::home(), followup.clone()).await.unwrap_err().to_string().contains("retained review criteria"));
    let mut restored = original; queue.put(&mut restored).unwrap();
    std::fs::write(fx.repo.join("README.md"), "owner changes in progress\n").unwrap();
    assert!(magi::repair_budget::grant(&queue, &magi::run::home(), followup.clone()).await.unwrap_err().to_string().contains("checkout is dirty"));
    std::fs::write(fx.repo.join("README.md"), "timeout = 7200\n").unwrap();
    let receipt = grant(&followup).await;
    assert_eq!(grant(&followup).await, receipt);
    let config = fx.tmp.path().join("followup.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    let opts = magi::daemon::Opts { repo: fx.repo.clone(), config: Some(config), once: true,
        poll: std::time::Duration::from_millis(20), ..Default::default() };
    tokio::time::timeout(std::time::Duration::from_secs(120), magi::daemon::serve(opts)).await.unwrap().unwrap();
    let after = RunState::load(&request.run).unwrap();
    let owner = queue.get(&task.id).unwrap();
    assert_eq!(owner.status, TaskStatus::Done, "{:?}", owner.last_error);
    assert_eq!(owner.runs, vec![request.run.clone()]);
    assert_eq!(owner.review_branch, task.review_branch);
    assert_eq!(owner.instruction, task.instruction);
    assert!(after.repair_grants[1].succeeded);
    assert_eq!(after.repair_grants[1].spent, 1);
    assert!(after.reviews.last().unwrap().clean);
    assert!(fx.repo.join("fixed.txt").exists(), "verified follow-up reaches the original checkout");
}
}

common::e2e! {
async fn explicit_followup_gate_repairs_a_confirmation_failure_with_unused_budget() {
    let (fx, task, mut runner, request) = stopped().await;
    runner.state.config.graph.gate_fix_rounds = 2;
    runner.state.save().unwrap();
    runner.state.config.verify.gate = vec!["test -f gatefix.txt || { echo 'gatefix.txt missing'; exit 1; }".into()];
    runner.state.save().unwrap(); grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert!(!resumed.state.repair_grants[0].succeeded);
    assert!(resumed.state.gate_ran);
    assert!(resumed.state.gate_fixes.is_empty());
    assert_eq!(resumed.state.repair_grants[0].phase, Phase::Complete);
    assert!(magi::chat_control::release(&Queue::open(), &task.id).is_err());
    let mut followup = request.clone();
    followup.kind = Kind::Gate; followup.tip = None; followup.instruction = None;
    followup.request = "explicit-gate-followup".into();
    followup.head = resumed.state.gate_head.clone().unwrap();
    followup.expect_pass = resumed.state.passes.last().unwrap().index;
    // A failed explicit follow-up must retain the same refusal and allow a
    // new head-bound owner grant, rather than falling back to a successor.
    let mut failed = RunState::load(&request.run).unwrap();
    for spec in &mut failed.config.agents { spec.env.insert("MOCK_GATE_FIX_NOOP".into(), "1".into()); }
    failed.save().unwrap();
    let receipt = grant(&followup).await;
    let mut no_op = Runner::resume(&request.run).unwrap(); no_op.execute().await.unwrap();
    assert!(!no_op.state.repair_grants[1].succeeded);
    assert!(magi::chat_control::release(&Queue::open(), &task.id).unwrap_err().to_string().contains("cannot select a successor"));
    assert_eq!(no_op.state.gate_fixes.len(), 1);
    assert_eq!(grant(&followup).await, receipt);
    for spec in &mut no_op.state.config.agents { spec.env.remove("MOCK_GATE_FIX_NOOP"); }
    no_op.state.save().unwrap();
    followup.request = "second-explicit-gate-followup".into();
    followup.expect_pass = no_op.state.passes.last().unwrap().index;
    let receipt = grant(&followup).await;
    let mut again = Runner::resume(&request.run).unwrap(); again.execute().await.unwrap();
    assert!(again.state.repair_grants[2].succeeded);
    assert_eq!(again.state.repair_grants[2].spent, 1);
    assert_eq!(again.state.gate_fixes.len(), 2);
    assert_eq!(again.state.config.graph.gate_fix_rounds, 2, "ordinary allowance is not reset or extended");
    assert!(again.state.reviews.len() > resumed.state.reviews.len());
    assert!(again.state.gate.iter().all(|outcome| outcome.ok()));
    assert!(fx.repo.join("gatefix.txt").is_file());
    assert_eq!(Queue::open().get(&task.id).unwrap().review_branch, task.review_branch);
    assert_eq!(grant(&followup).await, receipt);
}
}

async fn advance_base(fx: &common::Fixture, name: &str) -> String {
    let upstream = fx.tmp.path().join("upstream");
    std::fs::write(
        upstream.join(name),
        format!("upstream advancement {name}\n"),
    )
    .unwrap();
    git_ok(&upstream, &["add", name]).await;
    git_ok(
        &upstream,
        &["commit", "-m", "Advance upstream while repair is held"],
    )
    .await;
    git_ok(&upstream, &["push", "origin", "main"]).await;
    git_ok(&fx.repo, &["fetch", "origin"]).await;
    git_ok(&fx.repo, &["rev-parse", "origin/main"])
        .await
        .trim()
        .to_owned()
}

common::e2e! {
async fn moved_base_approval_can_be_explicitly_replaced_without_rewinding_or_restarting() {
    let (fx, task, runner, original) = stopped().await;
    let episodes = serde_json::to_value(&runner.state.base_sync_recovery.episodes).unwrap();
    let stop = runner.state.base_sync_recovery.stop.clone();
    let first = grant(&original).await;
    let mut stale_writer = Queue::open().get(&task.id).unwrap();
    let tip = advance_base(&fx, "later-one.txt").await;
    let mut resumed = Runner::resume(&original.run).unwrap();
    let error = resumed.execute().await.unwrap_err().to_string();
    assert!(error.contains("--replace-request timeout-resolution"), "{error}");
    assert!(error.contains(&tip));
    assert!(RunState::load(&original.run).unwrap().repair_grants.is_empty());
    let mut replacement = original.clone();
    replacement.request = "replacement-one".into(); replacement.tip = Some(tip);
    replacement.expect_pass = RunState::load(&original.run).unwrap().passes.last().unwrap().index;
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), replacement.clone()).await.unwrap_err().to_string().contains("another grant is queued"));
    replacement.replace_request = Some("wrong-request".into());
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), replacement.clone()).await.unwrap_err().to_string().contains("latest unimported"));
    replacement.replace_request = Some(original.request.clone());
    let second = grant(&replacement).await;
    assert_eq!(grant(&replacement).await, second);
    assert_eq!(second.stop, stop);
    assert_eq!(grant(&original).await, first, "replaying old approval does not requeue it");
    Queue::open().put(&mut stale_writer).unwrap();
    assert_eq!(Queue::open().get(&task.id).unwrap().repair_grants, vec![first.clone(), second.clone()]);
    // A second real upstream update needs a second explicit owner replacement.
    let tip = advance_base(&fx, "later-two.txt").await;
    let mut resumed = Runner::resume(&original.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("--replace-request replacement-one"));
    let mut latest = replacement.clone(); latest.request = "replacement-two".into();
    latest.replace_request = Some(replacement.request.clone()); latest.tip = Some(tip.clone());
    latest.expect_pass = RunState::load(&original.run).unwrap().passes.last().unwrap().index;
    let third = grant(&latest).await;
    let config = fx.tmp.path().join("replacement.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    let opts = magi::daemon::Opts { repo: fx.repo.clone(), config: Some(config), once: true,
        poll: std::time::Duration::from_millis(20), ..Default::default() };
    tokio::time::timeout(std::time::Duration::from_secs(120), magi::daemon::serve(opts)).await.unwrap().unwrap();
    let after = RunState::load(&original.run).unwrap();
    let owner = Queue::open().get(&task.id).unwrap();
    assert_eq!(owner.status, TaskStatus::Done, "{:?}", owner.last_error);
    assert_eq!(owner.runs, vec![original.run.clone()]);
    assert_eq!(owner.instruction, task.instruction);
    assert_eq!(owner.repair_grants, vec![first.clone(), second, third.clone()]);
    assert_eq!(after.repair_grants.len(), 1, "superseded approvals never dispatch");
    assert_eq!(after.repair_grants[0].receipt, third);
    assert_eq!(after.repair_grants[0].spent, 1);
    assert!(after.repair_grants[0].succeeded);
    assert_eq!(std::fs::read_to_string(after.dir().join("artifacts/base-conflict.log")).unwrap().lines().count(), 1);
    assert_eq!(after.base_sync.as_ref().unwrap().attempts, 3);
    assert_eq!(after.base_sync.as_ref().unwrap().tip, tip);
    assert_eq!(serde_json::to_value(&after.base_sync_recovery.episodes).unwrap(), episodes);
    assert!(after.base_sync_recovery.stops.contains(stop.as_ref().unwrap()));
    let result = after.repair_grants[0].conflict.as_ref().unwrap().result.as_ref().unwrap();
    assert_eq!(git_ok(&fx.repo, &["show", "-s", "--format=%P", result]).await.trim(), format!("{} {tip}", original.head));
    assert!(git::is_ancestor(&fx.repo, original.tip.as_ref().unwrap(), result).await);
    assert_eq!(git_ok(&fx.repo, &["rev-parse", "origin/main"]).await.trim(), tip);
    assert!(fx.repo.join("later-one.txt").exists()); assert!(fx.repo.join("later-two.txt").exists());
    assert_eq!(after.gate_head.as_ref(), Some(result));
    assert!(after.reviews.len() > runner.state.reviews.len());
    assert_eq!(grant(&original).await, first);
    assert_eq!(grant(&latest).await, third);
    assert_eq!(Queue::open().get(&task.id).unwrap().status, TaskStatus::Done);
}
}

common::e2e! {
async fn first_conflict_approval_can_pin_a_new_descendant_of_the_recorded_stop() {
    let (fx, _task, runner, mut request) = stopped().await;
    let old_stop = runner.state.base_sync_recovery.stop.clone();
    request.tip = Some(advance_base(&fx, "before-approval.txt").await);
    let receipt = grant(&request).await;
    assert_eq!(receipt.stop, old_stop, "original stop identity remains immutable");
    let mut resumed = Runner::resume(&request.run).unwrap(); resumed.execute().await.unwrap();
    assert!(resumed.state.repair_grants[0].succeeded);
    assert!(fx.repo.join("before-approval.txt").exists());
    assert_eq!(resumed.state.base_sync.as_ref().unwrap().attempts, 3);
}
}

common::e2e! {
async fn replacement_refuses_imported_work_and_does_not_revoke_its_reservation() {
    let (fx, _task, mut runner, request) = stopped().await;
    for spec in &mut runner.state.config.agents { spec.env.insert("MOCK_BASE_CONFLICT_STRAY".into(), "1".into()); }
    runner.state.save().unwrap(); grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("unapproved path"));
    let saved = RunState::load(&request.run).unwrap();
    let tip = advance_base(&fx, "after-dispatch.txt").await;
    let mut replacement = request.clone(); replacement.request = "replacement-after-dispatch".into();
    replacement.tip = Some(tip); replacement.replace_request = Some(request.request.clone());
    replacement.expect_pass = saved.passes.last().unwrap().index;
    let before = std::fs::read(Queue::open().path_of(&request.task)).unwrap();
    let error = magi::repair_budget::grant(&Queue::open(), &magi::run::home(), replacement).await.unwrap_err();
    assert!(error.to_string().contains("imported or dispatched grants cannot be replaced"), "{error:#}");
    assert_eq!(std::fs::read(Queue::open().path_of(&request.task)).unwrap(), before);
    assert_eq!(RunState::load(&request.run).unwrap().repair_grants[0].spent, 1);
    assert_eq!(std::fs::read_to_string(saved.dir().join("artifacts/base-conflict.log")).unwrap().lines().count(), 1);
}
}

common::e2e! {
async fn replacing_an_approval_requires_owner_control_and_actual_forward_tip_movement() {
    let (fx, task, runner, request) = stopped().await;
    let first = grant(&request).await;
    let mut replacement = request.clone();
    replacement.request = "replacement-control-fences".into();
    replacement.replace_request = Some(request.request.clone());
    let before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
    let error = magi::repair_budget::grant(&Queue::open(), &magi::run::home(), replacement.clone()).await.unwrap_err();
    assert!(error.to_string().contains("approved tip has not moved"), "{error:#}");
    assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), before);
    replacement.tip = Some(advance_base(&fx, "owner-fence.txt").await);
    let mut owner = Queue::open().get(&task.id).unwrap();
    owner.hold_manual(Some("inspect new upstream before authorizing".into()));
    Queue::open().put(&mut owner).unwrap();
    let before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), replacement).await.unwrap_err().to_string().contains("owner hold"));
    assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), before);
    assert_eq!(Queue::open().get(&task.id).unwrap().repair_grants, vec![first]);
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
    assert_eq!(git_ok(&runner.state.winner().unwrap().worktree, &["rev-parse", "HEAD"]).await.trim(), request.head);
}
}

common::e2e! {
async fn replacing_an_unused_approval_never_accepts_rewritten_base_history() {
    let (fx, task, _runner, request) = stopped().await;
    let first = grant(&request).await;
    // Simulate an external tracking-ref rewrite to the divergent retained tip.
    git_ok(&fx.repo, &["update-ref", "refs/remotes/origin/main", &request.head]).await;
    let mut replacement = request.clone(); replacement.request = "rewritten-base".into();
    replacement.replace_request = Some(request.request.clone()); replacement.tip = Some(request.head.clone());
    let before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
    let error = magi::repair_budget::grant(&Queue::open(), &magi::run::home(), replacement).await.unwrap_err();
    assert!(error.to_string().contains("new base tip must descend from the old approved tip"), "{error:#}");
    assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), before);
    assert_eq!(Queue::open().get(&task.id).unwrap().repair_grants, vec![first]);
    assert!(RunState::load(&request.run).unwrap().repair_grants.is_empty());
}
}

fn legacy_stop(runner: &mut Runner, head: &str, typed: bool) {
    runner.state.config.graph.review_rounds = 3;
    runner.state.base_sync_recovery.operations.clear();
    runner.state.rebase_fixes = (0..3)
        .map(|_| magi::run::RebaseFixRecord {
            agent: "fixture".into(),
            paths: vec!["README.md".into()],
            from: Some(head.into()),
            finished: false,
            error: None,
            stop: None,
            index_outside: None,
            launcher: None,
            infra: None,
            outage: None,
        })
        .collect();
    let detail = format!(
        "{} conflicts with origin/main and did not rebase: conflict not resolved after 3 of 3 fixer round(s) (the rounds are spent); remaining conflicted path(s): README.md; git said: could not apply",
        runner.state.winner().unwrap().branch
    );
    runner.state.base_sync.as_mut().unwrap().conflict = Some(detail.clone());
    runner.state.event("land", detail.clone());
    let at = runner.state.events.last().unwrap().at;
    runner.state.base_sync_recovery.stop = if typed {
        let mut stop = runner
            .state
            .base_sync_recovery
            .stops
            .last()
            .unwrap()
            .clone();
        stop.detail = detail;
        stop.at = at;
        Some(stop)
    } else {
        None
    };
    runner.state.save().unwrap();
}

async fn inspect(request: &Request) -> serde_json::Value {
    let output = magi_cli(&magi::run::home())
        .args([
            "task",
            "inspect-base-stop",
            &request.task,
            "--run",
            &request.run,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

common::e2e! {
async fn legacy_diagnostic_only_stop_without_scratch_recovers_once_after_restart() {
    let (fx, task, mut runner, mut request) = stopped().await;
    legacy_stop(&mut runner, &request.head, false);
    let run_before = std::fs::read(runner.state.dir().join("run.json")).unwrap();
    let task_before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
    let inspected = inspect(&request).await;
    assert_eq!(inspected["classification"], "legacy-textual-conflict");
    assert_eq!(inspected["scratch_present"], false);
    assert!(inspected["next_step"].as_str().unwrap().contains("--evidence-digest"));
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), request.clone()).await.unwrap_err().to_string().contains("--evidence-digest"));
    assert_eq!(std::fs::read(runner.state.dir().join("run.json")).unwrap(), run_before);
    assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), task_before);
    request.evidence_digest = Some(inspected["legacy"]["digest"].as_str().unwrap().into());
    let receipt = grant(&request).await;
    assert!(receipt.legacy.is_some());
    assert_eq!(grant(&request).await, receipt);
    assert_eq!(std::fs::read(runner.state.dir().join("run.json")).unwrap(), run_before, "grant must not fabricate a modern checkpoint");
    let config = fx.tmp.path().join("daemon.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    let opts = magi::daemon::Opts {
        repo: fx.repo.clone(), config: Some(config), once: true,
        poll: std::time::Duration::from_millis(20), ..Default::default()
    };
    tokio::time::timeout(std::time::Duration::from_secs(120), magi::daemon::serve(opts))
        .await.unwrap().unwrap();
    let after = RunState::load(&request.run).unwrap();
    assert!(after.repair_grants[0].succeeded);
    assert_eq!(after.repair_grants[0].spent, 1);
    assert_eq!(after.rebase_fixes.len(), 3);
    assert!(after.rebase_fixes.iter().all(|r| r.stop.is_none() && !r.finished));
    assert_eq!(after.base_sync.as_ref().unwrap().attempts, 3);
    let result = after.repair_grants[0].conflict.as_ref().unwrap().result.as_ref().unwrap();
    assert_eq!(git_ok(&after.repo, &["show", "-s", "--format=%P", result]).await.trim(), format!("{} {}", request.head, request.tip.as_ref().unwrap()));
    assert!(after.reviews.len() > runner.state.reviews.len());
    assert_eq!(after.gate_head.as_ref(), Some(result));
    assert_eq!(grant(&request).await, receipt);
    assert_eq!(std::fs::read_to_string(after.dir().join("artifacts/base-conflict.log")).unwrap().lines().count(), 1);
    assert!(after.base_sync_recovery.stops.iter().any(|s| Some(s) == receipt.stop.as_ref()));
    assert_eq!(Queue::open().get(&task.id).unwrap().runs, vec![request.run.clone()]);
    let mut again = Runner::resume(&request.run).unwrap();
    again.execute().await.unwrap();
    assert_eq!(again.state.repair_grants[0].spent, 1);

}
}

common::e2e! {
async fn legacy_typed_stop_revalidates_provenance_and_refusals_do_not_mutate_records() {
    let (_fx, task, mut runner, mut request) = stopped().await;
    legacy_stop(&mut runner, &request.head, true);
    let inspected = inspect(&request).await;
    assert_eq!(inspected["classification"], "legacy-textual-conflict");
    request.evidence_digest = Some(inspected["legacy"]["digest"].as_str().unwrap().into());
    let original = runner.state.clone();
    for case in 0..6 {
        runner.state = original.clone();
        match case {
            0 => runner.state.rebase_fixes[1].from = Some("0".repeat(40)),
            1 => runner.state.rebase_fixes[0].finished = true,
            2 => runner.state.rebase_fixes[0].paths.push("other.txt".into()),
            3 => { runner.state.events.retain(|e| e.node != "land"); },
            4 => { runner.state.events.push(runner.state.events.last().unwrap().clone()); },
            _ => runner.state.base_sync.as_mut().unwrap().tip = "0".repeat(40),
        }
        runner.state.save().unwrap();
        let before = std::fs::read(runner.state.dir().join("run.json")).unwrap();
        let queue_before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
        let refused = grant_output(&request);
        assert!(!refused.status.success(), "ambiguous legacy evidence must refuse");
        assert_eq!(inspect(&request).await["classification"], "insufficient-evidence");
        assert_eq!(std::fs::read(runner.state.dir().join("run.json")).unwrap(), before);
        assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), queue_before);
    }
    runner.state = original;
    runner.state.rebase_fixes[0].agent.push_str("-changed");
    runner.state.save().unwrap();
    assert!(magi::repair_budget::grant(&Queue::open(), &magi::run::home(), request).await.unwrap_err().to_string().contains("stale --evidence-digest"));
}
}

common::e2e! {
async fn missing_structured_evidence_is_not_a_modern_stop_or_movement_exhaustion() {
    let (_fx, _task, mut runner, request) = stopped().await;
    assert_eq!(inspect(&request).await["classification"], "modern-textual-conflict");
    runner.state.base_sync_recovery.operations.clear();
    runner.state.save().unwrap();
    let inspected = inspect(&request).await;
    assert_eq!(inspected["classification"], "insufficient-evidence");
    assert!(inspected["missing_evidence"].as_str().unwrap().contains("empty repair list"));
}
}

common::e2e! {
async fn legacy_reserved_attempt_never_redispatches_after_restart_or_changed_evidence() {
    let (_fx, _task, mut runner, mut request) = stopped().await;
    legacy_stop(&mut runner, &request.head, false);
    for spec in &mut runner.state.config.agents {
        spec.env.insert("MOCK_BASE_CONFLICT_STRAY".into(), "1".into());
    }
    runner.state.save().unwrap();
    request.evidence_digest = Some(inspect(&request).await["legacy"]["digest"].as_str().unwrap().into());
    grant(&request).await;
    let mut resumed = Runner::resume(&request.run).unwrap();
    assert!(resumed.execute().await.unwrap_err().to_string().contains("unapproved path"));
    let mut saved = RunState::load(&request.run).unwrap();
    assert_eq!(saved.repair_grants[0].spent, 1);
    saved.rebase_fixes[0].agent.push_str("-changed");
    saved.save().unwrap();
    let mut again = Runner::resume(&request.run).unwrap();
    assert!(again.execute().await.unwrap_err().to_string().contains("evidence digest changed"));
    assert_eq!(RunState::load(&request.run).unwrap().repair_grants[0].spent, 1);
    assert_eq!(std::fs::read_to_string(saved.dir().join("artifacts/base-conflict.log")).unwrap().lines().count(), 1);
}
}

#[cfg(unix)]
common::e2e! {
async fn legacy_scratch_descendant_and_owner_hold_refuse_without_losing_dirty_contents() {
    use std::os::fd::AsRawFd;
    let (_fx, mut task, mut runner, mut request) = stopped().await;
    legacy_stop(&mut runner, &request.head, false);
    request.evidence_digest = Some(inspect(&request).await["legacy"]["digest"].as_str().unwrap().into());
    let scratch = runner.state.dir().join("base-sync");
    git_ok(&runner.state.repo, &["worktree", "add", "--detach", scratch.to_str().unwrap(), &request.head]).await;
    std::fs::write(scratch.join("README.md"), "resolved contents retained for inspection\n").unwrap();
    let dir = git_ok(&scratch, &["rev-parse", "--absolute-git-dir"]).await;
    let lease = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(std::path::Path::new(dir.trim()).join("magi-output.lock")).unwrap();
    // SAFETY: this descriptor stays alive until the lease is explicitly dropped.
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) }, 0);
    let run_before = std::fs::read(runner.state.dir().join("run.json")).unwrap();
    let queue_before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
    let refs_before = git_ok(&runner.state.repo, &["show-ref"]).await;
    let refused = grant_output(&request);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("descendant still owns"));
    assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), queue_before);
    drop(lease);
    task.hold_manual(Some("owner hold".into()));
    Queue::open().put(&mut task).unwrap();
    let held_before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
    let refused = grant_output(&request);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("owner hold"));
    assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), held_before);
    assert_eq!(std::fs::read(runner.state.dir().join("run.json")).unwrap(), run_before);
    assert_eq!(git_ok(&runner.state.repo, &["show-ref"]).await, refs_before);
    assert_eq!(std::fs::read_to_string(scratch.join("README.md")).unwrap(), "resolved contents retained for inspection\n");
}
}

common::e2e! {
async fn legacy_movement_diagnostics_use_recover_base_without_mutation() {
    let (_fx, task, mut runner, request) = stopped().await;
    legacy_stop(&mut runner, &request.head, false);
    let branch = runner.state.winner().unwrap().branch.clone();
    for (noun, verb) in [("rebase(s)", "rebasing"), ("synchronization(s)", "synchronizing")] {
        let sync = runner.state.base_sync.as_mut().unwrap();
        sync.attempts = 4;
        let detail = format!("main moved {} commit(s) ahead of {branch} after {} {noun}; {verb} again would only race it", sync.behind, sync.attempts);
        sync.conflict = Some(detail.clone());
        runner.state.save().unwrap();
        let run_path = runner.state.dir().join("run.json");
        let before = std::fs::read(&run_path).unwrap();
        let owner_before = std::fs::read(Queue::open().path_of(&task.id)).unwrap();
        let assessment = magi::repair_budget::legacy::classify(&runner.state, &request.head, request.tip.as_deref().unwrap());
        assert_eq!(assessment.class, magi::repair_budget::legacy::Class::MovementExhausted);
        let report = inspect(&request).await;
        assert_eq!(report["classification"], "movement-exhaustion");
        assert!(report["next_step"].as_str().unwrap().contains("recover-base"));
        let refused = grant_output(&request);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("recover-base"));
        assert_eq!(std::fs::read(&run_path).unwrap(), before);
        assert_eq!(std::fs::read(Queue::open().path_of(&task.id)).unwrap(), owner_before);
        runner.state.base_sync.as_mut().unwrap().conflict = Some(format!("{detail}; unrecognized suffix"));
        runner.state.save().unwrap();
        assert_eq!(inspect(&request).await["classification"], "insufficient-evidence", "near matches must not authorize movement recovery");
    }
}
}
