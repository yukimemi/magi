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
        occurrence: None,
        text: "An automatic inquiry, not an owner instruction. Please discuss the required change."
            .into(),
        display: "A change needs discussion".into(),
        key: format!("inquiry:{id}"),
        evidence: None,
        created_at: jiff::Timestamp::now(),
        delivered_to: None,
        decision: None,
        delivery_error: None,
        archived: false,
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

common::e2e! {
async fn pending_decisions_survive_delivery_reading_burial_restart_and_settle_only_on_effect() {
    use magi::{chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("pending-decisions");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let inquiry = outbox.file(&task, None, "implement".into(), "Owner input needed", "Explain the change").unwrap();
    outbox.deliver(&queue, &talks, &inquiry.id).unwrap();
    // The delivery has been read and pushed far out of the visible transcript.
    talk::drain(&mut chat, &talks).unwrap();
    for index in 0..30 {
        talk::record(&mut chat, &talks, &format!("Later conversation {index}"), vec![]).unwrap();
    }
    talk::close(&mut chat, &talks).unwrap();
    let restarted = Outbox::at(&home);
    assert_eq!(restarted.get(&inquiry.id).unwrap().decision.unwrap().status, Status::AwaitingOwner);
    assert_eq!(restarted.list().unwrap().len(), 1);
    assert_eq!(outbox.file(&task, None, "implement".into(), "Owner input needed", "Explain the change").unwrap().id, inquiry.id);
    // Reading a closed chat and replaying delivery neither resolve nor reopen it.
    outbox.deliver(&queue, &talks, &inquiry.id).unwrap();
    assert!(!talks.get(&chat.id).unwrap().status.open());
    chat.status = talk::TalkStatus::Open;
    talks.put(&mut chat).unwrap();
    let command = Operation::Hold { task: task.id.clone(), reason: "Owner requested a hold".into() };
    outbox.update_decision(&inquiry.id, |d| d.accept(&serde_json::to_string(&command).unwrap())).unwrap();
    assert_eq!(Outbox::at(&home).get(&inquiry.id).unwrap().decision.unwrap().status, Status::Queued,
        "accepted command has no applied effect yet");
    assert_eq!(queue.get(&task.id).unwrap().status, magi::queue::TaskStatus::Queued);
    chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
        inquiry: inquiry.id.clone(), command: Box::new(command),
    }).await.unwrap();
    assert_eq!(queue.get(&task.id).unwrap().status, magi::queue::TaskStatus::Held);
    let applied = Outbox::at(&home).get(&inquiry.id).unwrap().decision.unwrap();
    assert_eq!(applied.status, Status::Resolved);
    assert_eq!(applied.history.len(), 3);
    assert!(applied.history.last().unwrap().evidence.contains("typed command completed"));
    assert!(!home.join("questions").exists());
}
}

common::e2e! {
async fn pending_decision_failure_supersession_and_scope_are_durable() {
    use magi::{chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("decision-failure");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let first = outbox.file(&task, None, "implement".into(), "Old inquiry", "Old context").unwrap();
    // A new owner transition creates a new occurrence, independently of prose.
    task.hold_manual(Some("owner replaced the old request".into()));
    queue.put(&mut task).unwrap();
    let next = outbox.file(&task, None, "implement".into(), "New inquiry", "New context").unwrap();
    for id in [&first.id, &next.id] { outbox.deliver(&queue, &talks, id).unwrap(); }
    assert!(outbox.settle(&first.id, "foreign-chat", "superseded", Some(&next.id)).is_err());
    outbox.settle(&first.id, &chat.id, "owner replaced the old request", Some(&next.id)).unwrap();
    assert_eq!(outbox.get(&first.id).unwrap().decision.unwrap().status, Status::Resolved);
    // The operation is accepted, but its precondition fails. Neither restart
    // nor another owner reply counts as successful application.
    task.succeed();
    queue.put(&mut task).unwrap();
    let result = chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
        inquiry: next.id.clone(), command: Box::new(Operation::Release { task: task.id.clone() }),
    }).await;
    assert!(result.is_err());
    let failed = Outbox::at(&home).get(&next.id).unwrap().decision.unwrap();
    assert_eq!(failed.status, Status::RetryNeeded);
    assert!(failed.error.as_deref().unwrap().contains("completed work"));
    assert!(failed.retry_safe, "the pure precondition failed before any effect");
    outbox.settle(&next.id, &chat.id, "owner cancels the request proven not to have started", None).unwrap();
    assert_eq!(outbox.get(&next.id).unwrap().decision.unwrap().status, Status::Resolved);
    assert_eq!(queue.get(&task.id).unwrap().status, magi::queue::TaskStatus::Done);
}
}

common::e2e! {
async fn migrating_answered_unapplied_decisions_never_replays_or_approves_them() {
    use magi::{ask::{Answer, ChoiceAction, Question, Questions}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("decision-migration");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    task.runs.push("run-a".into());
    queue.put(&mut task).unwrap();
    let questions = Questions::at(home.join("questions"));
    let mut old = Question::new("run-a".into(), "implement".into(), "impl-A".into(), "Saved decision".into(), "Preserve the original intent".into(), vec!["hold".into()]);
    old.actions.insert("hold".into(), ChoiceAction::Hold);
    old.answer(Answer::Choice("hold".into())).unwrap();
    questions.put(&mut old).unwrap();
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    for _ in 0..3 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
    let outbox = Outbox::at(&home);
    assert_eq!(outbox.list().unwrap().len(), 1);
    let decision = outbox.get(&old.id).unwrap().decision.unwrap();
    assert_eq!(decision.status, Status::RetryNeeded);
    assert!(decision.request.is_none(), "migration never manufactures an owner request");
    assert_eq!(before, std::fs::read(queue.path_of(&task.id)).unwrap());
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 1);
    assert!(questions.get_for_effects(&old.id).unwrap().answer.is_none(), "operational readers must not consume saved replies");
    let raw: magi::ask::Question = serde_json::from_slice(&std::fs::read(questions.path_of(&old.id)).unwrap()).unwrap();
    assert!(raw.answer.is_some(), "the passive source history remains intact");
}
}

common::e2e! {
async fn restart_reconciles_only_confirmed_merge_receipts_and_retains_accepted_or_unknown_effects() {
    use magi::{chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("merge-projection");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let inquiry = outbox.file(&task, None, "owner".into(), "Merge requested", "Inspect receipt").unwrap();
    outbox.deliver(&queue, &talks, &inquiry.id).unwrap();
    let head = "a".repeat(40);
    let command = Operation::MergePr { repo: "owner/repo".into(), number: 42, head: head.clone(), request: "merge-42".into() };
    outbox.update_decision(&inquiry.id, |d| d.accept(&serde_json::to_string(&command).unwrap())).unwrap();
    outbox.update_decision(&inquiry.id, |d| d.start()).unwrap();
    let dir = home.join("chat-controls").join(&chat.id);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("merge-42.json");
    let action = serde_json::json!({ "repo":"owner/repo", "number":42, "head":head, "request":"merge-42" });
    for (status, expected) in [("pending", Status::InProgress), ("unknown", Status::RetryNeeded), ("merged", Status::Resolved)] {
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"action": action, "status": status, "detail": "forge observation"})).unwrap()).unwrap();
        chat_control::reconcile_decisions(&queue, &home).unwrap();
        assert_eq!(Outbox::at(&home).get(&inquiry.id).unwrap().decision.unwrap().status, expected);
    }
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap().history.len(), 4);
    assert!(magi::worker::active(&home).unwrap().is_empty(), "receipt recovery starts no effect worker");
}
}

common::e2e! {
async fn explicit_reconciliation_closes_a_crash_window_without_replaying_a_task_effect() {
    use magi::{chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("effect-crash");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let inquiry = outbox.file(&task, None, "owner".into(), "Hold the task", "An explicit owner request").unwrap();
    outbox.deliver(&queue, &talks, &inquiry.id).unwrap();
    let command = Operation::Hold { task: task.id.clone(), reason: "owner requested this exact hold".into() };
    outbox.update_decision(&inquiry.id, |d| d.accept(&serde_json::to_string(&command).unwrap())).unwrap();
    outbox.update_decision(&inquiry.id, |d| d.start()).unwrap();
    chat_control::hold(&queue, &task.id, "owner requested this exact hold").unwrap();
    // Simulated crash after the queue commit but before the outcome projection.
    chat_control::reconcile_decisions(&queue, &home).unwrap();
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap().status, Status::RetryNeeded);
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ReconcileDecision { inquiry: inquiry.id.clone() }).await.unwrap();
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap().status, Status::Resolved);
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before, "reconciliation must not execute the hold again");
}
}

common::e2e! {
async fn orphaned_migration_stays_visible_and_recovers_original_ownership_once() {
    use magi::{ask::{Question, Questions}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("orphan-migration");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut old = Question::new("run-a".into(), "implement".into(), "impl-A".into(), "Retain unresolved intent".into(), "Original task ownership needs recovery".into(), vec![]);
    old.abandon("deadline elapsed; no owner reply was recorded");
    questions.put(&mut old).unwrap();
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    let outbox = Outbox::at(&home);
    let orphan = outbox.get(&old.id).unwrap();
    assert!(orphan.task.is_empty());
    assert!(orphan.delivery_error.as_deref().unwrap().contains("Owning task"));
    assert_eq!(orphan.decision.unwrap().status, Status::AwaitingOwner);
    assert!(queue.list().is_empty());
    assert!(talks.list().is_empty());
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("original work".into(), "original work".into(), fx.repo.clone(), Source::Human);
    task.runs.push("run-a".into());
    task.chat = Some(chat.id.clone());
    queue.put(&mut task).unwrap();
    for _ in 0..3 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
    let recovered = outbox.get(&old.id).unwrap();
    assert_eq!(recovered.task, task.id);
    assert_eq!(recovered.delivered_to.as_deref(), Some(chat.id.as_str()));
    assert!(recovered.delivery_error.is_none());
    assert_eq!(recovered.decision.unwrap().status, Status::AwaitingOwner);
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 1);
    assert_eq!(queue.get(&task.id).unwrap().status, magi::queue::TaskStatus::Queued);
}
}

