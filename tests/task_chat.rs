//! Task decision delivery exercises real stores without starting another seat.
#[allow(unused_macros, unused_imports)]
mod common;

use magi::{
    ask::{Answer, Question, Questions},
    queue::{Queue, Source, Task},
    talk::{self, Talks},
};

fn git(repo: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn task(repo: &std::path::Path, source: Source) -> Task {
    let mut task = Task::new(
        "Decision task".into(),
        "Work".into(),
        repo.to_owned(),
        source,
    );
    task.runs.push("run-a".into());
    task
}

fn question(run: &str) -> Question {
    Question::new(
        run.into(),
        "impl-A".into(),
        "impl-A".into(),
        "Need an owner decision".into(),
        "Context without offered choices".into(),
        vec!["old choice".into()],
    )
}

common::e2e! {
async fn existing_closed_chat_reopens_and_delivery_is_durable_once() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = task(&fx.repo, Source::Agent { run: chat.id.clone(), node: magi::queue::CHAT_NODE.into() });
    queue.put(&mut task).unwrap();
    talk::close(&mut chat, &talks).unwrap();
    let mut q = question("run-a");
    q.cwd = Some(fx.repo.to_string_lossy().into());
    questions.put(&mut q).unwrap();
    assert_eq!(q.routed_task.as_deref(), Some(task.id.as_str()));
    assert!(magi::deputy::kind_of(&q).is_none());
    assert!(matches!(magi::waiter::decide(&q, None, false, 60, jiff::Timestamp::now()), magi::waiter::Action::Idle));
    for _ in 0..2 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
    let chat = talks.get(&chat.id).unwrap();
    assert!(chat.status.open());
    assert_eq!(chat.pending_messages.len(), 1);
    assert!(!chat.pending.contains("old choice"));
    for text in [&task.id, "run-a", "State:", "PR:"] { assert!(chat.pending.contains(text)); }
    assert!(chat.pending_messages[0].display.is_some());
    assert_eq!(magi::chat_notify::talks_to_start(&talks), [chat.id.clone()]);
    assert_eq!(queue.get(&task.id).unwrap().chat, Some(chat.id.clone()));
    assert!(!questions.get(&q.id).unwrap().status.open());
    assert!(magi::chat_control::owned_task(&queue, &chat.id, &task.id).is_ok());
    assert!(magi::chat_control::owned_task(&queue, "stranger", &task.id).is_err());
    let mut later = question("run-a");
    questions.put(&mut later).unwrap();
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    assert_eq!(talks.list().len(), 1);
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 2);
}
}

common::e2e! {
async fn new_chat_is_solo_default_and_recovers_an_unrecorded_creation() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    // Config discovery uses the committed repository layer, just like production.
    std::fs::write(fx.repo.join("magi.toml"), toml::to_string(&fx.config).unwrap()).unwrap();
    git(&fx.repo, &["add", "magi.toml"]);
    git(&fx.repo, &["commit", "-m", "test: configure chatter"]);
    git(&fx.repo, &["push", "origin", "main"]);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut task = task(&fx.repo, Source::Human);
    queue.put(&mut task).unwrap();
    let q = question("run-a");
    let id = magi::task_chat::post(&queue, &talks, &home, &task.id, &q).unwrap();
    let chat = talks.get(&id).unwrap();
    assert_eq!(chat.repo, fx.repo.canonicalize().unwrap());
    assert_eq!(chat.implementers, 1);
    assert!(chat.persona.is_empty());
    assert_eq!(chat.agent, "alpha");
    // Simulate creation/draft committed but the task link not committed.
    queue.modify(&task.id, |t| { t.chat = None; true }).unwrap();
    assert_eq!(magi::task_chat::post(&queue, &talks, &home, &task.id, &q).unwrap(), id);
    assert_eq!(talks.list().len(), 1);
    assert_eq!(talks.get(&id).unwrap().pending_messages.len(), 1);
}
}

common::e2e! {
async fn merge_requires_owner_button_and_keeps_the_existing_consent_record() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = task(&fx.repo, Source::Human);
    task.origin_chat = Some(chat.id.clone());
    queue.put(&mut task).unwrap();
    let mut q = question("run-a");
    q.node = magi::land::APPROVAL_NODE.into();
    q.choices = vec!["merge".into(), "hold".into()];
    q.merge_target = Some(magi::merge_decision::Target { pr: "https://example.test/pull/1".into(), head: "reviewed-head".into(), language: "en".into() });
    questions.put(&mut q).unwrap();
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    let mut q = questions.get(&q.id).unwrap();
    assert!(q.status.open());
    assert!(magi::deputy::fixed_clock(&q));
    assert!(q.answer(Answer::Choice("merge".into())).is_err());
    assert!(magi::consult::validate_answer(&q, &talks, &chat.id, magi::queue::CHAT_NODE, "merge", Some("merge reviewed-head")).is_err());
    talk::record(&mut chat, &talks, &format!("{}: merge reviewed-head", q.id), vec![]).unwrap();
    assert!(!magi::task_chat::approval_offered(&chat, &q), "owner text cannot offer a button");
    let mut value = serde_json::to_value(&chat).unwrap();
    let mut turn = value["turns"][0].clone();
    turn["who"] = "agent".into();
    turn["sender"] = "persona".into();
    value["turns"].as_array_mut().unwrap().push(turn);
    chat = serde_json::from_value(value).unwrap();
    assert!(magi::task_chat::approval_offered(&chat, &q));
    // A delivered approval never reopens a subsequently closed conversation.
    talk::close(&mut chat, &talks).unwrap();
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    assert!(!talks.get(&chat.id).unwrap().status.open());
}
}
common::e2e! {
async fn task_ask_cli_posts_and_exits_without_a_waiter() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let mut task = task(&fx.repo, Source::Human);
    queue.put(&mut task).unwrap();
    let mut command = common::magi_cli(&home);
    command.current_dir(&fx.repo)
        .args(["ask", "--summary", "Decision needed", "--choice", "old choice"])
        .env("MAGI_RUN", "run-a").env("MAGI_NODE", "implement").env("MAGI_SEAT", "impl-A");
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), command.output()).await
        .expect("task ask must not wait for an owner").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("without waiting"));
    let questions = Questions::at(home.join("questions"));
    let filed = questions.list();
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0].routed_task.as_deref(), Some(task.id.as_str()));
    assert!(filed[0].waiter.is_none());
    assert!(filed[0].deputy.is_none());
    assert!(questions.read_lease(&filed[0].id).is_none());
}
}
common::e2e! {
async fn forwarded_triage_does_not_file_another_question_while_chat_decides() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = task(&fx.repo, Source::Human);
    task.origin_chat = Some(chat.id.clone());
    task.hold_machine(Some("Needs a decision".into()));
    queue.put(&mut task).unwrap();
    assert!(magi::chat_control::owned_task(&queue, &chat.id, &task.id).is_err(), "origin alone grants no control");
    let mut q = question(&task.id);
    q.node = magi::triage::NODE.into();
    questions.put(&mut q).unwrap();
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    let stored = queue.get(&task.id).unwrap();
    assert!(magi::chat_control::owned_task(&queue, &chat.id, &task.id).is_ok());
    assert!(magi::triage::pending_for(&questions, &stored));
    for _ in 0..2 {
        assert!(magi::triage::run_once(&queue, &questions, None, jiff::Timestamp::now()).asked.is_empty());
    }
    assert_eq!(questions.list().len(), 1);
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 1);
}
}
