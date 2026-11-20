//! Public completion and reopening contracts against an isolated on-disk queue.
use std::path::Path;

use magi::queue::{
    CompletionAction, CompletionActor, CompletionContext, Queue, Source, Task, TaskStatus,
};
use magi::run::{RunState, RunStatus};
use magi::task_completion::{close, describe, reopen};

fn task() -> Task {
    Task::new(
        "audit".into(),
        "do the work".into(),
        "repo".into(),
        Source::Human,
    )
}

fn retained(home: &Path, task: &mut Task, status: RunStatus) -> RunState {
    let mut run = RunState::new(
        "repo".into(),
        "main".into(),
        "base".into(),
        "work".into(),
        magi::config::Config::default(),
    );
    run.reserve_under(home).unwrap();
    run.status = status;
    run.save_under(home).unwrap();
    task.link_run(&run.id);
    run
}

#[tokio::test]
async fn unfinished_work_requires_discard_and_records_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::at(dir.path().join("queue"));
    let mut task = task();
    let run = retained(dir.path(), &mut task, RunStatus::Blocked);
    task.hold_manual(Some("review".into()));
    queue.put(&mut task).unwrap();
    let error = close(
        &queue,
        dir.path(),
        &task.id,
        CompletionActor::Cli,
        None,
        false,
    )
    .await
    .unwrap_err();
    let refusal = error.to_string();
    for evidence in ["--discard", &run.id, "blocked", "PR none", "branches"] {
        assert!(refusal.contains(evidence), "{refusal}");
    }
    for reason in [None, Some(String::new()), Some(" \t\n".into())] {
        let error = close(
            &queue,
            dir.path(),
            &task.id,
            CompletionActor::Cli,
            reason,
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--discard requires a non-empty reason"),
            "{error:#}"
        );
    }
    let refused = queue.get(&task.id).unwrap();
    assert_eq!(refused.status, TaskStatus::Held);
    assert!(refused.completion_history.is_empty());
    let done = close(
        &queue,
        dir.path(),
        &task.id,
        CompletionActor::Cli,
        Some("abandon".into()),
        true,
    )
    .await
    .unwrap();
    let event = done.completion_history.last().unwrap();
    assert_eq!(event.run.as_ref().unwrap().id, run.id);
    assert_eq!(event.run.as_ref().unwrap().status, Some(RunStatus::Blocked));
    assert!(event.discard);
    assert_eq!(event.actor, CompletionActor::Cli);
    assert_eq!(event.reason.as_deref(), Some("abandon"));
    assert_eq!(
        queue.get(&task.id).unwrap().completion_history,
        done.completion_history
    );
    assert_eq!(event.prior_status, TaskStatus::Held);
    assert!(describe(&done).contains(&run.id));
    assert_eq!(
        RunState::load_under(&run.id, dir.path()).unwrap().status,
        RunStatus::Blocked
    );
}

#[tokio::test]
async fn reopening_is_held_reasoned_and_preserves_all_runs() {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::at(dir.path().join("queue"));
    let mut task = task();
    retained(dir.path(), &mut task, RunStatus::Failed);
    retained(dir.path(), &mut task, RunStatus::Blocked);
    queue.put(&mut task).unwrap();
    let done = close(
        &queue,
        dir.path(),
        &task.id,
        CompletionActor::Web,
        Some("stop unfinished work".into()),
        true,
    )
    .await
    .unwrap();
    assert!(
        reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, " ")
            .await
            .is_err()
    );
    let held = reopen(
        &queue,
        dir.path(),
        &task.id,
        CompletionActor::Chat("talk".into()),
        "inspect before retry",
    )
    .await
    .unwrap();
    assert_eq!(held.status, TaskStatus::Held);
    assert_eq!(held.runs, done.runs);
    assert_eq!(held.attempts, done.attempts);
    assert_eq!(held.hold_reason.as_deref(), Some("inspect before retry"));
    assert_eq!(held.completion_history.len(), 2);
    assert_eq!(held.completion_history[1].action, CompletionAction::Reopen);
    assert!(!held.status.runnable());
}

#[tokio::test]
async fn any_successful_retained_run_prevents_reopen() {
    for status in [
        RunStatus::Ready,
        RunStatus::Merged,
        RunStatus::AlreadyInBase,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut task = task();
        retained(dir.path(), &mut task, status);
        retained(dir.path(), &mut task, RunStatus::Failed);
        task.succeed(CompletionContext::new(
            CompletionActor::DaemonSettle,
            None,
            dir.path(),
            &task,
        ));
        queue.put(&mut task).unwrap();
        let error = reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
            .await
            .unwrap_err();
        assert!(error.to_string().contains(status.as_str()), "{error:#}");
        assert_eq!(queue.get(&task.id).unwrap().status, TaskStatus::Done);
    }
}

