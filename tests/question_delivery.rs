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

fn publish(home: &std::path::Path, cfg: &Config) {
    let mut status = magi::daemon::Status::new();
    status.question_recovery = Some(magi::question_delivery::Recovery::new(
        Some(cfg),
        cfg.daemon.max_deputies,
    ));
    magi::daemon::write_status_to(&home.join("daemon.json"), &status).unwrap();
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
    publish(home.path(), &cfg);
    assert_eq!(availability(&store, &q, Some(&cfg)).state, "waiting");
    store
        .update(&q.id, |q| {
            require(&store, q, Some(&cfg))?;
            q.say("Explain the review")
        })
        .unwrap();
    assert_eq!(store.get(&q.id).unwrap().thread.len(), 1);
    cfg.daemon.max_deputies = 0;
    assert_eq!(
        availability(&store, &q, Some(&cfg)).state,
        "waiting",
        "editing config does not disable the running deputy loop"
    );
    publish(home.path(), &cfg);
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
    publish(home.path(), &cfg);
    let mut status = magi::daemon::Status::new();
    status.updated_at -= std::time::Duration::from_secs(3600);
    status.question_recovery = Some(magi::question_delivery::Recovery::new(
        Some(&cfg),
        cfg.daemon.max_deputies,
    ));
    magi::daemon::write_status_to(&home.path().join("daemon.json"), &status).unwrap();
    let mut progress = magi::updater::Progress::new("old".into(), "new".into());
    progress.advance(magi::updater::Stage::Restarting);
    magi::updater::write_progress(home.path(), &progress).unwrap();
    let lease = magi::updater::LeaseGuard::enter(home.path(), None);
    assert_eq!(availability(&store, &q, Some(&cfg)).state, "waiting");
    let mut edited = cfg.clone();
    edited.daemon.max_deputies = 0;
    assert_eq!(
        availability(&store, &q, Some(&edited)).reason,
        "daemon_unavailable"
    );
    drop(lease);
    assert_eq!(
        availability(&store, &q, Some(&cfg)).reason,
        "daemon_unavailable"
    );
    publish(home.path(), &cfg);
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
    publish(home.path(), &cfg);
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
    publish(home.path(), &cfg);
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

#[tokio::test]
async fn recovery_uses_running_capacity_instead_of_edited_configuration() {
    let home = tempfile::tempdir().unwrap();
    let store = Questions::at(home.path().join("questions"));
    let q = question(&store);
    let mut startup = config();
    startup.daemon.max_deputies = 0;
    publish(home.path(), &startup);
    let mut edited = startup.clone();
    edited.daemon.max_deputies = 1;
    let before = std::fs::read(store.path_of(&q.id)).unwrap();
    let error = store
        .update(&q.id, |q| {
            require(&store, q, Some(&edited))?;
            q.say("Please explain")
        })
        .unwrap_err();
    assert!(error.to_string().contains("deputy_disabled"));
    assert_eq!(std::fs::read(store.path_of(&q.id)).unwrap(), before);
    let mut deputies = magi::deputy::Deputies::new(
        store.clone(),
        home.path().to_path_buf(),
        Some(startup),
        home.path().to_path_buf(),
        0,
        std::sync::Arc::new(|| false),
    );
    deputies.tick(jiff::Timestamp::now());
    deputies.drain().await;
    assert_eq!(store.get(&q.id).unwrap().deputy.unwrap().starts, 0);

    // A restarted daemon publishes the new capability before admitting text.
    publish(home.path(), &edited);
    assert_eq!(availability(&store, &q, None).state, "waiting");
    // A legacy heartbeat without a capability snapshot cannot prove recovery.
    magi::daemon::write_status_to(
        &home.path().join("daemon.json"),
        &magi::daemon::Status::new(),
    )
    .unwrap();
    assert_eq!(
        availability(&store, &q, Some(&edited)).reason,
        "daemon_unavailable"
    );
}

#[test]
fn terminal_text_answer_remains_a_decision_without_a_recipient() {
    let home = tempfile::tempdir().unwrap();
    let store = Questions::at(home.path().join("questions"));
    let mut q = Question::new(
        "task".into(),
        "ask".into(),
        "operator".into(),
        "Which name?".into(),
        String::new(),
        vec![],
    );
    store.put(&mut q).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["answer", &q.id, "--reply", "chosen name"])
        .env("MAGI_HOME", home.path())
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .env_remove("MAGI_SEAT")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved = store.get(&q.id).unwrap();
    assert_eq!(saved.resolution().as_deref(), Some("chosen name"));
    assert!(!saved.status.open());
}

#[test]
fn release_watch_deputy_thread_with_empty_run_is_an_agent_reply() {
    for live_lease in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let store = Questions::at(home.path().join("questions"));
        let mut q = Question::new(
            String::new(),
            magi::bump::NOTICE_NODE.into(),
            "release-watch".into(),
            "Continue watching?".into(),
            String::new(),
            vec!["hold".into()],
        );
        q.answer_timeout = 60;
        q.deputy = Some(Deputy::new("release context".into()));
        q.deputy.as_mut().unwrap().starts = magi::deputy::MAX_STARTS;
        q.say("Explain the release status").unwrap();
        q.delivered_turns = q.thread.len(); // Already read by the replying deputy.
        store.put(&mut q).unwrap();
        if live_lease {
            store.beat(&q.id, WaiterKind::Deputy);
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_magi"))
            .args([
                "ask",
                "--thread",
                &q.id,
                "--summary",
                "Release explanation",
                "--choice",
                "hold",
            ])
            .current_dir(home.path())
            .env("MAGI_HOME", home.path())
            .env(Config::CONFIG_DIR_ENV, "")
            .env("MAGI_RUN", "")
            .env("MAGI_NODE", magi::deputy::NODE)
            .env("MAGI_SEAT", magi::ask::deputy_seat_key(&q.id))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut answered = false;
        let mut timed_out = false;
        loop {
            if !answered
                && let Ok(fresh) = store.get(&q.id)
                && fresh
                    .thread
                    .last()
                    .is_some_and(|t| t.who == magi::ask::Who::Agent)
            {
                answered = store
                    .update(&q.id, |q| {
                        q.answer(magi::ask::Answer::Choice("hold".into()))
                    })
                    .is_ok();
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                timed_out = true;
                child.kill().unwrap();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!timed_out, "thread command did not finish");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let saved = store.get(&q.id).unwrap();
        assert!(answered, "the command must wait for the owner's decision");
        assert_eq!(saved.thread.len(), 2);
        assert_eq!(saved.thread[0].body, "Explain the release status");
        assert_eq!(saved.thread[1].who, magi::ask::Who::Agent);
        assert_eq!(saved.thread[1].body, "Release explanation");
        assert_eq!(saved.delivered_turns, 2);
        assert_eq!(saved.resolution().as_deref(), Some("hold"));
    }
}