common::e2e! {
async fn receipt_reconciliation_isolates_missing_runs_corrupt_receipts_and_corrupt_inquiries() {
    use magi::{chat_control::{self, Operation}, task_decision::{Decision, Status}};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("isolated-receipts");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let head = "a".repeat(40);
    let missing = Operation::RerunChecks { run: "20260101-000000-dead".into(), request:"missing".into(), head:head.clone(), workflows:vec!["123".into()] };
    let merge = |request: &str| Operation::MergePr { repo:"owner/repo".into(), number:42, head:head.clone(), request:request.into() };
    for (id, op) in [("a-missing", missing), ("b-corrupt", merge("corrupt")), ("z-applied", merge("applied"))] {
        let mut record = inquiry(&task, id); record.decision = Some(Decision::awaiting(id.into()));
        outbox.enqueue(record).unwrap(); outbox.deliver(&queue, &talks, id).unwrap();
        outbox.update_decision(id, |d| d.accept(&serde_json::to_string(&op).unwrap())).unwrap();
        outbox.update_decision(id, |d| d.start()).unwrap();
    }
    let dir = home.join("chat-controls").join(&chat.id); std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("corrupt.json"), "{truncated").unwrap();
    std::fs::write(dir.join("applied.json"), serde_json::to_vec(&serde_json::json!({"action":{"repo":"owner/repo","number":42,"head":head,"request":"applied"},"status":"merged","detail":"confirmed by forge"})).unwrap()).unwrap();
    std::fs::write(home.join("notifications/task-inquiries/broken.json"), "{bad").unwrap();
    assert!(chat_control::reconcile_decisions(&queue, &home).is_err());
    assert_eq!(outbox.get("z-applied").unwrap().decision.unwrap().status, Status::Resolved);
    for id in ["a-missing", "b-corrupt"] {
        let decision = outbox.get(id).unwrap().decision.unwrap();
        assert_eq!(decision.status, Status::RetryNeeded); assert!(decision.error.unwrap().contains("reconciliation failed"));
    }
    assert_eq!(std::fs::read_to_string(dir.join("corrupt.json")).unwrap(), "{truncated");
    assert!(magi::worker::active(&home).unwrap().is_empty());
}
}

common::e2e! {
async fn imported_discussion_reply_can_be_explicitly_settled_without_creating_a_request() {
    use magi::{ask::{Answer, Question, Questions}, chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("settle-import");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(),"work".into(),fx.repo.clone(),Source::Human);
    task.chat=Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let questions = Questions::at(home.join("questions"));
    let mut q = Question::new("run-a".into(),"implement".into(),"impl-A".into(),"Saved discussion".into(),"No typed effect".into(),vec![]);
    q.answer(Answer::Text("Historical owner response".into())).unwrap(); questions.put(&mut q).unwrap();
    // Upgrade a record already imported by the previous version.
    magi::task_chat::post(&queue,&talks,&home,&task.id,&q).unwrap();
    let outbox=Outbox::at(&home); outbox.update_decision(&q.id,|d|d.fail("Saved reply has no effect receipt")).unwrap();
    let before=std::fs::read(questions.path_of(&q.id)).unwrap();
    chat_control::dispatch(&queue,&home,&talks,&chat.id,Operation::SettleDecision{inquiry:q.id.clone(),reason:"Owner explicitly confirmed no further operation is needed".into(),replacement:None}).await.unwrap();
    let decision=outbox.get(&q.id).unwrap().decision.unwrap(); assert_eq!(decision.status,Status::Resolved); assert!(decision.request.is_none());
    assert_eq!(before,std::fs::read(questions.path_of(&q.id)).unwrap());
    assert!(magi::worker::active(&home).unwrap().is_empty());
}
}

common::e2e! {
async fn completed_history_migration_is_bounded_passive_and_has_a_durable_boundary() {
    use magi::ask::{Answer, Question, Questions};
    let fx=common::fixture(common::home_lock().await,common::Judges::Unanimous,false);
    let home=fx.tmp.path().join("passive-migration"); let queue=Queue::at(home.join("queue")); let talks=Talks::at(home.join("talks"));
    let mut chat=talk::begin(&talks,&fx.config,fx.repo.clone(),Some("alpha")).unwrap(); talk::close(&mut chat,&talks).unwrap();
    let mut task=Task::new("completed".into(),"completed".into(),fx.repo.clone(),Source::Human);
    task.chat=Some(chat.id.clone()); task.runs.push("run-a".into()); task.succeed(); queue.put(&mut task).unwrap();
    let questions=Questions::at(home.join("questions"));
    for index in 0..65 {
        let mut q=Question::new("run-a".into(),"implement".into(),"impl-A".into(),format!("Historical discussion {index}"),"Saved history".into(),vec![]);
        q.answer(Answer::Text("Already discussed".into())).unwrap(); questions.put(&mut q).unwrap();
    }
    let outbox=Outbox::at(&home);
    magi::task_chat::sweep(&queue,&talks,&questions,&home);
    assert_eq!(outbox.list().unwrap().len(),64,"bounded first batch");
    magi::task_chat::sweep(&queue,&talks,&questions,&home);
    assert_eq!(outbox.list().unwrap().len(),65);
    assert!(outbox.list().unwrap().iter().all(|record| record.archived && record.decision.is_none()));
    assert!(outbox.pending().unwrap().is_empty()); outbox.sweep(&queue,&talks).unwrap();
    let fresh=talks.get(&chat.id).unwrap(); assert!(!fresh.status.open()); assert!(fresh.pending_messages.is_empty());
    assert!(magi::chat_notify::talks_to_start(&talks).is_empty());
    let mut later=Question::new("run-a".into(),"implement".into(),"impl-A".into(),"Outside migration boundary".into(),"Later history".into(),vec![]);
    later.answer(Answer::Text("Saved after migration".into())).unwrap(); questions.put(&mut later).unwrap();
    magi::task_chat::sweep(&queue,&talks,&questions,&home);
    assert_eq!(outbox.list().unwrap().len(),65,"completed migration does not expand its snapshot after restart");
    assert!(questions.get(&later.id).unwrap().answer.is_some());
}
}

common::e2e! {
async fn a_fast_completed_resume_has_an_exact_durable_start_receipt_after_restart() {
    use magi::{chat_control::{self,Operation},run::{RunState,RunStatus,Origin},task_decision::Status};
    let mut fx=common::fixture(common::home_lock().await,common::Judges::Unanimous,false);
    fx.config.merge.mode=magi::config::MergeMode::None; fx.config.graph.land=false;
    fx.config.disk.min_free_bytes=0; fx.config.disk.auto_fold=false; fx.config.disk.cache_limit_bytes=0;
    let config=fx.tmp.path().join("resume-config.toml"); std::fs::write(&config,toml::to_string(&fx.config).unwrap()).unwrap();
    let home=magi::run::home(); let queue=Queue::at(home.join("queue")); let talks=Talks::at(home.join("talks"));
    let chat=talk::begin(&talks,&fx.config,fx.repo.clone(),Some("alpha")).unwrap();
    let mut task=Task::new("Resume work".into(),"Add a note.txt file".into(),fx.repo.clone(),Source::Human); task.chat=Some(chat.id.clone());
    let head=std::process::Command::new("git").args(["rev-parse","HEAD"]).current_dir(&fx.repo).output().unwrap();
    let mut state=RunState::new(fx.repo.clone(),"main".into(),String::from_utf8(head.stdout).unwrap().trim().into(),task.instruction.clone(),fx.config.clone());
    state.reserve_under(&home).unwrap(); state.origin=Some(Origin::queue(&task.id)); state.status=RunStatus::Blocked; state.driver_exited=true; state.begin_pass(); state.save_under(&home).unwrap();
    task.runs.push(state.id.clone()); task.hold_manual(Some("Owner input required".into())); queue.put(&mut task).unwrap();
    let outbox=Outbox::at(&home); let record=outbox.file(&task,Some(state.id.clone()),"owner".into(),"Resume this execution","Explicit owner request").unwrap(); outbox.deliver(&queue,&talks,&record.id).unwrap();
    let claim = queue.claim(&task.id).unwrap();
    assert!(chat_control::dispatch(&queue,&home,&talks,&chat.id,Operation::ForDecision{inquiry:record.id.clone(),command:Box::new(Operation::Resume{run:state.id.clone()})}).await.is_err());
    let failed = Outbox::at(&home).get(&record.id).unwrap().decision.unwrap();
    assert_eq!(failed.status, Status::RetryNeeded);
    assert!(failed.retry_safe, "claim contention has not begun the resume");
    assert!(queue.get(&task.id).unwrap().resume_override.is_none());
    drop(claim);
    chat_control::reconcile_decisions(&queue, &home).unwrap();
    assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status, Status::RetryNeeded);
    chat_control::dispatch(&queue,&home,&talks,&chat.id,Operation::ForDecision{inquiry:record.id.clone(),command:Box::new(Operation::Resume{run:state.id.clone()})}).await.unwrap();
    let mut pending=queue.get(&task.id).unwrap();
    assert_eq!(pending.resume_override.as_ref().unwrap().question_id,record.id);
    pending.status=magi::queue::TaskStatus::Running; queue.put(&mut pending).unwrap();
    chat_control::reconcile_decisions(&queue,&home).unwrap();
    assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status,Status::Queued,"a task status alone is not execution-start proof");
    // Crash boundary: dispatch committed, but no driver admission or pass save.
    pending.start(state.id.clone()); queue.put(&mut pending).unwrap();
    drop(pending);
    let mut pending=Queue::at(home.join("queue")).get(&task.id).unwrap();
    assert!(pending.resume_override.is_none());
    assert_eq!(pending.dispatched_resumes.get(&record.id),Some(&state.id));
    assert!(RunState::load_under(&state.id,&home).unwrap().decision_starts.is_empty());
    chat_control::reconcile_decisions(&queue,&home).unwrap();
    assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status,Status::Queued);
    // Recovery retains the dispatched mapping; explicitly release the held task.
    pending.hold_manual(Some("Recovered before driver admission".into())); queue.put(&mut pending).unwrap();
    chat_control::dispatch(&queue,&home,&talks,&chat.id,Operation::Release{task:task.id.clone()}).await.unwrap();
    let mut pending=queue.get(&task.id).unwrap();
    let opts=magi::daemon::Opts{repo:fx.repo.clone(),config:Some(config),worktrees_root:Some(fx.tmp.path().join("resume-wt")),..Default::default()};
    let claim=queue.claim(&task.id).unwrap(); Box::pin(magi::daemon::run_claimed(&opts,&queue,&mut pending)).await; drop(claim);
    let finished=queue.get(&task.id).unwrap(); assert_ne!(finished.status,magi::queue::TaskStatus::Running);
    let disk=RunState::load_under(&state.id,&home).unwrap(); assert!(disk.decision_starts.contains_key(&record.id),"{:?}",finished.last_error);
    chat_control::reconcile_decisions(&queue,&home).unwrap();
    assert_eq!(Outbox::at(&home).get(&record.id).unwrap().decision.unwrap().status,Status::Resolved);
    chat_control::dispatch(&queue,&home,&talks,&chat.id,Operation::ReconcileDecision{inquiry:record.id.clone()}).await.unwrap();
}
}

