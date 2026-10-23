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
    assert_eq!(magi::chat_notify::talks_to_start(&talks), std::slice::from_ref(&chat.id));
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
    let words = format!("{}: merge reviewed-head", q.id);
    assert!(magi::chat_control::decide(&queue, &talks, &home, &chat.id, &q.id,
        magi::land::APPROVE, Some(&words)).is_err(), "even exact owner text cannot merge through task control");
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

common::e2e! {
async fn targetless_release_approval_keeps_its_compatibility_card_and_answer() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut task = task(&fx.repo, Source::Human);
    queue.put(&mut task).unwrap();
    let mut q = question("");
    q.node = magi::bump::NOTICE_NODE.into();
    q.seat = "release-watch".into();
    q.choices = vec![magi::land::APPROVE.into(), magi::land::HOLD.into()];
    questions.put(&mut q).unwrap();
    // Old records can identify their task through the watcher, without any
    // saved approval head. Never manufacture a head or strand their card.
    std::fs::create_dir_all(home.join("release-watch")).unwrap();
    std::fs::write(home.join("release-watch/legacy.json"), serde_json::to_vec(&serde_json::json!({
        "question": q.id, "run": "run-a", "url": "https://example.test/pull/1",
        "repo": fx.repo, "local": true
    })).unwrap()).unwrap();
    for _ in 0..2 { magi::task_chat::sweep(&queue, &talks, &questions, &home); }
    let q = questions.get(&q.id).unwrap();
    assert!(q.routed_task.is_none());
    assert!(q.routed_chat.is_none());
    assert!(q.merge_target.is_none());
    assert!(talks.list().is_empty());
    assert!(magi::deputy::kind_of(&q).is_some());
    // Repair a record already misrouted by the previous implementation too.
    let mut misrouted = q.clone();
    misrouted.routed_task = Some(task.id.clone());
    misrouted.routed_chat = Some("old-chat".into());
    misrouted.consult = Some(magi::ask::ChatConsult { talk: "old-chat".into(), at: jiff::Timestamp::now() });
    questions.put(&mut misrouted).unwrap();
    let repaired = questions.get(&q.id).unwrap();
    assert!(repaired.routed_task.is_none());
    assert!(repaired.routed_chat.is_none());
    assert!(repaired.consult.is_none());
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    assert!(talks.list().is_empty());
    let (q, ()) = questions.update(&q.id, |q| q.answer(Answer::Choice(magi::land::HOLD.into()))).unwrap();
    assert_eq!(q.resolution().as_deref(), Some(magi::land::HOLD));
}
}