#[tokio::test]
async fn unknown_retained_record_and_active_claim_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::at(dir.path().join("queue"));
    let mut task = task();
    task.link_run("missing");
    task.succeed(CompletionContext::new(
        CompletionActor::Cli,
        None,
        dir.path(),
        &task,
    ));
    queue.put(&mut task).unwrap();
    let claim = queue.claim(&task.id).unwrap();
    assert!(
        reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
            .await
            .is_err()
    );
    drop(claim);
    let error = reopen(&queue, dir.path(), &task.id, CompletionActor::Cli, "retry")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown"));
    assert_eq!(queue.get(&task.id).unwrap().completion_history.len(), 1);
    let stored = queue.get(&task.id).unwrap();
    let snapshot = stored.completion_history[0].run.as_ref().unwrap();
    assert_eq!(snapshot.id, "missing");
    assert_eq!(snapshot.status, None);
    assert!(snapshot.error.is_some(), "unreadable is not no run");
}

#[tokio::test]
async fn legacy_done_without_audit_reopens_without_inventing_a_closer() {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::at(dir.path().join("queue"));
    let mut task = task();
    let run = retained(dir.path(), &mut task, RunStatus::Blocked);
    task.status = TaskStatus::Done;
    task.attempts = 2;
    queue.put(&mut task).unwrap();
    let mut raw = serde_json::to_value(&task).unwrap();
    raw.as_object_mut().unwrap().remove("completion_history");
    std::fs::write(queue.path_of(&task.id), serde_json::to_vec(&raw).unwrap()).unwrap();
    let run_path = dir.path().join("runs").join(&run.id).join("run.json");
    let before = std::fs::read(&run_path).unwrap();

    let held = reopen(
        &queue,
        dir.path(),
        &task.id,
        CompletionActor::Web,
        "recover legacy close",
    )
    .await
    .unwrap();
    assert_eq!(held.status, TaskStatus::Held);
    assert_eq!(held.runs, task.runs);
    assert_eq!(held.attempts, 2);
    assert_eq!(held.completion_history.len(), 1);
    let event = &held.completion_history[0];
    assert_eq!(event.action, CompletionAction::Reopen);
    assert_eq!(event.prior_status, TaskStatus::Done);
    assert_eq!(event.actor, CompletionActor::Web);
    assert_eq!(event.reason.as_deref(), Some("recover legacy close"));
    assert_eq!(std::fs::read(&run_path).unwrap(), before);
}

#[tokio::test]
async fn explicit_discard_audits_unreadable_runs_without_losing_current_evidence() {
    for broken in ["missing", "corrupt", "schema"] {
        for break_latest in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path();
            let queue = Queue::at(home.join("queue"));
            let mut task = task();
            let old = retained(home, &mut task, RunStatus::Failed);
            let latest = retained(home, &mut task, RunStatus::Blocked);
            let unknown = if break_latest { &latest } else { &old };
            let path = home.join("runs").join(&unknown.id).join("run.json");
            match broken {
                "missing" => std::fs::remove_file(&path).unwrap(),
                "corrupt" => std::fs::write(&path, "{").unwrap(),
                "schema" => {
                    let mut raw = serde_json::to_value(unknown).unwrap();
                    raw["schema"] = serde_json::json!(0);
                    std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
                }
                _ => unreachable!(),
            }
            let before = std::fs::read(&path).ok();
            task.hold_manual(Some("Inspect unfinished work".into()));
            queue.put(&mut task).unwrap();
            assert!(
                close(&queue, home, &task.id, CompletionActor::Cli, None, false)
                    .await
                    .is_err()
            );
            assert!(queue.get(&task.id).unwrap().completion_history.is_empty());

            let done = close(
                &queue,
                home,
                &task.id,
                CompletionActor::Cli,
                Some("Discard despite missing history".into()),
                true,
            )
            .await
            .unwrap();
            let event = done.completion_history.last().unwrap();
            assert_eq!(done.status, TaskStatus::Done);
            assert_eq!(done.runs, task.runs);
            assert_eq!(event.prior_status, TaskStatus::Held);
            assert!(event.discard);
            assert!(
                event
                    .unavailable_runs
                    .iter()
                    .any(|error| error.contains(&unknown.id))
            );
            assert!(describe(&done).contains("outcome unknown"));
            let snapshot = event.run.as_ref().unwrap();
            assert_eq!(snapshot.id, latest.id);
            assert_eq!(
                snapshot.status,
                (!break_latest).then_some(RunStatus::Blocked)
            );
            assert_eq!(std::fs::read(&path).ok(), before);
            assert!(
                reopen(&queue, home, &task.id, CompletionActor::Cli, "recover")
                    .await
                    .is_err()
            );
            assert_eq!(queue.get(&task.id).unwrap().status, TaskStatus::Done);
        }
    }
}