common::e2e! {
async fn task_command_receipts_survive_later_transitions_and_lost_projection_writes() {
    use magi::{chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("task-command-receipts");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    for kind in ["hold", "release", "done", "retry"] {
        let mut task = Task::new(kind.into(), "original instruction".into(), fx.repo.clone(), Source::Human);
        task.chat = Some(chat.id.clone());
        task.hold_manual(Some("original hold".into()));
        queue.put(&mut task).unwrap();
        let outbox = Outbox::at(&home);
        let record = outbox.file(&task, None, "receipt-test".into(), "Owner decision", kind).unwrap();
        outbox.deliver(&queue, &talks, &record.id).unwrap();
        let command = match kind {
            "hold" => Operation::Hold { task: task.id.clone(), reason: "new hold".into() },
            "release" => Operation::Release { task: task.id.clone() },
            "done" => Operation::Done { task: task.id.clone() },
            "retry" => Operation::Retry { task: task.id.clone(), title: None, instruction: "revised instruction".into() },
            _ => unreachable!(),
        };
        let request = serde_json::to_string(&command).unwrap();
        chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
            inquiry: record.id.clone(), command: Box::new(command),
        }).await.unwrap();
        assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status, Status::Resolved);
        let committed = queue.get(&task.id).unwrap();
        assert_eq!(committed.decision_effects.get(&record.id), Some(&request));
        // Simulate death after the queue commit and before the final inquiry
        // projection rename. The independently committed receipt is authoritative.
        let path = home.join("notifications/task-inquiries").join(format!("{}.json", record.id));
        let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        saved["decision"]["status"] = "in_progress".into();
        saved["decision"]["history"].as_array_mut().unwrap().pop();
        std::fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
        // A subsequent transition must not erase proof of the earlier effect.
        // This writer still holds a pre-effect snapshot. Queue persistence
        // must merge the receipt even when saving its later transition.
        task.succeed(); queue.put(&mut task).unwrap();
        assert_eq!(queue.get(&task.id).unwrap().decision_effects.get(&record.id), Some(&request));
        let before = std::fs::read(queue.path_of(&task.id)).unwrap();
        chat_control::reconcile_decisions(&queue, &home).unwrap();
        let decision = Outbox::at(&home).get(&record.id).unwrap().decision.unwrap();
        assert_eq!(decision.status, Status::Resolved, "{kind}");
        assert!(decision.history.last().unwrap().evidence.contains("receipt"));
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before, "recovery never replays {kind}");
    }
}
}

common::e2e! {
async fn failed_task_transition_has_no_effect_receipt_and_stays_actionable_after_restart() {
    use magi::{chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("failed-task-receipt");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("completed work".into(), "completed work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    task.succeed(); queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let record = outbox.file(&task, None, "receipt-test".into(), "Discuss completed work", "A reply alone does not restart it").unwrap();
    outbox.deliver(&queue, &talks, &record.id).unwrap();
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
        inquiry: record.id.clone(),
        command: Box::new(Operation::Hold { task: task.id.clone(), reason: "change direction".into() }),
    }).await.is_err());
    chat_control::reconcile_decisions(&queue, &home).unwrap();
    let failed = Outbox::at(&home).get(&record.id).unwrap().decision.unwrap();
    assert_eq!(failed.status, Status::RetryNeeded);
    assert!(failed.error.unwrap().contains("completed work must not be restarted"));
    assert!(queue.get(&task.id).unwrap().decision_effects.is_empty());
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
}
}

common::e2e! {
async fn malformed_legacy_sources_remain_visible_across_restart_and_repair_once() {
    use magi::ask::{Question, Questions};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("malformed-migration");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("original work".into(), "original work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let questions = Questions::at(home.join("questions"));
    std::fs::create_dir_all(questions.root()).unwrap();
    let mut old = Question::new("run-a".into(), "implement".into(), "impl-A".into(), "Original pending intent".into(), "Preserve me".into(), vec![]);
    let path = questions.path_of(&old.id);
    std::fs::write(&path, b"{").unwrap();
    for _ in 0..2 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
    let errors = magi::task_chat::migration_issues(&home);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].id, old.id);
    assert!(errors[0].reason.contains("Import executed no action"));
    assert_eq!(std::fs::read(&path).unwrap(), b"{");
    assert!(Outbox::at(&home).list().unwrap().is_empty());
    assert!(talks.get(&chat.id).unwrap().pending_messages.is_empty());
    // Restoring the original saved source is a fixture repair, not an answer.
    questions.put(&mut old).unwrap();
    for _ in 0..2 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
    assert!(magi::task_chat::migration_issues(&home).is_empty());
    let records = Outbox::at(&home).list().unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].decision.as_ref().unwrap().request.is_none());
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 1);
}
}

common::e2e! {
async fn identical_inquiry_wording_in_a_new_owner_generation_is_not_lost_as_redelivery() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("inquiry-generations");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("recurring work".into(), "recurring work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let first = outbox.file(&task, None, "implement:impl-A".into(), "Discuss the same issue", "The reason is unchanged").unwrap();
    assert_eq!(first.id, outbox.file(&task, None, "implement:impl-A".into(), "Discuss the same issue", "The reason is unchanged").unwrap().id);
    outbox.deliver(&queue, &talks, &first.id).unwrap();
    outbox.settle(&first.id, &chat.id, "Owner settled this occurrence", None).unwrap();
    task.hold_manual(Some("owner requests a new direction".into()));
    queue.put(&mut task).unwrap();
    let second = Outbox::at(&home).file(&task, None, "implement:impl-A".into(), "Discuss the same issue", "The reason is unchanged").unwrap();
    assert_ne!(first.id, second.id);
    outbox.deliver(&queue, &talks, &second.id).unwrap();
    assert_eq!(outbox.get(&first.id).unwrap().decision.unwrap().status, magi::task_decision::Status::Resolved);
    assert_eq!(outbox.get(&second.id).unwrap().decision.unwrap().status, magi::task_decision::Status::AwaitingOwner);
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 2);
}
}

common::e2e! {
async fn retained_native_inquiry_without_occurrence_metadata_is_adopted_without_redelivery() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("retained-native-generation");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("retained work".into(), "retained work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let first = outbox.file(&task, None, "implement:impl-A".into(), "Retained inquiry", "Unresolved context").unwrap();
    outbox.deliver(&queue, &talks, &first.id).unwrap();
    let path = home.join("notifications/task-inquiries").join(format!("{}.json", first.id));
    let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved.as_object_mut().unwrap().remove("occurrence");
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    // The retained implementation did not include this owner generation in
    // its key. Restart must preserve the existing unresolved delivery instead
    // of treating upgrade alone as a second inquiry or an owner request.
    task.hold_manual(Some("original owner generation".into())); queue.put(&mut task).unwrap();
    for _ in 0..2 {
        let adopted = Outbox::at(&home).file(&task, None, "implement:impl-A".into(), "Retained inquiry", "Unresolved context").unwrap();
        assert_eq!(adopted.id, first.id);
        assert!(adopted.occurrence.is_some());
        assert!(adopted.decision.as_ref().unwrap().request.is_none());
        outbox.deliver(&queue, &talks, &adopted.id).unwrap();
    }
    assert_eq!(outbox.list().unwrap().len(), 1);
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 1);
}
}

