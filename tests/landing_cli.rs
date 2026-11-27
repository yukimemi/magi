//! Regression tests for landing confirmation, bounded done-task auditing,
//! and generation-bound investigation completion.

use std::path::Path;
use std::process::{Command, Output};

use magi::ask::{ChoiceAction, Question, Questions};
use magi::config::{Config, MergeMode};
use magi::landing::{Limits, MAX_PER_PASS, State, annotation, audit_pass, current_note, verdict};
use magi::notices::Notices;
use magi::queue::{CompletionActor, CompletionContext, Queue, Source, Task, TaskStatus};
use magi::run::{Origin, RunState, RunStatus};
use magi::talk::{self, Talks};
use serde_json::Value;

fn cli(root: &Path, repo: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_magi"));
    command
        .args(args)
        .current_dir(repo)
        .env("MAGI_HOME", root.join("state"))
        .env("MAGI_CONFIG_DIR", root.join("config"))
        .env(magi::updater::NO_AUTOUPDATE_ENV, "1");
    for key in magi::worker::INVOCATION_ENV.into_iter().chain([
        "MAGI_RUN",
        "MAGI_NODE",
        "MAGI_SEAT",
        "MAGI_CHAT_CONTROL_SOCKET",
        "MAGI_CHAT_CONTROL_PIPE",
        "MAGI_CHAT_CONTROL_FD",
        "MAGI_ALLOW_WRITE",
    ]) {
        command.env_remove(key);
    }
    command.output().unwrap()
}

fn successful(out: Output) -> Vec<u8> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn show(root: &Path, repo: &Path, id: &str) -> Value {
    let stdout = successful(cli(root, repo, &["task", "show", id, "--json"]));
    serde_json::from_slice(&stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Landing Test",
            "-c",
            "user.email=landing@example.invalid",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn init_repo_with_origin(root: &Path) -> (std::path::PathBuf, String) {
    let repo = root.join("repo");
    let origin = root.join("origin.git");
    std::fs::create_dir(&repo).unwrap();
    git(root, &["init", "--bare", "-b", "main", "origin.git"]);
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["commit", "--allow-empty", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "origin", "main"]);
    git(&repo, &["update-ref", "refs/remotes/origin/main", &base]);
    (repo, base)
}

#[test]
fn investigation_requires_findings_and_invalidates_after_instruction_edit_including_revert() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (repo, _base) = init_repo_with_origin(root);

    let added = successful(cli(
        root,
        &repo,
        &[
            "task",
            "add",
            "--investigation",
            "--hold",
            "manual research",
            "--force",
            "--json",
            "--",
            "Investigate behavior",
        ],
    ));
    let task: Task = serde_json::from_slice(&added).unwrap();
    assert!(task.investigation);

    // Toggle off and back on via `magi task investigation`.
    successful(cli(
        root,
        &repo,
        &["task", "investigation", &task.id, "--clear"],
    ));
    assert_ne!(show(root, &repo, &task.id)["investigation"], true);
    successful(cli(root, &repo, &["task", "investigation", &task.id]));

    for result in [None, Some("  ")] {
        let mut args = vec!["task", "done", &task.id];
        if let Some(result) = result {
            args.extend(["--result", result]);
        }
        let out = cli(root, &repo, &args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("--result"));
        let stored = show(root, &repo, &task.id);
        assert_eq!(stored["status"], "held");
        assert!(stored["completion_result"].is_null());
    }

    successful(cli(
        root,
        &repo,
        &["task", "done", &task.id, "--result", "Cause established"],
    ));
    let saved = show(root, &repo, &task.id);
    assert_eq!(saved["status"], "done");
    assert_eq!(saved["completion_result"]["text"], "Cause established");
    assert_eq!(saved["completion_result"]["instruction_rev"], 0);
    assert!(saved["landing"].is_null());

    // Reopen to Held: title-only edit keeps the recorded findings valid.
    successful(cli(
        root,
        &repo,
        &[
            "task",
            "control",
            "reopen",
            &task.id,
            "--reason",
            "check title edit",
        ],
    ));
    let queue = Queue::at(root.join("state").join("queue"));
    queue
        .modify(&task.id, |t| {
            t.edit("New title only".into(), t.instruction.clone())
                .unwrap();
            true
        })
        .unwrap();
    successful(cli(root, &repo, &["task", "done", &task.id]));
    assert_eq!(show(root, &repo, &task.id)["status"], "done");

    // Reopen again, edit instruction A -> B -> A: stale findings cannot complete it.
    successful(cli(
        root,
        &repo,
        &[
            "task",
            "control",
            "reopen",
            &task.id,
            "--reason",
            "revise scope",
        ],
    ));
    queue
        .modify(&task.id, |t| {
            t.edit(
                "New title only".into(),
                "Investigate different scope".into(),
            )
            .unwrap();
            t.edit("New title only".into(), "Investigate behavior".into())
                .unwrap();
            true
        })
        .unwrap();
    let edited = queue.get(&task.id).unwrap();
    assert_eq!(edited.instruction_rev, 2);
    assert!(edited.completion_result.is_none());

    let refused = cli(root, &repo, &["task", "done", &task.id]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--result"));
    assert_eq!(show(root, &repo, &task.id)["status"], "held");

    // Fresh findings bind to generation 2 and complete the investigation.
    successful(cli(
        root,
        &repo,
        &[
            "task",
            "done",
            &task.id,
            "--result",
            "Fresh findings after edit",
        ],
    ));
    let finished = show(root, &repo, &task.id);
    assert_eq!(finished["status"], "done");
    assert_eq!(
        finished["completion_result"]["text"],
        "Fresh findings after edit"
    );
    assert_eq!(finished["completion_result"]["instruction_rev"], 2);
}

#[test]
fn converting_done_to_investigation_requires_current_findings() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (repo, _) = init_repo_with_origin(root);
    let queue = Queue::at(root.join("state").join("queue"));
    let mut task = Task::new(
        "research".into(),
        "research".into(),
        repo.clone(),
        Source::Human,
    );
    task.status = TaskStatus::Done;
    queue.put(&mut task).unwrap();

    successful(cli(root, &repo, &["task", "investigation", &task.id]));
    let held = queue.get(&task.id).unwrap();
    assert!(held.investigation);
    assert!(held.operator_held());
    assert!(held.current_investigation_result().is_none());
    assert!(
        !cli(root, &repo, &["task", "done", &task.id])
            .status
            .success()
    );
    successful(cli(
        root,
        &repo,
        &["task", "done", &task.id, "--result", "Observed behavior"],
    ));
    let done = queue.get(&task.id).unwrap();
    assert_eq!(done.status, TaskStatus::Done);
    assert_eq!(done.current_investigation_result(), Some("Observed behavior"));

    // Previously saved Done investigations may already lack findings.
    queue
        .modify(&task.id, |t| {
            t.completion_result = None;
            true
        })
        .unwrap();
    assert!(
        !cli(root, &repo, &["task", "done", &task.id])
            .status
            .success()
    );
    successful(cli(
        root,
        &repo,
        &["task", "done", &task.id, "--result", "Recovered findings"],
    ));
    let repaired = queue.get(&task.id).unwrap();
    assert_eq!(repaired.current_investigation_result(), Some("Recovered findings"));
    assert_eq!(repaired.completion_history.len(), done.completion_history.len());
}

