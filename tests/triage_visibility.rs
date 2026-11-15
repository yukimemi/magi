mod common;

use magi::ask::{Answer, Question, Questions};
use magi::queue::{Queue, Source, Task};

fn cli(home: &std::path::Path, args: &[&str]) -> String {
    let output = common::magi_cli(home).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn answered_triage_is_history_and_done_tasks_have_no_open_annotation() {
    let home = tempfile::tempdir().unwrap();
    let queue = Queue::at(home.path().join("queue"));
    let questions = Questions::at(home.path().join("questions"));
    let mut task = Task::new(
        "held task".into(),
        "do work".into(),
        home.path().join("repo"),
        Source::Human,
    );
    task.hold_manual(Some("waiting".into()));
    queue.put(&mut task).unwrap();
    assert!(!cli(home.path(), &["task", "show", &task.id]).contains("actions   "));
    let mut question = Question::new(
        task.id.clone(),
        magi::triage::NODE.into(),
        "triage".into(),
        "review this hold".into(),
        String::new(),
        vec!["resume it".into(), "not yet".into(), "discard it".into()],
    );
    questions.put(&mut question).unwrap();
    assert!(cli(home.path(), &["task", "list"]).contains("triage question open"));
    assert!(cli(home.path(), &["answer", "--list"]).contains(question.short()));

    question.answer(Answer::Choice("not yet".into())).unwrap();
    questions.put(&mut question).unwrap();
    task.mark_triage_applied(&question.id);
    task.hold_manual(Some(format!(
        "[triage:{}] operator: not yet",
        question.short()
    )));
    queue.put(&mut task).unwrap();
    let migration = home.path().join("notifications/task-chat-migration-v1");
    std::fs::create_dir_all(&migration).unwrap();
    std::fs::write(
        migration.join("state.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": 1, "classification_revision": 1, "pending": [], "sources": [question.id]
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(questions.list_for_effects()[0].status.open());
    assert!(!cli(home.path(), &["answer", "--list"]).contains(question.short()));
    assert!(!cli(home.path(), &["task", "list"]).contains("triage question open"));
    let show = cli(home.path(), &["task", "show", &task.id]);
    assert!(!show.contains("question  "));
    assert!(show.contains("magi task release"));
    assert!(show.contains("magi task rm"));
    let second = common::magi_cli(home.path())
        .args(["answer", &question.id, "--reply", "discard it"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already answered"));

    // Even a genuinely unanswered historical question must not mark a done task.
    let mut other = Question::new(
        task.id.clone(),
        magi::triage::NODE.into(),
        "triage".into(),
        "old hold review".into(),
        String::new(),
        vec!["resume it".into(), "not yet".into()],
    );
    questions.put(&mut other).unwrap();
    task.succeed();
    queue.put(&mut task).unwrap();
    assert!(!cli(home.path(), &["task", "list", "--all"]).contains("triage question open"));
    assert!(!cli(home.path(), &["task", "show", &task.id]).contains("question  "));
}