common::e2e! {
async fn distinct_inquiries_share_a_producer_generation_without_losing_delivery_or_pending_items() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("distinct-inquiries");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let first = outbox.file(&task, Some("run-a".into()), "implement:impl-A".into(), "Storage decision", "Choose the storage").unwrap();
    let second = outbox.file(&task, Some("run-a".into()), "implement:impl-A".into(), "Interface decision", "Choose the interface").unwrap();
    assert_ne!(first.id, second.id);
    for id in [&first.id, &second.id] { outbox.deliver(&queue, &talks, id).unwrap(); }
    let replay = Outbox::at(&home).file(&task, Some("run-a".into()), "implement:impl-A".into(), "Storage decision", "Choose the storage").unwrap();
    assert_eq!(replay.id, first.id);
    outbox.deliver(&queue, &talks, &replay.id).unwrap();
    assert_eq!(talks.get(&chat.id).unwrap().pending_messages.len(), 2);
    assert_eq!(outbox.list().unwrap().len(), 2);
    assert!(outbox.list().unwrap().iter().all(|item| item.decision.as_ref().unwrap().status == magi::task_decision::Status::AwaitingOwner));
    // The same wording may also be a different occurrence. Its explicit key
    // distinguishes it; rephrasing a replay of that key preserves the first post.
    let third = outbox.file_for_occurrence(&task, Some("run-a".into()), "implement:impl-A".into(), "third-occurrence", "Storage decision", "Choose the storage").unwrap();
    assert_ne!(third.id, first.id);
    let third_replay = Outbox::at(&home).file_for_occurrence(&task, Some("run-a".into()), "implement:impl-A".into(), "third-occurrence", "Rephrased", "Rephrased context").unwrap();
    assert_eq!(third, third_replay);
}
}

common::e2e! {
async fn task_claim_contention_is_durably_retryable_and_safe_to_cancel_before_effects() {
    use magi::{chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("claim-contention");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    for cancel in [false, true] {
        let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
        task.chat = Some(chat.id.clone()); queue.put(&mut task).unwrap();
        let outbox = Outbox::at(&home);
        let inquiry = outbox.file(&task, None, "implement".into(), "Hold requested", "Owner requests a hold").unwrap();
        outbox.deliver(&queue, &talks, &inquiry.id).unwrap();
        let command = Operation::Hold { task: task.id.clone(), reason: "owner requested hold".into() };
        let bytes = serde_json::to_vec(&command).unwrap();
        let claim = queue.claim(&task.id).unwrap();
        assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision { inquiry: inquiry.id.clone(), command: Box::new(command) }).await.is_err());
        let failed = Outbox::at(&home).get(&inquiry.id).unwrap().decision.unwrap();
        assert_eq!(failed.status, Status::RetryNeeded);
        assert!(failed.retry_safe);
        assert_eq!(failed.request.as_deref(), Some(std::str::from_utf8(&bytes).unwrap()));
        let different = Operation::Hold { task: task.id.clone(), reason: "different request".into() };
        assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
            inquiry: inquiry.id.clone(), command: Box::new(different)
        }).await.is_err());
        assert_eq!(Outbox::at(&home).get(&inquiry.id).unwrap().decision.unwrap(), failed,
            "claim contention must not replace the original owner request");
        assert!(queue.get(&task.id).unwrap().decision_effects.is_empty());
        drop(claim);
        chat_control::reconcile_decisions(&queue, &home).unwrap();
        assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap().status, Status::RetryNeeded, "restart is not an owner retry");
        if cancel {
            chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::SettleDecision { inquiry: inquiry.id.clone(), reason: "Owner cancels this unstarted request".into(), replacement: None }).await.unwrap();
            assert_eq!(queue.get(&task.id).unwrap().status, magi::queue::TaskStatus::Queued);
            assert!(queue.get(&task.id).unwrap().decision_effects.is_empty());
        } else {
            let command = serde_json::from_slice(&bytes).unwrap();
            chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision { inquiry: inquiry.id.clone(), command: Box::new(command) }).await.unwrap();
            let task = queue.get(&task.id).unwrap();
            assert_eq!(task.status, magi::queue::TaskStatus::Held);
            assert_eq!(task.decision_effects.len(), 1);
        }
        assert_eq!(Outbox::at(&home).get(&inquiry.id).unwrap().decision.unwrap().status, Status::Resolved);
    }
    // A new admission failure cannot turn an older uncertain outcome into
    // proof of non-execution, even though this particular attempt never starts.
    let mut task = Task::new("uncertain work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let inquiry = outbox.file(&task, None, "implement".into(), "Uncertain hold", "Inspect the prior outcome").unwrap();
    outbox.deliver(&queue, &talks, &inquiry.id).unwrap();
    let command = Operation::Hold { task: task.id.clone(), reason: "original owner request".into() };
    let request = serde_json::to_string(&command).unwrap();
    outbox.update_decision(&inquiry.id, |d| {
        d.accept(&request)?; d.start()?; d.fail("prior effect has no confirmed outcome")
    }).unwrap();
    let before = outbox.get(&inquiry.id).unwrap().decision.unwrap();
    let claim = queue.claim(&task.id).unwrap();
    assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
        inquiry: inquiry.id.clone(), command: Box::new(command)
    }).await.is_err());
    assert_eq!(Outbox::at(&home).get(&inquiry.id).unwrap().decision.unwrap(), before);
    assert!(!before.retry_safe);
    assert!(queue.get(&task.id).unwrap().decision_effects.is_empty());
    drop(claim);
}
}

common::e2e! {
async fn answered_sources_never_read_as_open_cards_before_during_or_after_partial_migration() {
    use magi::ask::{Answer, ChoiceAction, Question, QuestionStatus, Questions};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("answered-not-open");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let questions = Questions::at(home.join("questions"));
    let mut ids = Vec::new();
    for index in 0..65 {
        let mut q = Question::new("run-a".into(), "implement".into(), "impl-A".into(), format!("Saved hold {index}"), "history".into(), vec!["hold".into()]);
        q.actions.insert("hold".into(), ChoiceAction::Hold);
        q.answer(Answer::Choice("hold".into())).unwrap(); questions.put(&mut q).unwrap(); ids.push(q.id);
    }
    // An upgraded installation has already snapshotted these saved sources,
    // while the first bounded batch may still be pending.
    let migration = home.join("notifications/task-chat-migration-v1");
    std::fs::create_dir_all(&migration).unwrap();
    std::fs::write(migration.join("state.json"), serde_json::to_vec(&serde_json::json!({
        "schema": 1, "sources": ids, "pending": ids
    })).unwrap()).unwrap();
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    for count in [0, 64, 65, 65] {
        // Reopen stores to exercise the persisted partial migration boundary.
        let questions = Questions::at(home.join("questions"));
        if count > 0 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
        assert_eq!(Outbox::at(&home).list().unwrap().len(), count);
        for id in &ids {
            let q = questions.get(id).unwrap();
            assert_eq!(q.status, QuestionStatus::Answered);
            assert_eq!(q.resolution().as_deref(), Some("hold"));
            assert!(q.clone().answer(Answer::Choice("hold".into())).is_err(), "view and answer agree");
            assert!(questions.update(id, |q| q.answer(Answer::Choice("hold".into()))).is_err(), "persisted answers cannot be replaced");
        }
        assert!(questions.list().iter().all(|q| !q.status.open()));
        assert!(questions.list_for_effects().iter().all(|q| q.chosen_action().is_none()));
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
    }
    let raw: Question = serde_json::from_slice(&std::fs::read(questions.path_of(&ids[0])).unwrap()).unwrap();
    assert_eq!(raw.status, QuestionStatus::Answered);
    assert_eq!(raw.resolution().as_deref(), Some("hold"), "the source history stays intact");
}
}

