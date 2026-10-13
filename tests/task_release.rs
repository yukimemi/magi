use std::path::PathBuf;
use std::process::Command;

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
    let output = Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["task", "release", &task.id])
        .env("MAGI_HOME", dir.path())
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
