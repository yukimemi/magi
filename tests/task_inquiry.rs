//! Native inquiry delivery exercises restart boundaries without question envelopes.
#[allow(unused_macros, unused_imports)]
mod common;

use magi::{
    queue::{Queue, Source, Task},
    talk::{self, Talks},
    task_inquiry::{Inquiry, Outbox},
};

fn publish_config(fixture: &common::Fixture) {
    std::fs::write(
        fixture.repo.join("magi.toml"),
        toml::to_string(&fixture.config).unwrap(),
    )
    .unwrap();
    for args in [
        vec!["add", "magi.toml"],
        vec!["commit", "-m", "test: configure inquiry chat"],
        vec!["push", "origin", "main"],
    ] {
        let result = std::process::Command::new("git")
            .args(args)
            .current_dir(&fixture.repo)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

fn inquiry(task: &Task, id: &str) -> Inquiry {
    Inquiry {
        schema: 1,
        id: id.into(),
        task: task.id.clone(),
        run: Some("run-a".into()),
        repo: task.repo.clone(),
        source: "implement:impl-A".into(),
        text: "An automatic inquiry, not an owner instruction. Please discuss the required change."
            .into(),
        display: "A change needs discussion".into(),
        key: format!("inquiry:{id}"),
        evidence: None,
        created_at: jiff::Timestamp::now(),
        delivered_to: None,
    }
}

common::e2e! {
async fn native_delivery_recovers_receipt_loss_preserves_drafts_and_never_records_approval() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    publish_config(&fx);
    let home = fx.tmp.path().join("native-inquiries");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    talk::queue(&mut chat, &talks, "Unsent owner draft", vec![]).unwrap();
    let mut task = Task::new("work".into(), "work".into(), chat.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    task.runs.push("run-a".into());
    queue.put(&mut task).unwrap();
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    let record = inquiry(&task, "native-a");
    let outbox = Outbox::at(&home);
    outbox.enqueue(record.clone()).unwrap();
    assert_eq!(Outbox::at(&home).pending().unwrap(), vec![record.clone()]);
    outbox.sweep(&queue, &talks).unwrap();
    let fresh = talks.get(&chat.id).unwrap();
    assert_eq!(fresh.pending_messages.len(), 2);
    assert_eq!(fresh.pending_messages[0].body, "Unsent owner draft");
    assert_eq!(fresh.pending_messages[1].body, record.text);
    assert!(fresh.turns.is_empty(), "delivery must not manufacture owner turns");
    assert!(magi::chat_notify::talks_to_start(&talks).is_empty(), "draft cannot be submitted automatically");
    assert_eq!(before, std::fs::read(queue.path_of(&task.id)).unwrap());
    assert!(!home.join("questions").exists(), "native transport must not create legacy records");
    // Crash after the transcript accepted the key but before the receipt rename.
    std::fs::write(home.join("notifications/task-inquiries/native-a.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    Outbox::at(&home).sweep(&queue, &talks).unwrap();
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 2);
    assert_eq!(outbox.get(&record.id).unwrap().delivered_to, Some(chat.id));
    assert!(outbox.pending().unwrap().is_empty());
}
}

common::e2e! {
async fn native_delivery_creates_the_configured_solo_chat_and_reopens_it_for_new_intent() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    publish_config(&fx);
    let home = fx.tmp.path().join("native-inquiries");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut task = Task::new("work".into(), "work".into(), fx.repo.canonicalize().unwrap(), Source::Human);
    task.runs.push("run-a".into());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let first = inquiry(&task, "native-a");
    outbox.enqueue(first.clone()).unwrap();
    let id = outbox.deliver(&queue, &talks, &first.id).unwrap();
    let mut chat = talks.get(&id).unwrap();
    assert_eq!(chat.repo, task.repo);
    assert_eq!(chat.implementers, 1);
    assert!(chat.persona.is_empty());
    assert_eq!(chat.agent, "alpha");
    talk::close(&mut chat, &talks).unwrap();
    // Replaying an acknowledged delivery must not reopen a closed conversation.
    outbox.deliver(&queue, &talks, &first.id).unwrap();
    assert!(!talks.get(&id).unwrap().status.open());
    let mut next = inquiry(&task, "native-b");
    next.text = "New unresolved intent".into();
    outbox.enqueue(next.clone()).unwrap();
    Outbox::at(&home).sweep(&queue, &talks).unwrap();
    assert!(talks.get(&id).unwrap().status.open());
    assert_eq!(talks.list().len(), 1);
    assert_eq!(queue.get(&task.id).unwrap().chat, Some(id));
}
}

common::e2e! {
async fn native_delivery_refuses_wrong_task_run_and_repository_without_touching_chat() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    publish_config(&fx);
    let home = fx.tmp.path().join("native-inquiries");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut task = Task::new("work".into(), "work".into(), fx.repo.canonicalize().unwrap(), Source::Human);
    task.runs.push("run-a".into());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    for (id, repository) in [("wrong-run", false), ("wrong-repo", true)] {
        let mut record = inquiry(&task, id);
        if repository { record.repo = home.join("other-repository"); }
        else { record.run = Some("foreign-run".into()); }
        outbox.enqueue(record).unwrap();
        assert!(outbox.deliver(&queue, &talks, id).is_err());
        assert!(outbox.get(id).unwrap().delivered_to.is_none());
    }
    assert!(talks.list().is_empty());
    assert!(queue.get(&task.id).unwrap().chat.is_none());
    assert!(outbox.enqueue(inquiry(&task, "../escape")).is_err());

}
}

common::e2e! {
async fn concurrent_native_delivery_uses_one_task_chat_and_one_draft() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    publish_config(&fx);
    let home = fx.tmp.path().join("native-inquiries");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut task = Task::new("work".into(), "work".into(), fx.repo.canonicalize().unwrap(), Source::Human);
    task.runs.push("run-a".into());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    outbox.enqueue(inquiry(&task, "native-a")).unwrap();
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| outbox.deliver(&queue, &talks, "native-a"));
        let b = scope.spawn(|| outbox.deliver(&queue, &talks, "native-a"));
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });
    assert_eq!(a, b);
    assert_eq!(talks.list().len(), 1);
    assert_eq!(talks.get(&a).unwrap().pending_messages.len(), 1);
}
}

common::e2e! {
async fn corrupt_inquiry_is_reported_without_stranding_an_unrelated_delivery() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    publish_config(&fx);
    let home = fx.tmp.path().join("native-inquiries");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut task = Task::new("work".into(), "work".into(), fx.repo.canonicalize().unwrap(), Source::Human);
    task.runs.push("run-a".into());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    outbox.enqueue(inquiry(&task, "healthy")).unwrap();
    let corrupt = home.join("notifications/task-inquiries/corrupt.json");
    std::fs::write(&corrupt, "{truncated").unwrap();
    assert!(outbox.sweep(&queue, &talks).unwrap_err().to_string().contains("corrupt"));
    assert!(outbox.get("healthy").unwrap().delivered_to.is_some());
    assert_eq!(std::fs::read_to_string(corrupt).unwrap(), "{truncated");
}
}