common::e2e! {
async fn scheduler_effect_consumers_never_replay_migrated_or_deferred_saved_answers() {
    use magi::{ask::{Question, Questions, Answer, ChoiceAction}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("source-consumer-migration");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let questions = Questions::at(home.join("questions"));
    let mut ids = Vec::new();
    for index in 0..65 {
        let mut q = Question::new("run-a".into(), "implement".into(), "impl-A".into(), format!("Saved hold {index}"), "Saved reply is history".into(), vec!["hold".into()]);
        q.actions.insert("hold".into(), ChoiceAction::Hold);
        q.answer(Answer::Choice("hold".into())).unwrap(); questions.put(&mut q).unwrap(); ids.push(q.id);
    }
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    let outbox = Outbox::at(&home);
    for count in [64, 65, 65] {
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
        // These are the actual effect consumers called after migration in the
        // scheduler poll, not just transport-only sweep coverage.
        magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        assert_eq!(outbox.list().unwrap().len(), count);
        assert!(questions.list_for_effects().iter().all(|q| q.chosen_action().is_none()));
        assert!(outbox.list().unwrap().iter().all(|q| q.decision.as_ref().unwrap().request.is_none()));
    }
    // Even an accepted native command cannot expose the *old* source payload
    // in the interval before the refreshed response is committed.
    let id = &ids[0];
    let raw: Question = serde_json::from_slice(&std::fs::read(questions.path_of(id)).unwrap()).unwrap();
    let different_generation = raw.answered_at.unwrap().checked_add(std::time::Duration::from_secs(1)).unwrap();
    outbox.update_decision(id, |decision| {
        decision.accept("decision: hold")?;
        decision.source_answer_at = Some(different_generation);
        Ok(true)
    }).unwrap();
    magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
    assert_eq!(outbox.get(id).unwrap().decision.unwrap().status, Status::Queued);
    // A fresh, explicit owner command can subsequently apply exactly one
    // operation, while all other saved replies remain inert after restart.
    magi::chat_control::decide(&talks, &home, &chat.id, id, "hold").unwrap();
    for _ in 0..2 {
        magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
    }
    let applied = queue.get(&task.id).unwrap();
    assert_eq!(applied.status, magi::queue::TaskStatus::Held);
    assert_eq!(applied.actions_applied, vec![id.clone()]);
    assert_eq!(outbox.get(id).unwrap().decision.unwrap().status, Status::Resolved);
}
}

common::e2e! {
async fn imported_effect_receipts_recover_from_delay_and_unreadable_receipts_without_replay() {
    use magi::{ask::{Answer, ChoiceAction, Question, Questions}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    for corrupt_receipt in [false, true] {
        let home = fx.tmp.path().join(format!("late-source-receipt-{corrupt_receipt}"));
        let queue = Queue::at(home.join("queue"));
        let talks = Talks::at(home.join("talks"));
        let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
        let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
        task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
        let questions = Questions::at(home.join("questions"));
        let mut q = Question::new("run-a".into(), "conduct".into(), "conductor".into(),
            "Saved resume request".into(), "Do not replay this historical reply".into(), vec!["resume".into()]);
        let action = ChoiceAction::ResumeRun { run: "20260101-000000-abcd".into() };
        q.actions.insert("resume".into(), action.clone());
        q.answer(Answer::Choice("resume".into())).unwrap(); questions.put(&mut q).unwrap();
        let receipt = home.join("choice-actions").join(format!("{}.json", q.id));
        std::fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        if corrupt_receipt { std::fs::write(&receipt, "{truncated").unwrap(); }
        let before_task = std::fs::read(queue.path_of(&task.id)).unwrap();
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
        let outbox = Outbox::at(&home);
        let pending = outbox.get(&q.id).unwrap().decision.unwrap();
        assert_eq!(pending.status, Status::RetryNeeded);
        assert!(pending.request.is_none());
        assert!(questions.get_for_effects(&q.id).unwrap().resolution().is_none(), "execution view must still quarantine the saved reply");
        let before_source = std::fs::read(questions.path_of(&q.id)).unwrap();
        // An already-running legacy consumer can commit this receipt later;
        // migration and subsequent reconciliation may only observe it.
        std::fs::write(&receipt, serde_json::to_vec(&serde_json::json!({"action":action,"status":"applied"})).unwrap()).unwrap();
        for _ in 0..2 {
            magi::task_chat::sweep(&Queue::at(home.join("queue")), &Talks::at(home.join("talks")), &Questions::at(home.join("questions")), &home);
            magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
            let decision = Outbox::at(&home).get(&q.id).unwrap().decision.unwrap();
            assert_eq!(decision.status, Status::Resolved);
            assert!(decision.request.is_none(), "receipt observation must not manufacture owner authority");
            assert!(questions.get_for_effects(&q.id).unwrap().resolution().is_none());
            assert_eq!(std::fs::read(questions.path_of(&q.id)).unwrap(), before_source);
            assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before_task);
            assert!(magi::worker::active(&home).unwrap().is_empty());
        }
    }
}
}

common::e2e! {
async fn retired_routing_opt_out_does_not_strand_saved_task_intent() {
    use magi::ask::{Question, Questions, ChoiceAction, Answer};
    let mut fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    fx.config.graph.question_routing = false; publish_config(&fx);
    let home = fx.tmp.path().join("retired-opt-out");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    talk::close(&mut chat, &talks).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let questions = Questions::at(home.join("questions"));
    let mut source = Question::new("run-a".into(), "implement".into(), "impl-A".into(), "Saved inquiry".into(), "Original intent".into(), vec!["hold".into()]);
    source.actions.insert("hold".into(), ChoiceAction::Hold); source.answer(Answer::Choice("hold".into())).unwrap();
    questions.put(&mut source).unwrap();
    // Snapshot the unrouted shape produced by the retired setting.
    source.routed_task = None;
    std::fs::write(questions.path_of(&source.id), serde_json::to_vec(&source).unwrap()).unwrap();
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
    let imported = Outbox::at(&home).get(&source.id).unwrap();
    assert_eq!(imported.task, task.id);
    assert_eq!(imported.delivered_to.as_deref(), Some(chat.id.as_str()));
    assert!(imported.decision.unwrap().request.is_none());
    assert!(talks.get(&chat.id).unwrap().status.open());
    assert_eq!(queue.get(&task.id).unwrap().status, magi::queue::TaskStatus::Queued);
    assert!(queue.get(&task.id).unwrap().actions_applied.is_empty());
}
}

common::e2e! {
async fn informational_release_notices_never_become_pending_decisions_during_migration() {
    use magi::ask::{Question, Questions};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("informational-release");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let questions = Questions::at(home.join("questions"));
    let mut notice = Question::new("run-a".into(), magi::bump::NOTICE_NODE.into(), "bump".into(), "Release completed".into(), "Informational only".into(), vec![]);
    questions.put(&mut notice).unwrap();
    for _ in 0..2 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
    assert!(Outbox::at(&home).list().unwrap().is_empty());
    assert!(talks.get(&chat.id).unwrap().pending_messages.is_empty());
}
}

common::e2e! {
async fn ownerless_migration_can_be_explicitly_settled_without_assigning_an_owner_or_replaying() {
    use magi::{ask::{Answer, ChoiceAction, Question, Questions}, chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("ownerless-settlement");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let questions = Questions::at(home.join("questions"));
    let mut q = Question::new("missing-run".into(), "implement".into(), "impl-A".into(), "Old request".into(), "Owner confirms no effect remains".into(), vec!["hold".into()]);
    q.actions.insert("hold".into(), ChoiceAction::Hold);
    q.answer(Answer::Choice("hold".into())).unwrap(); questions.put(&mut q).unwrap();
    let source_before = std::fs::read(questions.path_of(&q.id)).unwrap();
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    let outbox = Outbox::at(&home);
    let imported = outbox.get(&q.id).unwrap().decision.unwrap();
    assert_eq!(imported.status, Status::RetryNeeded, "historical replies without receipts need reconciliation");
    assert!(imported.request.is_none(), "a historical reply is not an accepted operation");
    chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::SettleDecision { inquiry: q.id.clone(), reason: "Owner confirms this obsolete inquiry needs no effect".into(), replacement: None }).await.unwrap();
    for _ in 0..2 {
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
        magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
    }
    let settled = Outbox::at(&home).get(&q.id).unwrap();
    assert_eq!(settled.decision.unwrap().status, Status::Resolved);
    assert!(settled.task.is_empty()); assert!(settled.delivered_to.is_none());
    assert!(queue.list().is_empty()); assert!(magi::worker::active(&home).unwrap().is_empty());
    assert_eq!(source_before, std::fs::read(questions.path_of(&q.id)).unwrap());
}
}

common::e2e! {
async fn foreground_non_task_questions_remain_answerable_after_migration_and_restart() {
    use magi::ask::{Answer, Question, Questions};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    for kind in ["empty", "run-operator", "old-agent"] {
    for old_boundary in ["new", "pending", "completed"] {
        let home = fx.tmp.path().join(format!("foreground-{kind}-{old_boundary}"));
        let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
        let questions = Questions::at(home.join("questions"));
        let mut q = Question::new(String::new(), "ask".into(), "operator".into(), "Standalone question".into(), "Foreground wait must receive the reply".into(), vec![]);
        if kind == "run-operator" { q.run = "legacy-run-without-owner".into(); }
        if kind == "old-agent" {
            let mut run = magi::run::RunState::new(fx.repo.clone(), "main".into(), "base".into(), "legacy work".into(), fx.config.clone());
            run.reserve_under(&home).unwrap(); run.origin = Some(magi::run::Origin::operator()); run.save_under(&home).unwrap();
            q.run = run.id; q.node = "implement".into(); q.seat = "impl-A".into(); q.cwd = Some(fx.repo.to_string_lossy().into_owned());
        }
        questions.put(&mut q).unwrap();
        if old_boundary != "new" {
            let dir = home.join("notifications/task-chat-migration-v1"); std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("state.json"), serde_json::to_vec(&serde_json::json!({"schema":1,"pending":if old_boundary == "pending" { vec![q.id.clone()] } else { vec![] },"sources":[q.id],"errors":{}})).unwrap()).unwrap();
        }
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
        questions.update(&q.id, |q| q.answer(Answer::Text("Fresh owner reply".into()))).unwrap();
        for _ in 0..2 {
            magi::task_chat::sweep(&queue, &talks, &questions, &home);
            assert_eq!(Questions::at(home.join("questions")).get(&q.id).unwrap().resolution().as_deref(), Some("Fresh owner reply"));
            assert_eq!(questions.list()[0].resolution().as_deref(), Some("Fresh owner reply"));
            assert!(Outbox::at(&home).list().unwrap().is_empty());
        }
    }
    }
}
}

common::e2e! {
async fn rejected_resume_preconditions_remain_unstarted_and_explicitly_cancellable() {
    use magi::{chat_control::{self, Operation}, run::{RunState, RunStatus}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("resume-preconditions");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    for rejection in ["older", "done", "running"] {
        let mut state = RunState::new(fx.repo.clone(), "main".into(), "base".into(), "work".into(), fx.config.clone());
        state.reserve_under(&home).unwrap(); state.status = RunStatus::Blocked; state.driver_exited = true; state.save_under(&home).unwrap();
        let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
        task.chat = Some(chat.id.clone()); task.runs.push(state.id.clone());
        match rejection {
            "older" => task.runs.push("newer-run".into()),
            "done" => task.status = magi::queue::TaskStatus::Done,
            "running" => task.status = magi::queue::TaskStatus::Running,
            _ => unreachable!(),
        }
        queue.put(&mut task).unwrap();
        let outbox = Outbox::at(&home);
        let q = outbox.file(&task, Some(state.id.clone()), "owner".into(), "Resume request", rejection).unwrap();
        outbox.deliver(&queue, &talks, &q.id).unwrap();
        let before = std::fs::read(queue.path_of(&task.id)).unwrap();
        for _ in 0..2 {
            assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision { inquiry: q.id.clone(), command: Box::new(Operation::Resume { run: state.id.clone() }) }).await.is_err());
            chat_control::reconcile_decisions(&queue, &home).unwrap();
            let failed = Outbox::at(&home).get(&q.id).unwrap().decision.unwrap();
            assert_eq!(failed.status, Status::RetryNeeded); assert!(failed.retry_safe);
            assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        }
        chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::SettleDecision { inquiry: q.id.clone(), reason: "Owner cancels the rejected resume".into(), replacement: None }).await.unwrap();
        assert_eq!(outbox.get(&q.id).unwrap().decision.unwrap().status, Status::Resolved);
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        assert!(magi::run::RunState::load_under(&state.id, &home).unwrap().decision_starts.is_empty());
    }
    assert!(magi::worker::active(&home).unwrap().is_empty());
}
}

