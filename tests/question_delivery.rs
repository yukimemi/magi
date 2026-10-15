//! Operator ingress preserves text only when there is a delivery route.
use std::process::Command;

use magi::ask::{Deputy, Question, Questions, WaiterKind};
use magi::config::Config;
use magi::question_delivery::{availability, require};

fn config() -> Config {
    toml::from_str(
        r#"
        [[agents]]
        id = "listener"
        kind = "command"
        command = ["true"]
    "#,
    )
    .unwrap()
}

fn question(store: &Questions) -> Question {
    let mut q = Question::new(
        "task".into(),
        magi::land::APPROVAL_NODE.into(),
        "land".into(),
        "May this PR merge?".into(),
        String::new(),
        vec!["merge".into(), "hold".into()],
    );
    q.deputy = Some(Deputy::new("reviewed head".into()));
    q.deputy.as_mut().unwrap().agent = "listener".into();
    q.answer_timeout = 3600;
    store.put(&mut q).unwrap();
    q
}

#[test]
fn restart_recovery_requires_dispatch_capacity_and_a_live_owner() {
    let home = tempfile::tempdir().unwrap();
    let store = Questions::at(home.path().join("questions"));
    let mut q = question(&store);
    let mut cfg = config();
    assert_eq!(
        availability(&store, &q, Some(&cfg)).reason,
        "daemon_unavailable"
    );
    magi::daemon::write_status_to(
        &home.path().join("daemon.json"),
        &magi::daemon::Status::new(),
    )
    .unwrap();
    assert_eq!(availability(&store, &q, Some(&cfg)).state, "waiting");
    store
        .update(&q.id, |q| {
            require(&store, q, Some(&cfg))?;
            q.say("Explain the review")
        })
        .unwrap();
    assert_eq!(store.get(&q.id).unwrap().thread.len(), 1);
    cfg.daemon.max_deputies = 0;
    cfg.graph.language = "ja".into();
    assert!(
        availability(&store, &q, Some(&cfg))
            .message
            .contains("自由文を送信できません")
    );
    assert_eq!(
        availability(&store, &q, Some(&cfg)).reason,
        "deputy_disabled"
    );
    cfg.daemon.max_deputies = 1;
    q.deputy.as_mut().unwrap().starts = magi::deputy::MAX_STARTS;
    assert_eq!(
        availability(&store, &q, Some(&cfg)).reason,
        "deputy_exhausted"
    );
    let error = require(&store, &q, Some(&cfg)).unwrap_err();
    let refusal = error
        .downcast_ref::<magi::question_delivery::TextUnavailable>()
        .unwrap();
    assert_eq!(
        refusal.reason,
        magi::question_delivery::UnavailableReason::DeputyExhausted
    );
    assert!(refusal.message.contains("起動回数"));
    cfg.graph.language = "en".into();
    let english = require(&store, &q, Some(&cfg)).unwrap_err();
    let english = english
        .downcast_ref::<magi::question_delivery::TextUnavailable>()
        .unwrap();
    assert_eq!(refusal.reason, english.reason);
    assert_ne!(refusal.message, english.message);
    store.beat(&q.id, WaiterKind::Deputy);
    assert_eq!(availability(&store, &q, None).state, "live");
}

#[test]
fn deadline_and_handover_use_existing_recovery_rules() {
    let home = tempfile::tempdir().unwrap();
    let store = Questions::at(home.path().join("questions"));
    let mut q = question(&store);
    let cfg = config();
    let mut progress = magi::updater::Progress::new("old".into(), "new".into());
    progress.advance(magi::updater::Stage::Restarting);
    magi::updater::write_progress(home.path(), &progress).unwrap();
    let lease = magi::updater::LeaseGuard::enter(home.path(), None);
    assert_eq!(availability(&store, &q, Some(&cfg)).state, "waiting");
    drop(lease);
    assert_eq!(
        availability(&store, &q, Some(&cfg)).reason,
        "daemon_unavailable"
    );
    q.asked_at -= std::time::Duration::from_secs(7200);
    assert_eq!(availability(&store, &q, Some(&cfg)).reason, "deadline");
}