#[tokio::test]
async fn regression_seventeen_shapes_empty_missing_branch_multi_retry_wrong_base_and_forge_error() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    let (repo, base) = init_repo_with_origin(root);
    let queue = Queue::at(home.join("queue"));

    // 1. Empty commit left Ready/Done -> NotLanded + aggregated notice.
    git(&repo, &["switch", "-c", "magi/empty/A"]);
    git(&repo, &["commit", "--allow-empty", "-m", "no-op"]);
    let empty_sha = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["switch", "main"]);
    let mut empty_run = RunState::new(
        repo.clone(),
        "main".into(),
        base.clone(),
        "empty change".into(),
        Config::default(),
    );
    empty_run.status = RunStatus::Ready;
    empty_run.pr_head = Some("magi/empty/A".into());
    empty_run.pr_pushed_head = Some(empty_sha);
    let mut empty_task = Task::new(
        "empty".into(),
        "empty change".into(),
        repo.clone(),
        Source::Human,
    );
    empty_task.link_run(&empty_run.id);
    empty_run.origin = Some(Origin::queue(&empty_task.id));
    empty_run.task_instruction = Some(empty_task.instruction.clone());
    empty_run.task_instruction_rev = Some(empty_task.instruction_rev);
    empty_run.save_under(&home).unwrap();
    empty_task.succeed(CompletionContext::from_run(
        CompletionActor::DaemonSettle,
        None,
        &empty_task,
        &empty_run,
    ));
    queue.put(&mut empty_task).unwrap();

    // 2. Branch disappeared (unknown/unreachable commit SHA) -> Unknown (no false NotLanded notice).
    let mut missing_run = RunState::new(
        repo.clone(),
        "main".into(),
        base.clone(),
        "deleted branch".into(),
        Config::default(),
    );
    missing_run.status = RunStatus::Ready;
    missing_run.pr_head = Some("magi/gone/A".into());
    missing_run.pr_pushed_head = Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into());
    let mut missing_task = Task::new(
        "gone".into(),
        "deleted branch".into(),
        repo.clone(),
        Source::Human,
    );
    missing_task.link_run(&missing_run.id);
    missing_run.origin = Some(Origin::queue(&missing_task.id));
    missing_run.task_instruction = Some(missing_task.instruction.clone());
    missing_run.task_instruction_rev = Some(missing_task.instruction_rev);
    missing_run.save_under(&home).unwrap();
    missing_task.succeed(CompletionContext::from_run(
        CompletionActor::DaemonSettle,
        None,
        &missing_task,
        &missing_run,
    ));
    queue.put(&mut missing_task).unwrap();

    // 3. Multiple retries still unlanded (3 runs: Blocked, Failed, Ready with empty commit / unmerged head).
    let mut retry_task = Task::new(
        "multi-retry".into(),
        "retries unlanded".into(),
        repo.clone(),
        Source::Human,
    );
    for status in [RunStatus::Blocked, RunStatus::Failed] {
        let mut r = RunState::new(
            repo.clone(),
            "main".into(),
            base.clone(),
            retry_task.instruction.clone(),
            Config::default(),
        );
        r.status = status;
        r.origin = Some(Origin::queue(&retry_task.id));
        r.task_instruction = Some(retry_task.instruction.clone());
        r.task_instruction_rev = Some(retry_task.instruction_rev);
        r.save_under(&home).unwrap();
        retry_task.link_run(&r.id);
    }
    let mut last_retry = RunState::new(
        repo.clone(),
        "main".into(),
        base.clone(),
        retry_task.instruction.clone(),
        Config::default(),
    );
    last_retry.status = RunStatus::Ready;
    last_retry.pr_head = Some("magi/empty/A".into());
    last_retry.pr_pushed_head = Some(base.clone()); // base tip only -> NotLanded
    last_retry.origin = Some(Origin::queue(&retry_task.id));
    last_retry.task_instruction = Some(retry_task.instruction.clone());
    last_retry.task_instruction_rev = Some(retry_task.instruction_rev);
    last_retry.save_under(&home).unwrap();
    retry_task.link_run(&last_retry.id);
    retry_task.succeed(CompletionContext::from_run(
        CompletionActor::DaemonSettle,
        None,
        &retry_task,
        &last_retry,
    ));
    queue.put(&mut retry_task).unwrap();

    // 4. Merged into a different base branch (`release`, not `main`) is NOT landed on `main`.
    git(&repo, &["switch", "-c", "release"]);
    git(&repo, &["switch", "-c", "magi/otherbase/A"]);
    std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
    git(&repo, &["add", "feature.txt"]);
    git(&repo, &["commit", "-m", "feature"]);
    let feature_sha = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["switch", "release"]);
    git(&repo, &["merge", "--ff-only", "magi/otherbase/A"]);
    git(&repo, &["push", "origin", "release"]);
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/release", &feature_sha],
    );
    git(&repo, &["switch", "main"]);
    let mut wrong_base_run = RunState::new(
        repo.clone(),
        "main".into(),
        base.clone(),
        "feature for main".into(),
        Config::default(),
    );
    wrong_base_run.status = RunStatus::Ready;
    wrong_base_run.pr_head = Some("magi/otherbase/A".into());
    wrong_base_run.pr_pushed_head = Some(feature_sha.clone());
    // With a local bare origin (not a forge repository), forge lookup fails -> Unknown, NEVER Landed.
    let (wb_state, wb_reason) = verdict(&wrong_base_run, Limits::default()).await;
    assert_eq!(wb_state, State::Unknown, "{wb_reason}");

    // Audit pass classifies empty + base-tip multi-retry as NotLanded, disappeared branch as Unknown.
    let summary = audit_pass(&home, Limits::default(), MAX_PER_PASS).await;
    assert_eq!(summary.audited, 3);
    assert_eq!(summary.not_landed, 2);

    let after_empty = queue.get(&empty_task.id).unwrap();
    let after_missing = queue.get(&missing_task.id).unwrap();
    let after_retry = queue.get(&retry_task.id).unwrap();
    assert_eq!(
        current_note(&after_empty).map(|n| n.state),
        Some(State::NotLanded)
    );
    assert_eq!(
        current_note(&after_retry).map(|n| n.state),
        Some(State::NotLanded)
    );
    assert_eq!(
        current_note(&after_missing).map(|n| n.state),
        Some(State::Unknown)
    );
    assert!(annotation(&after_empty).unwrap().starts_with("NOT LANDED"));
    assert!(
        annotation(&after_missing)
            .unwrap()
            .starts_with("landing unconfirmed")
    );

    // Aggregated per-repo notice names only confirmed NotLanded tasks, never Unknown.
    let notices = Notices::at(home.join("notifications")).list();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].message.contains(&empty_task.id));
    assert!(notices[0].message.contains(&retry_task.id));
    assert!(!notices[0].message.contains(&missing_task.id));
}