common::e2e! {
async fn task_scoped_cli_ask_survives_unrelated_corruption_without_overwriting_it() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    publish_config(&fx);
    let home = fx.tmp.path().join("file-around-corruption");
    let queue = Queue::at(home.join("queue"));
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
    let dir = home.join("notifications/task-inquiries"); std::fs::create_dir_all(&dir).unwrap();
    let corrupt = dir.join("unrelated.json"); std::fs::write(&corrupt, "{truncated").unwrap();
    for _ in 0..2 {
        let output = common::magi_cli(&home).current_dir(&fx.repo)
            .env("MAGI_RUN", "run-a").env("MAGI_NODE", "implement").env("MAGI_SEAT", "impl-A")
            .args(["ask", "--summary", "New request", "--detail", "Unrelated corruption must stay isolated", "--request", "stable-request"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let (items, unreadable) = Outbox::at(&home).list_readable().unwrap();
        assert_eq!(items.len(), 1); assert_eq!(unreadable, 1);
        assert_eq!(items[0].task, task.id); assert!(items[0].decision.as_ref().unwrap().request.is_none());
        assert_eq!(std::fs::read_to_string(&corrupt).unwrap(), "{truncated");
    }
}
}

common::e2e! {
async fn real_foreground_ask_with_a_run_survives_migration_and_receives_a_fresh_reply() {
    use magi::ask::{Answer, Questions};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    publish_config(&fx);
    let home = fx.tmp.path().join("run-foreground");
    let queue = Queue::at(home.join("queue")); let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut child = common::magi_cli(&home).current_dir(&fx.repo)
        .env("MAGI_RUN", "run-without-a-task").env("MAGI_NODE", "implement").env("MAGI_SEAT", "impl-A")
        .args(["ask", "--summary", "Foreground request", "--detail", "No task owns this run", "--timeout", "15"])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let q = loop {
        if let Some(q) = questions.list().pop() { break q; }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill(); let output = child.wait_with_output().unwrap();
            panic!("foreground inquiry not filed: {}", String::from_utf8_lossy(&output.stderr));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // Finish/reap the child before making assertions, including failure paths.
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    let answer = questions.update(&q.id, |q| q.answer(Answer::Text("Fresh foreground reply".into())));
    if answer.is_err() { let _ = child.kill(); }
    let output = child.wait_with_output().unwrap();
    answer.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Fresh foreground reply"));
    assert!(q.legacy_foreground); assert_eq!(q.run, "run-without-a-task");
    assert!(Outbox::at(&home).list().unwrap().is_empty()); assert!(queue.list().is_empty());
    assert_eq!(Questions::at(home.join("questions")).get(&q.id).unwrap().resolution().as_deref(), Some("Fresh foreground reply"));
}
}

common::e2e! {
async fn superseded_resume_pins_reconcile_without_starting_or_replaying() {
    use magi::{chat_control::{self, Operation}, run::{RunState, RunStatus}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("superseded-pins");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    for transition in ["replacement", "done", "retry", "dispatch"] {
        let mut run = RunState::new(fx.repo.clone(), "main".into(), "base".into(), "work".into(), fx.config.clone());
        run.reserve_under(&home).unwrap();
        run.status = RunStatus::Blocked;
        run.driver_exited = true;
        run.save_under(&home).unwrap();
        let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
        task.chat = Some(chat.id.clone());
        task.runs.push(run.id.clone());
        queue.put(&mut task).unwrap();
        let outbox = Outbox::at(&home);
        let first = outbox.file(&task, Some(run.id.clone()), "owner".into(), "Resume", transition).unwrap();
        outbox.deliver(&queue, &talks, &first.id).unwrap();
        chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
            inquiry: first.id.clone(), command: Box::new(Operation::Resume { run: run.id.clone() })
        }).await.unwrap();
        chat_control::reconcile_decisions(&queue, &home).unwrap();
        assert_eq!(outbox.get(&first.id).unwrap().decision.unwrap().status, Status::Queued);
        if transition == "dispatch" {
            let mut pending = queue.get(&task.id).unwrap();
            pending.start(run.id.clone());
            queue.put(&mut pending).unwrap();
            chat_control::reconcile_decisions(&queue, &home).unwrap();
            assert_eq!(outbox.get(&first.id).unwrap().decision.unwrap().status, Status::Queued,
                "normal dispatch consumes the pin but still owes a committed execution pass");
            assert!(queue.get(&task.id).unwrap().superseded_resumes.is_empty());
            continue;
        }
        if transition == "replacement" {
            let second = outbox.file(&task, Some(run.id.clone()), "owner".into(), "Resume again", "New request").unwrap();
            outbox.deliver(&queue, &talks, &second.id).unwrap();
            chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
                inquiry: second.id.clone(), command: Box::new(Operation::Resume { run: run.id.clone() })
            }).await.unwrap();
            chat_control::reconcile_decisions(&queue, &home).unwrap();
            assert_eq!(outbox.get(&second.id).unwrap().decision.unwrap().status, Status::Queued);
        } else {
            let operation = if transition == "done" {
                Operation::Done { task: task.id.clone() }
            } else {
                Operation::Retry { task: task.id.clone(), title: None, instruction: "New instruction".into() }
            };
            chat_control::dispatch(&queue, &home, &talks, &chat.id, operation).await.unwrap();
        }
        // Re-open both stores to exercise the durable receipt, including explicit reconciliation.
        chat_control::reconcile_decisions(&Queue::at(home.join("queue")), &home).unwrap();
        chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ReconcileDecision { inquiry: first.id.clone() }).await.unwrap();
        assert_eq!(Outbox::at(&home).get(&first.id).unwrap().decision.unwrap().status, Status::Resolved);
        let disk = RunState::load_under(&run.id, &home).unwrap();
        assert!(disk.decision_starts.is_empty());
        assert!(disk.passes.is_empty());
    }
}
}

common::e2e! {
async fn abandonment_receipt_recovers_a_lost_outcome_without_reexecution() {
    use magi::{chat_control::{self, Operation}, run::{RunState, RunStatus}, task_decision::{Decision, Status}};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("abandon-receipt");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut run = RunState::new(fx.repo.clone(), "main".into(), "base".into(), "work".into(), fx.config.clone());
    run.reserve_under(&home).unwrap();
    run.status = RunStatus::Blocked;
    run.driver_exited = true;
    run.save_under(&home).unwrap();
    let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    task.runs.push(run.id.clone());
    queue.put(&mut task).unwrap();
    let outbox = Outbox::at(&home);
    let record = outbox.file(&task, Some(run.id.clone()), "owner".into(), "Abandon", "Stop the dead execution").unwrap();
    outbox.deliver(&queue, &talks, &record.id).unwrap();
    let command = Operation::Abandon { run: run.id.clone() };
    let request = serde_json::to_string(&command).unwrap();
    chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
        inquiry: record.id.clone(), command: Box::new(command)
    }).await.unwrap();
    let disk = RunState::load_under(&run.id, &home).unwrap();
    assert_eq!(disk.status, RunStatus::Failed);
    assert_eq!(disk.abandonment_receipts.get(&record.id), Some(&request));
    let run_bytes = std::fs::read(home.join("runs").join(&run.id).join("run.json")).unwrap();
    // Simulate the final projection write being lost after the run commit.
    let mut lost = outbox.get(&record.id).unwrap();
    let mut decision = Decision::awaiting("Abandon".into());
    decision.accept(&request).unwrap();
    decision.start().unwrap();
    decision.fail("Outcome write lost").unwrap();
    lost.decision = Some(decision);
    let path = home.join("notifications/task-inquiries").join(format!("{}.json", record.id));
    std::fs::write(&path, serde_json::to_vec(&lost).unwrap()).unwrap();
    chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ReconcileDecision { inquiry: record.id.clone() }).await.unwrap();
    assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status, Status::Resolved);
    std::fs::write(&path, serde_json::to_vec(&lost).unwrap()).unwrap();
    chat_control::reconcile_decisions(&Queue::at(home.join("queue")), &home).unwrap();
    assert_eq!(Outbox::at(&home).get(&record.id).unwrap().decision.unwrap().status, Status::Resolved);
    assert_eq!(std::fs::read(home.join("runs").join(&run.id).join("run.json")).unwrap(), run_bytes);
    // A terminal status alone cannot confirm a different inquiry.
    let other = outbox.file(&task, Some(run.id.clone()), "owner".into(), "Another abandonment", "No receipt for this request").unwrap();
    outbox.deliver(&queue, &talks, &other.id).unwrap();
    outbox.update_decision(&other.id, |d| { d.accept(&request)?; d.start()?; Ok(true) }).unwrap();
    chat_control::reconcile_decisions(&queue, &home).unwrap();
    assert_ne!(outbox.get(&other.id).unwrap().decision.unwrap().status, Status::Resolved);
}
}

