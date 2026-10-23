mod common;

use std::path::PathBuf;

use magi::queue::{AnsweredQuestion, Queue, Source, Task, TaskStatus};

#[test]
fn explicit_resumption_cli_release_persists_revised_instruction_and_history() {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::at(dir.path().join("queue"));
    let mut task = Task::new(
        "recover".into(),
        "Inspect remote".into(),
        PathBuf::from("."),
        Source::Human,
    );
    task.answers.push(AnsweredQuestion {
        question: "Inspect then resume?".into(),
        answer: "Inspect, then wait for instruction".into(),
    });
    task.start("run-A".into());
    task.release_operator("choice", Some("resume-question"));
    task.resume_override = Some(magi::queue::OperatorResume {
        question_id: "resume-question".into(),
        at: jiff::Timestamp::now(),
        conductor_rehold: None,
        forced: true,
        pinned_run: Some("run-A".into()),
    });
    task.hold_machine(Some("waiting for inspection".into()));
    task.edit(
        "recover".into(),
        "Reuse approved PR head, fix Clippy, review".into(),
    )
    .unwrap();
    queue.put(&mut task).unwrap();
    let output = common::magi_cli(dir.path())
        .args(["task", "release", &task.id])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reloaded = Queue::at(dir.path().join("queue")).get(&task.id).unwrap();
    assert_eq!(reloaded.status, TaskStatus::Queued);
    assert!(reloaded.resume_override.is_none());
    assert_eq!(reloaded.answers, task.answers);
    let approval = reloaded.resumption.unwrap();
    assert_eq!(approval.source, "cli");
    assert_eq!(approval.instruction, task.instruction);
    assert_eq!(approval.answers_through, 1);
    assert_eq!(
        approval.cleared_reason.as_deref(),
        Some("waiting for inspection")
    );
}

#[test]
fn release_retains_unreadable_work_and_the_approved_intent() {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::at(dir.path().join("queue"));
    let mut task = Task::new(
        "recover".into(),
        "repair existing work".into(),
        dir.path().into(),
        Source::Human,
    );
    task.start("missing-execution".into());
    task.hold_machine(Some("review stopped".into()));
    queue.put(&mut task).unwrap();
    let output = common::magi_cli(dir.path())
        .args(["task", "release", &task.id])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let back = queue.get(&task.id).unwrap();
    assert_eq!(back.status, TaskStatus::Held);
    assert_eq!(back.runs, task.runs);
    assert_eq!(back.resumption.as_ref().unwrap().source, "cli");
    assert!(
        back.hold_reason
            .as_deref()
            .unwrap()
            .contains("cannot be read")
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("No new competition"));
}

#[test]
fn release_distinguishes_exhausted_review_from_completed_review_with_failed_gate() {
    for blocking in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut config = magi::config::Config::default();
        config.graph.review_rounds = 1;
        let mut run = magi::run::RunState::new(
            dir.path().into(),
            "main".into(),
            "base".into(),
            "repair existing work".into(),
            config,
        );
        run.status = magi::run::RunStatus::Blocked;
        run.driver_exited = true;
        run.gate_ran = true;
        run.candidates.push(
            serde_json::from_value(serde_json::json!({
                "index": 0, "label": "A", "stat": "", "files": 1, "commits": 1,
                "agent": "fixture", "branch": "retained-work", "worktree": dir.path(),
                "summary": "", "empty": false
            }))
            .unwrap(),
        );
        run.tally = Some(serde_json::from_value(serde_json::json!({
            "first_choice": {}, "borda": {}, "winner": "A", "unanimous_initial": true,
            "deliberated": false, "changed_votes": 0, "unanimous_final": true, "met_quorum": true
        })).unwrap());
        run.reviews.push(
            serde_json::from_value(serde_json::json!({
                "round": 1, "head": "head", "reviews": [], "e2e": [], "verify_retried": false,
                "blocking": blocking, "answered": 2, "expected": 2, "clean": blocking == 0
            }))
            .unwrap(),
        );
        run.save_under(dir.path()).unwrap();
        let mut task = Task::new(
            "recover".into(),
            run.instruction.clone(),
            dir.path().into(),
            Source::Human,
        );
        task.start(run.id.clone());
        task.hold_machine(Some("review stopped".into()));
        queue.put(&mut task).unwrap();
        let output = common::magi_cli(dir.path())
            .args(["task", "release", &task.id])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let back = queue.get(&task.id).unwrap();
        assert_eq!(back.runs, task.runs);
        assert_eq!(back.approved_review, blocking > 0);
        if blocking > 0 {
            assert_eq!(back.status, TaskStatus::Queued);
            assert_eq!(back.review_branch.as_deref(), Some("retained-work"));
        } else {
            assert_eq!(back.status, TaskStatus::Held);
            assert!(
                back.hold_reason
                    .as_deref()
                    .unwrap()
                    .contains("verification")
            );
        }
    }
}