#[test]
fn terminal_say_and_operator_thread_refuse_without_saving_or_forging_agent_turns() {
    let home = tempfile::tempdir().unwrap();
    let store = Questions::at(home.path().join("questions"));
    let mut q = question(&store);
    q.deputy.as_mut().unwrap().starts = magi::deputy::MAX_STARTS;
    store.put(&mut q).unwrap();
    let before = std::fs::read(store.path_of(&q.id)).unwrap();
    for args in [
        vec!["answer", &q.id, "--say", "close the PR"],
        vec!["ask", "--thread", &q.id, "--summary", "close the PR"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_magi"))
            .args(args)
            .env("MAGI_HOME", home.path())
            .env_remove("MAGI_RUN")
            .env_remove("MAGI_NODE")
            .env_remove("MAGI_SEAT")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("deputy_exhausted"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(store.path_of(&q.id)).unwrap(), before);
    }
}

#[tokio::test]
async fn an_accepted_recovery_message_is_consumed_by_the_deputy_dispatcher() {
    let home = tempfile::tempdir().unwrap();
    let store = Questions::at(home.path().join("questions"));
    let mut q = question(&store);
    q.deputy.as_mut().unwrap().starts = 1; // A previous process died.
    store.put(&mut q).unwrap();
    let mut cfg = config();
    cfg.graph.sessions = false;
    cfg.agents[0].command = vec!["echo".into(), "Here is the review explanation".into()];
    magi::daemon::write_status_to(
        &home.path().join("daemon.json"),
        &magi::daemon::Status::new(),
    )
    .unwrap();
    store
        .update(&q.id, |q| {
            require(&store, q, Some(&cfg))?;
            q.say("Explain the review")
        })
        .unwrap();
    let mut deputies = magi::deputy::Deputies::new(
        store.clone(),
        home.path().to_path_buf(),
        Some(cfg),
        home.path().to_path_buf(),
        1,
        std::sync::Arc::new(|| false),
    );
    deputies.tick(jiff::Timestamp::now());
    for _ in 0..200 {
        let fresh = store.get(&q.id).unwrap();
        if fresh.thread.iter().any(|t| t.who == magi::ask::Who::Agent) {
            assert_eq!(fresh.thread[0].body, "Explain the review");
            assert_eq!(fresh.delivered_turns, fresh.thread.len());
            assert!(fresh.status.open());
            assert!(fresh.answer.is_none());
            deputies.drain().await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    deputies.drain().await;
    panic!("the restart did not deliver the accepted message");
}

#[test]
fn ordinary_waiter_admission_uses_the_actual_session_target() {
    let home = tempfile::tempdir().unwrap();
    let store = Questions::at(home.path().join("questions"));
    let cfg = config();
    let mut run = magi::run::RunState::new(
        home.path().to_path_buf(),
        "main".into(),
        "0".repeat(40),
        "work".into(),
        cfg.clone(),
    );
    run.status = magi::run::RunStatus::Implementing;
    let mut seat = magi::agent::SeatState::new("impl-A", "listener", 1);
    seat.turns = 1;
    run.seats.insert(seat.key.clone(), seat);
    run.save_under(home.path()).unwrap();
    let mut q = Question::new(
        run.id.clone(),
        "implement".into(),
        "impl-A".into(),
        "Which?".into(),
        String::new(),
        vec![],
    );
    q.cwd = Some(home.path().to_string_lossy().into_owned());
    q.answer_timeout = 3600;
    store.put(&mut q).unwrap();
    magi::daemon::write_status_to(
        &home.path().join("daemon.json"),
        &magi::daemon::Status::new(),
    )
    .unwrap();
    assert_eq!(availability(&store, &q, Some(&cfg)).state, "waiting");
    run.seats.get_mut("impl-A").unwrap().turns = 0;
    run.save_under(home.path()).unwrap();
    assert_eq!(
        availability(&store, &q, Some(&cfg)).reason,
        "recipient_unavailable"
    );
    store.beat(&q.id, WaiterKind::Asker);
    assert_eq!(availability(&store, &q, Some(&cfg)).state, "live");
}