common::e2e! {
async fn applied_legacy_choice_receipts_survive_later_task_transitions() {
    use magi::{ask::{ChoiceAction, Question, Questions}, chat_control, run::{RunState, RunStatus}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("choice-receipts");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    for action in [ChoiceAction::Requeue, ChoiceAction::Hold, ChoiceAction::Done] {
        let mut run = RunState::new(fx.repo.clone(), "main".into(), "base".into(), "work".into(), fx.config.clone());
        run.reserve_under(&home).unwrap();
        run.status = RunStatus::Blocked; run.driver_exited = true; run.save_under(&home).unwrap();
        let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
        task.chat = Some(chat.id.clone()); task.runs.push(run.id.clone()); task.hold_manual(Some("Awaiting input".into()));
        queue.put(&mut task).unwrap();
        let mut q = Question::new(run.id.clone(), "implement".into(), "impl-A".into(), "Choose next step".into(), String::new(), vec!["apply".into()]);
        q.actions.insert("apply".into(), action.clone());
        questions.put(&mut q).unwrap();
        magi::task_chat::post(&queue, &talks, &home, &task.id, &q).unwrap();
        chat_control::decide(&talks, &home, &chat.id, &q.id, "apply").unwrap();
        let outbox = Outbox::at(&home);
        assert_eq!(outbox.get(&q.id).unwrap().decision.unwrap().status, Status::Queued);
        magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
        let mut applied = queue.get(&task.id).unwrap();
        assert!(applied.action_applied(&q.id));
        assert_eq!(applied.decision_effects.get(&q.id).map(String::as_str), Some("decision: apply"));
        match action {
            ChoiceAction::Requeue => {
                assert!(applied.fresh_start);
                applied.start(run.id.clone()); // A worker wins the race before the next sweep.
                assert!(!applied.fresh_start);
            }
            ChoiceAction::Hold => {
                applied.release_operator("chat", None);
                applied.hold_manual(Some("A later, unrelated reason".into()));
            }
            ChoiceAction::Done => assert_eq!(applied.status, magi::queue::TaskStatus::Done),
            _ => unreachable!(),
        }
        queue.put(&mut applied).unwrap();
        let before = std::fs::read(queue.path_of(&task.id)).unwrap();
        magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before, "the legacy effect still executes once");
        magi::task_chat::sweep(&Queue::at(home.join("queue")), &talks, &questions, &home);
        assert_eq!(Outbox::at(&home).get(&q.id).unwrap().decision.unwrap().status, Status::Resolved);
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before, "receipt reconciliation must not replay an effect");
    }
}
}

common::e2e! {
async fn abandonment_preflight_is_retryable_but_a_failed_write_remains_uncertain() {
    use magi::{chat_control::{self, Operation}, run::{RunState, RunStatus}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("abandon-preflight");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    for cause in ["live", "terminal", "write"] {
        let mut run = RunState::new(fx.repo.clone(), "main".into(), "base".into(), "work".into(), fx.config.clone());
        run.reserve_under(&home).unwrap();
        run.status = if cause == "terminal" { RunStatus::Failed } else { RunStatus::Blocked };
        run.driver_exited = cause != "live";
        if cause == "live" {
            run.driver_pid = Some(std::process::id());
            run.driver_started_at = magi::proc::process_started_at(std::process::id());
        }
        run.save_under(&home).unwrap();
        let run_path = home.join("runs").join(&run.id).join("run.json");
        let before = std::fs::read(&run_path).unwrap();
        let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
        task.chat = Some(chat.id.clone()); task.runs.push(run.id.clone()); queue.put(&mut task).unwrap();
        let outbox = Outbox::at(&home);
        let record = outbox.file(&task, Some(run.id.clone()), "owner".into(), "Abandon", cause).unwrap();
        outbox.deliver(&queue, &talks, &record.id).unwrap();
        let blocked_write = home.join("runs").join(&run.id).join("run.json.tmp");
        if cause == "write" { std::fs::create_dir(&blocked_write).unwrap(); }
        for _ in 0..2 {
            assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
                inquiry: record.id.clone(), command: Box::new(Operation::Abandon { run: run.id.clone() })
            }).await.is_err());
            let failed = outbox.get(&record.id).unwrap().decision.unwrap();
            assert_eq!(failed.status, Status::RetryNeeded);
            assert_eq!(failed.retry_safe, cause != "write");
            assert_eq!(std::fs::read(&run_path).unwrap(), before);
        }
        if cause == "live" {
            run.driver_pid = None; run.driver_started_at = None; run.driver_exited = true;
            run.save_under(&home).unwrap();
            chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
                inquiry: record.id.clone(), command: Box::new(Operation::Abandon { run: run.id.clone() })
            }).await.unwrap();
            assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status, Status::Resolved);
            assert_eq!(RunState::load_under(&run.id, &home).unwrap().status, RunStatus::Failed);
        } else {
            let settlement = chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::SettleDecision {
                inquiry: record.id.clone(), reason: "Cancel the refused request".into(), replacement: None
            }).await;
            if cause == "terminal" {
                settlement.unwrap();
                assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status, Status::Resolved);
            } else {
                assert!(settlement.is_err());
                std::fs::remove_dir(&blocked_write).unwrap();
                chat_control::reconcile_decisions(&queue, &home).unwrap();
                assert_eq!(outbox.get(&record.id).unwrap().decision.unwrap().status, Status::RetryNeeded);
            }
        }
    }
}
}