#[tokio::test]
async fn choice_done_and_conductor_and_dismiss_enforce_investigation_findings_without_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    let (repo, base) = init_repo_with_origin(root);
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut chat_cfg = Config::default();
    chat_cfg.agents.push(magi::config::AgentSpec {
        id: "alpha".into(),
        kind: magi::config::AgentKind::Command,
        command: vec!["true".into()],
        model: None,
        extra_args: Vec::new(),
        prompt_delivery: Default::default(),
        max_turn_secs: None,
        env: Default::default(),
    });
    let chat = talk::begin(&talks, &chat_cfg, repo.clone(), Some("alpha")).unwrap();

    // ChoiceAction::Done on an investigation with no findings must refuse and leave no decision_effects receipt.
    let mut run = RunState::new(
        repo.clone(),
        "main".into(),
        base.clone(),
        "research".into(),
        Config::default(),
    );
    run.reserve_under(&home).unwrap();
    run.status = RunStatus::Ready;
    run.driver_exited = true;
    run.save_under(&home).unwrap();
    let mut task = Task::new(
        "research".into(),
        "research".into(),
        repo.clone(),
        Source::Human,
    );
    task.investigation = true;
    task.chat = Some(chat.id.clone());
    task.link_run(&run.id);
    task.hold_manual(Some("awaiting findings".into()));
    queue.put(&mut task).unwrap();

    let mut q = Question::new(
        run.id.clone(),
        "implement".into(),
        "impl-A".into(),
        "Finish investigation?".into(),
        String::new(),
        vec!["done".into()],
    );
    q.actions.insert("done".into(), ChoiceAction::Done);
    questions.put(&mut q).unwrap();
    magi::task_chat::post(&queue, &talks, &home, &task.id, &q).unwrap();
    magi::chat_control::decide(&talks, &home, &chat.id, &q.id, "done").unwrap();
    magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
    let after = queue.get(&task.id).unwrap();
    assert_eq!(after.status, TaskStatus::Held);
    assert!(
        !after.decision_effects.contains_key(&q.id),
        "refused Done action must not write a completion effect receipt"
    );

    // prepare_close (used by conductor Recovery::Done) also refuses investigations without findings
    // and unlanded Ready change tasks.
    assert!(
        magi::task_completion::prepare_close(
            &after,
            &home,
            CompletionActor::Conductor,
            Some("done".into()),
            false,
        )
        .is_err()
    );
    let mut change_task = Task::new(
        "change".into(),
        "research".into(),
        repo.clone(),
        Source::Human,
    );
    change_task.link_run(&run.id);
    assert!(
        magi::task_completion::prepare_close(
            &change_task,
            &home,
            CompletionActor::Conductor,
            Some("done".into()),
            false,
        )
        .is_err()
    );

    // Explicitly discarded change tasks are exempt from landing audit notices.
    change_task.hold_manual(Some("no longer needed".into()));
    queue.put(&mut change_task).unwrap();
    let discarded = magi::task_completion::close(
        &queue,
        &home,
        &change_task.id,
        CompletionActor::Cli,
        Some("explicitly abandoned".into()),
        true,
    )
    .await
    .unwrap();
    assert!(discarded.completed_by_discard());
    audit_pass(&home, Limits::default(), MAX_PER_PASS).await;
    assert!(current_note(&queue.get(&change_task.id).unwrap()).is_none());

    // Change task with Ready run edited after landing cannot close without --discard.
    let mut edited_change = Task::new(
        "edited-change".into(),
        "original spec".into(),
        repo.clone(),
        Source::Human,
    );
    let mut merged_run = RunState::new(
        repo.clone(),
        "main".into(),
        base,
        "original spec".into(),
        Config::default(),
    );
    merged_run.reserve_under(&home).unwrap();
    merged_run.status = RunStatus::Merged;
    merged_run.merge = Some(magi::run::MergeOutcome {
        mode: MergeMode::Pr,
        ok: true,
        detail: "merged".into(),
        empty: false,
    });
    merged_run.driver_exited = true;
    merged_run.origin = Some(Origin::queue(&edited_change.id));
    merged_run.task_instruction = Some("original spec".into());
    merged_run.task_instruction_rev = Some(0);
    merged_run.save_under(&home).unwrap();
    edited_change.link_run(&merged_run.id);
    edited_change.hold_manual(Some("edit check".into()));
    edited_change
        .edit("edited-change".into(), "new spec".into())
        .unwrap();
    edited_change
        .edit("edited-change".into(), "original spec".into())
        .unwrap();
    queue.put(&mut edited_change).unwrap();
    let err = magi::task_completion::close(
        &queue,
        &home,
        &edited_change.id,
        CompletionActor::Cli,
        None,
        false,
    )
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("current task instruction"),
        "{err:#}"
    );
}