common::e2e! {
async fn approval_recovery_uses_chat_control_with_individual_owner_confirmation() {
    use magi::{ask::ChoiceAction, run::{RunState, RunStatus, LandApproval, PrRecord}};
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    for choice in [magi::land::REPAIR.to_owned(), magi::merge_decision::WITHDRAW.to_owned(),
        format!("{}alpha", magi::merge_decision::REIMPLEMENT), magi::merge_decision::DISMISS.to_owned()] {
        let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
        let other = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
        let mut run = RunState::new(fx.repo.clone(), "main".into(), "base".into(), "work".into(), fx.config.clone());
        run.status = RunStatus::Landing;
        run.parked = true;
        run.driver_exited = true;
        run.pr = Some(PrRecord { url: "https://example.test/pull/1".into(), number: 1,
            state: "open".into(), checks: "green".into(), round: 0, rounds: 1, red_at_merge: vec![] });
        let mut task = task(&fx.repo, Source::Agent { run: chat.id.clone(), node: magi::queue::CHAT_NODE.into() });
        task.runs = vec![run.id.clone()];
        task.status = magi::queue::TaskStatus::Parked;
        queue.put(&mut task).unwrap();
        let mut q = question(&run.id);
        q.node = magi::land::APPROVAL_NODE.into();
        q.merge_target = Some(magi::merge_decision::Target { pr: run.pr.as_ref().unwrap().url.clone(),
            head: "reviewed-head".into(), language: "en".into() });
        q.choices = vec![magi::land::APPROVE.into(), magi::land::HOLD.into(), choice.clone()];
        run.land_approval = Some(LandApproval { question: q.id.clone(), head: "reviewed-head".into() });
        run.save_under(&home).unwrap();
        questions.put(&mut q).unwrap();
        magi::task_chat::sweep(&queue, &talks, &questions, &home);
        assert!(talks.get(&chat.id).unwrap().pending.contains("non-merge recovery"));
        assert!(magi::chat_control::decide(&queue, &talks, &home, &other.id, &q.id, &choice, None).is_err());
        assert!(magi::chat_control::decide(&queue, &talks, &home, &chat.id, &q.id, &choice, None).is_err());
        talk::record(&mut chat, &talks, "go ahead", vec![]).unwrap();
        assert!(magi::chat_control::decide(&queue, &talks, &home, &chat.id, &q.id, &choice, Some("go ahead")).is_err());
        let words = format!("{}: {}", q.id, choice);
        talk::record(&mut chat, &talks, &words, vec![]).unwrap();
        let answered = magi::chat_control::decide(&queue, &talks, &home, &chat.id, &q.id, &choice, Some(&words)).unwrap();
        assert_eq!(answered.resolution().as_deref(), Some(choice.as_str()));
        assert_eq!(answered.answer_authority.as_ref().unwrap().instruction, words);
        if choice == magi::merge_decision::DISMISS {
            assert_eq!(RunState::load_under(&run.id, &home).unwrap().status, RunStatus::Superseded);
            assert_eq!(queue.get(&task.id).unwrap().status, magi::queue::TaskStatus::Done);
        } else {
            // Dispatch stays on the existing Resume action and its owner /
            // liveness checks; this command only records the authorized choice.
            assert!(matches!(answered.chosen_action(), Some(ChoiceAction::Resume { run: id }) if id == &run.id));
            assert_eq!(RunState::load_under(&run.id, &home).unwrap().status, RunStatus::Landing);
        }
    }
}
}

common::e2e! {
async fn publication_divergence_and_release_decisions_route_and_wait_for_the_chat_control() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("outbox");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let other = talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = task(&fx.repo, Source::Agent { run: chat.id.clone(), node: magi::queue::CHAT_NODE.into() });
    queue.put(&mut task).unwrap();
    let mut ids = Vec::new();
    for (node, seat, choice) in [
        (magi::github_text::ASK_NODE, "posting-gate", "use fallback"),
        (magi::reconcile::NODE, magi::reconcile::SEAT, "keep"),
        (magi::bump::NOTICE_NODE, "release-watch", "leave it"),
    ] {
        let mut q = question("run-a");
        q.node = node.into();
        q.seat = seat.into();
        q.choices = vec![choice.into()];
        questions.put(&mut q).unwrap();
        assert_eq!(q.routed_task.as_deref(), Some(task.id.as_str()), "{node}");
        ids.push(q.id);
    }
    magi::task_chat::sweep(&queue, &talks, &questions, &home);
    let delivered = talks.get(&chat.id).unwrap();
    assert_eq!(delivered.pending_messages.len(), 3);
    assert!(delivered.pending.contains("magi task control decide"));
    for id in &ids {
        let q = questions.get(id).unwrap();
        // Still the record the machinery reads, but nobody waits or deputizes.
        assert!(q.status.open());
        assert_eq!(q.routed_chat.as_deref(), Some(chat.id.as_str()));
        assert!(magi::deputy::kind_of(&q).is_none());
        // Another conversation, or no owner evidence, records nothing.
        assert!(magi::chat_control::decide(&queue, &talks, &home, &other.id, id, &q.choices[0], None).is_err());
        assert!(magi::chat_control::decide(&queue, &talks, &home, &chat.id, id, &q.choices[0], None).is_err());
        assert!(questions.get(id).unwrap().status.open());
        assert!(magi::chat_control::decide(&queue, &talks, &home, &chat.id, id, "not offered", Some("x")).is_err());
    }
    // The owner's own latest words, quoted whole, settle it; the record then
    // carries the answer the release watcher applies on its next lap.
    let release = ids[2].clone();
    talk::record(&mut chat, &talks, "leave it", vec![]).unwrap();
    let q = magi::chat_control::decide(&queue, &talks, &home, &chat.id, &release, "leave it", Some("leave it")).unwrap();
    assert_eq!(q.resolution().as_deref(), Some("leave it"));
    assert!(questions.get(&ids[0]).unwrap().status.open());
}
}