#[cfg(unix)]
common::e2e! {
async fn native_resume_of_a_landing_hold_records_its_exact_execution_pass() {
    use magi::{chat_control::{self, Operation}, merge_decision::{Decision, Target}, run::{RunState, RunStatus, Origin, PrRecord}, task_decision::Status};
    use std::os::unix::fs::PermissionsExt as _;
    let mut fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0; fx.config.disk.auto_fold = false; fx.config.disk.cache_limit_bytes = 0;
    let home = magi::run::home();
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new("Resume held landing".into(), "Preserve the existing work".into(), fx.repo.clone(), Source::Human);
    task.chat = Some(chat.id.clone());
    let head = magi::git::git(&fx.repo, &["rev-parse", "HEAD"]).await.unwrap().trim().to_owned();
    let forge = fx.tmp.path().join("held-landing-forge");
    std::fs::create_dir(&forge).unwrap();
    let script = forge.join("gh");
    std::fs::write(&script, r#"#!/bin/sh
printf '%s
' "$*" >> "$RESUME_FORGE/calls"
case "$1 $2" in
  'pr view') cat "$RESUME_FORGE/view.json" ;;
  'api graphql') cat "$RESUME_FORGE/checks.json" ;;
  'api repos/{owner}/{repo}/pulls/1/comments') echo '[]' ;;
  'pr merge') case "$*" in *--disable-auto*) exit 0 ;; *) exit 99 ;; esac ;;
  *) exit 1 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let url = "https://github.com/example/repo/pull/1";
    std::fs::write(forge.join("view.json"), serde_json::to_vec(&serde_json::json!({
        "url":url, "number":1, "state":"OPEN", "title":"Existing work", "reviews":[], "comments":[],
        "mergeStateStatus":"CLEAN", "headRefOid":head, "baseRefName":"main", "autoMergeRequest":null
    })).unwrap()).unwrap();
    std::fs::write(forge.join("checks.json"), serde_json::to_vec(&serde_json::json!({
        "data":{"repository":{"pullRequest":{"commits":{"nodes":[{"commit":{"oid":head,
        "statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[
        {"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]}}}}
    })).unwrap()).unwrap();
    let mut paths = vec![forge.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    // SAFETY: this e2e runs in its own process before spawning the runner.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("RESUME_FORGE", &forge);
    }
    let mut state = RunState::new(fx.repo.clone(), "main".into(), head.clone(), task.instruction.clone(), fx.config.clone());
    state.reserve_under(&home).unwrap();
    state.origin = Some(Origin::queue(&task.id));
    state.status = RunStatus::Blocked; state.driver_exited = true;
    state.merge_decision = Some(Decision {
        question:"previous-hold".into(), target:Target { pr:url.into(), head:head.clone(), language:"en".into() },
        choice:magi::land::HOLD.into(), closed:false, replacement_run:None, replacement_pr:None,
    });
    state.pr = Some(PrRecord { url:url.into(), number:1, state:"open".into(), checks:"green".into(), round:1, rounds:1, red_at_merge:vec![] });
    state.begin_pass(); state.save_under(&home).unwrap();
    let old_passes = state.passes.len();
    task.runs.push(state.id.clone()); task.hold_manual(Some("Landing held by owner".into())); queue.put(&mut task).unwrap();
    let record = Outbox::at(&home).file(&task, Some(state.id.clone()), "owner".into(), "Resume the held landing", "Ask for a fresh decision").unwrap();
    Outbox::at(&home).deliver(&queue, &talks, &record.id).unwrap();
    chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ForDecision {
        inquiry:record.id.clone(), command:Box::new(Operation::Resume { run:state.id.clone() })
    }).await.unwrap();
    assert_eq!(Outbox::at(&home).get(&record.id).unwrap().decision.unwrap().status, Status::Queued);
    let config = fx.tmp.path().join("held-landing-config.toml");
    std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
    let opts = magi::daemon::Opts { repo:fx.repo.clone(), config:Some(config), worktrees_root:Some(fx.tmp.path().join("held-landing-wt")), ..Default::default() };
    let mut pending = queue.get(&task.id).unwrap();
    let claim = queue.claim(&task.id).unwrap();
    magi::daemon::run_claimed(&opts, &queue, &mut pending).await;
    drop(claim);
    let disk = RunState::load_under(&state.id, &home).unwrap();
    assert_eq!(disk.passes.len(), old_passes + 1);
    let pass = disk.decision_starts.get(&record.id).expect("landing-stop branch must carry the native request into begin_pass");
    assert_eq!(*pass, old_passes);
    assert!(disk.passes[*pass].task_entry);
    assert!(disk.parked, "the existing held PR should await a fresh owner answer: {:?}", queue.get(&task.id).unwrap().last_error);
    assert!(disk.merge_decision.is_none(), "the old hold was reopened");
    assert!(disk.land_approval.as_ref().is_some_and(|approval| approval.question != "previous-hold"));
    let current = queue.get(&task.id).unwrap();
    assert_eq!(current.runs, vec![state.id.clone()]);
    assert!(current.superseded_resumes.is_empty(), "normal dispatch is not supersession");
    chat_control::reconcile_decisions(&queue, &home).unwrap();
    chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ReconcileDecision { inquiry:record.id.clone() }).await.unwrap();
    assert_eq!(Outbox::at(&home).get(&record.id).unwrap().decision.unwrap().status, Status::Resolved);
    let calls = std::fs::read_to_string(forge.join("calls")).unwrap();
    assert!(calls.lines().filter(|line| line.starts_with("pr merge")).all(|line| line.contains("--disable-auto")));
}
}

common::e2e! {
async fn accepted_legacy_effects_support_explicit_settlement_without_replay() {
    use magi::{ask::{Question, Questions}, chat_control::{self, Operation}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    for (node, seat, choice) in [
        (magi::bump::NOTICE_NODE, "release-watch", "merge"),
        (magi::bump::NOTICE_NODE, "release-watch", "retry"),
        (magi::bump::NOTICE_NODE, "release-watch", "rerun again"),
        (magi::reconcile::NODE, magi::reconcile::SEAT, "keep"),
    ] {
        let home = fx.tmp.path().join(choice);
        let queue = Queue::at(home.join("queue"));
        let questions = Questions::at(home.join("questions"));
        let talks = Talks::at(home.join("talks"));
        let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
        let other = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
        let mut task = Task::new("legacy".into(), "legacy".into(), fx.repo.clone(), Source::Human);
        task.origin_chat = Some(chat.id.clone()); task.runs.push("run-a".into()); queue.put(&mut task).unwrap();
        let mut q = Question::new("run-a".into(), node.into(), seat.into(), "Owner decision".into(), String::new(), vec![choice.into()]);
        if choice == "merge" { q.merge_target = Some(magi::merge_decision::Target { pr: "https://example.test/pull/1".into(), head: "head".into(), language: "en".into() }); }
        questions.put(&mut q).unwrap();
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
        chat_control::decide(&talks, &home, &chat.id, &q.id, choice).unwrap();
        let saved = std::fs::read(questions.path_of(&q.id)).unwrap();
        assert_eq!(Outbox::at(&home).get(&q.id).unwrap().decision.unwrap().status, Status::Queued);
        assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::ReconcileDecision { inquiry: q.id.clone() }).await.is_err());
        for (speaker, reason) in [(&other.id, "Owner inspected the outcome"), (&chat.id, " ")] {
            assert!(chat_control::dispatch(&queue, &home, &talks, speaker, Operation::SettleDecision { inquiry: q.id.clone(), reason: reason.into(), replacement: None }).await.is_err());
        }
        // Task-backed consumers share their task claim with settlement, even
        // when they do not project InProgress into the decision record.
        let before_contention = Outbox::at(&home).get(&q.id).unwrap().decision.unwrap();
        let claim = queue.claim(&task.id).unwrap();
        assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::SettleDecision {
            inquiry: q.id.clone(), reason: "Consumer is still executing".into(), replacement: None,
        }).await.is_err());
        assert_eq!(Outbox::at(&home).get(&q.id).unwrap().decision.unwrap(), before_contention);
        drop(claim);
        // Preserve the uncertain outcome in history, then explicitly confirm
        // that no further effect is needed. This is not an application receipt.
        if choice == "retry" {
            Outbox::at(&home).update_decision(&q.id, |d| d.start()).unwrap();
            assert!(chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::SettleDecision { inquiry: q.id.clone(), reason: "Do not hide an executing consumer".into(), replacement: None }).await.is_err());
            Outbox::at(&home).update_decision(&q.id, |d| d.fail("Consumer ended without an authoritative receipt")).unwrap();
        }
        chat_control::dispatch(&queue, &home, &talks, &chat.id, Operation::SettleDecision { inquiry: q.id.clone(), reason: "Owner inspected the external outcome and confirms no effect remains".into(), replacement: None }).await.unwrap();
        for _ in 0..2 {
            magi::task_chat::sweep(&queue, &talks, &questions, &home);
            let decision = Outbox::at(&home).get(&q.id).unwrap().decision.unwrap();
            assert_eq!(decision.status, Status::Resolved); assert!(decision.legacy_settled);
            assert_eq!(decision.request.as_deref(), Some(format!("decision: {choice}").as_str()));
            if choice == "retry" { assert!(decision.history.iter().any(|e| e.status == Status::RetryNeeded)); }
            assert!(questions.get_for_effects(&q.id).unwrap().answer.is_none());
            assert!(questions.get_for_effects(&q.id).unwrap().status.open(), "settlement must not activate abandoned-question defaults");
            assert!(!questions.list_for_effects().into_iter().any(|q| q.answer.is_some()));
            assert!(questions.get(&q.id).unwrap().answer.is_some(), "presentation retains the recorded answer");
            assert_eq!(std::fs::read(questions.path_of(&q.id)).unwrap(), saved);
            assert!(chat_control::decide(&talks, &home, &chat.id, &q.id, choice).is_err());
        }
    }
}
}

common::e2e! {
async fn fresh_cli_answers_survive_migration_before_and_after_first_sweep() {
    use magi::{ask::{Answer, ChoiceAction, Question, Questions}, task_decision::Status};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    for (migrate_first, associate_later, delivery_fails) in [(false, false, false), (true, false, false), (false, true, false), (false, true, true)] {
        let home = fx.tmp.path().join(format!("fresh-cli-{migrate_first}-{associate_later}-{delivery_fails}"));
        let queue = Queue::at(home.join("queue"));
        let talks = Talks::at(home.join("talks"));
        let questions = Questions::at(home.join("questions"));
        let mut task = Task::new("work".into(), "work".into(), fx.repo.clone(), Source::Human);
        task.runs.push("run-a".into());
        if !associate_later { queue.put(&mut task).unwrap(); }
        let mut fresh = Question::new("run-a".into(), "implement".into(), "impl-A".into(), "Fresh decision".into(), "Keep this work held".into(), vec!["hold".into()]);
        fresh.actions.insert("hold".into(), ChoiceAction::Hold);
        questions.put(&mut fresh).unwrap();
        let mut historical = fresh.clone();
        historical.id = format!("{}-historical", fresh.id);
        historical.answer(Answer::Choice("hold".into())).unwrap();
        questions.put(&mut historical).unwrap();
        let history_before = historical.clone();
        if migrate_first { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
        let output = common::magi_cli(&home).args(["answer", &fresh.id, "--reply", "hold"])
            .current_dir(&fx.repo).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let accepted: Question = serde_json::from_slice(&std::fs::read(questions.path_of(&fresh.id)).unwrap()).unwrap();
        assert!(accepted.accepted_answer_at.is_some(), "CLI must persist fresh acceptance before migration; migrate_first={migrate_first} associate_later={associate_later} delivery_fails={delivery_fails}; executable={:?}; source={accepted:?}; stdout={}", common::magi_cli(&home).get_program(), String::from_utf8_lossy(&output.stdout));
        assert_eq!(accepted.accepted_answer_at, accepted.answered_at, "CLI acceptance must bind the exact response generation");
        if associate_later { queue.put(&mut task).unwrap(); }
        // A late ownership association must preserve fresh consent even
        // when reserving or creating its chat cannot deliver the inquiry.
        if delivery_fails { std::fs::write(talks.root(), "delivery unavailable").unwrap(); }
        for _ in 0..2 {
            magi::task_chat::sweep(&queue, &talks, &questions, &home);
            let reopened = Questions::at(home.join("questions")).get(&fresh.id).unwrap();
            assert_eq!(reopened.resolution().as_deref(), Some("hold"));
            let decision = Outbox::at(&home).get(&fresh.id).unwrap().decision.unwrap();
            assert_eq!(decision.request.as_deref(), Some("decision: hold"), "migrate_first={migrate_first} associate_later={associate_later} delivery_fails={delivery_fails}: {decision:?}; source={reopened:?}");
            assert_eq!(decision.source_answer_at, reopened.answered_at);
            assert_eq!(decision.status, Status::Queued, "acceptance is not an effect receipt");
            assert!(questions.get_for_effects(&historical.id).unwrap().answer.is_none());
            assert!(Outbox::at(&home).get(&historical.id).unwrap().decision.unwrap().request.is_none());
        }
        if delivery_fails {
            assert!(Outbox::at(&home).get(&fresh.id).unwrap().delivered_to.is_none());
            assert!(!magi::task_chat::migration_issues(&home).is_empty());
            assert!(!queue.get(&task.id).unwrap().action_applied(&fresh.id), "acceptance is not an effect");
            std::fs::remove_file(talks.root()).unwrap();
        }
        magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
        let applied = queue.get(&task.id).unwrap();
        assert!(applied.operator_held());
        assert!(applied.action_applied(&fresh.id));
        assert!(!applied.action_applied(&historical.id));
        assert_eq!(Outbox::at(&home).get(&fresh.id).unwrap().decision.unwrap().status, Status::Resolved);
        let history_after: Question = serde_json::from_slice(&std::fs::read(questions.path_of(&historical.id)).unwrap()).unwrap();
        assert_eq!(history_before.answer, history_after.answer);
        assert_eq!(history_before.answered_at, history_after.answered_at);
        assert!(history_after.accepted_answer_at.is_none(), "migration must not accept saved history");
    }
}
}
