//! A real standing-chat turn carries out the owner's request through the
//! actual CLI: no quote, no consent phrase and no question-specific grant,
//! whichever conversation the question was handed to. The read-only turn's
//! answer goes over its inherited control bridge.

mod common;

use magi::ask::{Question, Questions};
use magi::config::{AgentKind, AgentSpec, Config};
use magi::talk::{self, Talks};

common::e2e! {
async fn standing_turn_answers_owner_requests_through_the_cli_without_quotes() {
    let _home = common::home_lock().await;
    let tmp = tempfile::tempdir().unwrap();
    magi::run::set_home(tmp.path().to_path_buf());
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let talks = Talks::at(tmp.path().join("talks"));
    let questions = Questions::at(tmp.path().join("questions"));
    let script = tmp.path().join("answer.sh");
    // The CLI inside the turn reads a decoy home holding only copies of the
    // question and the conversation: a direct store write would land in the
    // decoy, so the real store saving the answer proves the bridge carried it.
    let decoy = tmp.path().join("decoy");
    std::fs::write(
        &script,
        "cat >/dev/null\n\
         # Title generation reuses the agent on its own seat; only the chat\n\
         # turn carries out the owner's request.\n\
         [ \"$MAGI_NODE\" = talk-title ] && exit 0\n\
         test \"$MAGI_ALLOW_WRITE\" = 0 || exit 31\n\
         test -n \"$MAGI_CHAT_CONTROL_FD$MAGI_CHAT_CONTROL_PIPE\" || exit 33\n\
         MAGI_HOME=\"$DECOY\" \"$ANSWER_BIN\" answer \"$QUESTION\" --reply \"$CHOICE\" || exit 32\n\
         echo 'Turn finished.'\n",
    )
    .unwrap();
    let mut cfg = Config {
        agents: vec![AgentSpec {
            id: "mock".into(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["sh".into(), script.to_string_lossy().into_owned()],
            extra_args: vec![],
            env: {
                let mut env = common::magi_cli_env(tmp.path(), "ANSWER_BIN");
                env.insert("DECOY".into(), decoy.to_string_lossy().into_owned());
                env
            },
            prompt_delivery: None,
            max_turn_secs: None,
        }],
        ..Config::default()
    };
    // Consultation delivery, not task-chat routing: routed decisions are
    // covered separately below and must not replace this path.
    cfg.graph.question_routing = false;
    std::fs::write(repo.join("magi.toml"), toml::to_string(&cfg).unwrap()).unwrap();
    assert!(!Config::discover(&repo, None).unwrap().0.graph.question_routing);
    assert!(!cfg.talk.allow_write);
    for choice in [
        "hold",
        magi::land::REPAIR,
        "retry",
        magi::merge_decision::DISMISS,
    ] {
        for destination in ["elsewhere", "none", "same"] {
            let mut q = Question::new(
                "20260902-000000-beef".into(),
                magi::land::APPROVAL_NODE.into(),
                "land".into(),
                "Merge?".into(),
                "Review this change".into(),
                vec![
                    "merge".into(),
                    "hold".into(),
                    magi::land::REPAIR.into(),
                    "retry".into(),
                    magi::merge_decision::DISMISS.into(),
                ],
            );
            let mut owner_task = None;
            if choice == magi::merge_decision::DISMISS {
                let mut state = magi::run::RunState::new(
                    repo.clone(),
                    "main".into(),
                    "head".into(),
                    "Original task".into(),
                    Config::default(),
                );
                // Question IDs and run IDs have independent formats.
                // Exercise dismissal of a real run, rather than using a
                // question ID as a synthetic run directory name.
                q.run = state.id.clone();
                assert!(magi::run::is_run_id(&q.run));
                assert_ne!(q.run, q.id);
                state.status = magi::run::RunStatus::Landing;
                state.parked = true;
                state.driver_exited = true;
                state.land_approval = Some(magi::run::LandApproval {
                    question: q.id.clone(),
                    head: "head".into(),
                });
                state.pr = Some(magi::run::PrRecord {
                    url: "https://example.test/pull/1".into(),
                    number: 1,
                    state: "open".into(),
                    checks: "green".into(),
                    round: 1,
                    rounds: 2,
                    red_at_merge: vec![],
                });
                state.save_under(tmp.path()).unwrap();
                q.merge_target = Some(magi::merge_decision::Target {
                    pr: "https://example.test/pull/1".into(),
                    head: "head".into(),
                    language: "en".into(),
                });
                let queue = magi::queue::Queue::at(tmp.path().join("queue"));
                let mut task = magi::queue::Task::new(
                    "Original task".into(),
                    "Original instruction".into(),
                    repo.clone(),
                    magi::queue::Source::Human,
                );
                task.runs.push(q.run.clone());
                task.status = magi::queue::TaskStatus::Parked;
                queue.put(&mut task).unwrap();
                owner_task = Some(task.id);
            }
            questions.put(&mut q).unwrap();
            assert!(
                q.routed_task.is_none(),
                "fixture must stay on the consultation path"
            );
            let mut t = talk::begin(&talks, &cfg, repo.clone(), Some("mock")).unwrap();
            if destination != "none" {
                let origin = if destination == "same" {
                    t.clone()
                } else {
                    talk::begin(&talks, &cfg, repo.clone(), Some("mock")).unwrap()
                };
                magi::consult::begin(&questions, &talks, &q, &origin).unwrap();
                if destination == "same" {
                    t = talks.get(&t.id).unwrap();
                    talk::drain(&mut t, &talks).unwrap();
                }
            }
            // Plain words: nothing quotes the question, the choice or a phrase.
            let words = "それでお願い";
            talk::record(&mut t, &talks, words, vec![]).unwrap();
            let env = &mut cfg.agents[0].env;
            env.insert("CHOICE".into(), choice.into());
            env.insert("QUESTION".into(), q.id.clone());
            let _ = std::fs::remove_dir_all(&decoy);
            let decoy_questions = Questions::at(decoy.join("questions"));
            decoy_questions
                .put(&mut questions.get(&q.id).unwrap())
                .unwrap();
            std::fs::create_dir_all(decoy.join("talks")).unwrap();
            std::fs::copy(
                talks.path_of(&t.id),
                Talks::at(decoy.join("talks")).path_of(&t.id),
            )
            .unwrap();
            let lease = talks.claim_turn(&t.id).unwrap().unwrap();
            talk::respond(&lease, &mut t, &talks, &cfg, words)
                .await
                .unwrap_or_else(|error| {
                    panic!("{choice} ({destination}): {error}; last: {:?}", t.turns.last());
                });
            let saved = questions.get(&q.id).unwrap();
            assert_eq!(
                saved.resolution().as_deref(),
                Some(choice),
                "{choice} ({destination}): {:?}",
                t.turns.last()
            );
            assert!(
                decoy_questions.get(&q.id).unwrap().status.open(),
                "{choice} ({destination}): the answer must not be written by the sandboxed CLI"
            );
            // A handed-over or open question changes nothing about the turn's
            // access: read-only, sandbox kept, and the standing chat's store
            // grant (effective only when a turn may write).
            let accesses: Vec<serde_json::Value> =
                std::fs::read_dir(talks.artifacts_of(&t.id))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.to_string_lossy().ends_with(".access.json"))
                    .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
                    .collect();
            assert_eq!(accesses.len(), 1, "{choice} ({destination})");
            assert_eq!(accesses[0]["allow_write"], false, "{choice} ({destination})");
            assert_eq!(accesses[0]["unsandboxed"], false);
            assert_eq!(
                accesses[0]["writable_roots"],
                serde_json::json!([tmp.path().join("questions"), tmp.path().join("queue")]),
                "{choice} ({destination})"
            );
            if let Some(task_id) = owner_task {
                let state = magi::run::RunState::load_under(&q.run, tmp.path()).unwrap();
                let task = magi::queue::Queue::at(tmp.path().join("queue"))
                    .get(&task_id)
                    .unwrap();
                assert_eq!(
                    state.merge_decision.unwrap().choice,
                    magi::merge_decision::DISMISS
                );
                assert_eq!(state.status, magi::run::RunStatus::Superseded);
                assert_eq!(task.status, magi::queue::TaskStatus::Done);
                assert_eq!(state.pr.unwrap().state, "open");
            }
        }
    }
}
}

common::e2e! {
async fn routed_merge_is_recorded_by_its_own_standing_turn_from_plain_words() {
    let _home = common::home_lock().await;
    let tmp = tempfile::tempdir().unwrap();
    magi::run::set_home(tmp.path().to_path_buf());
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let script = tmp.path().join("decide.sh");
    std::fs::write(
        &script,
        "cat >/dev/null\n\
         \"$ANSWER_BIN\" task control decide \"$QUESTION\" merge || exit 32\n\
         echo 'Decision recorded.'\n",
    )
    .unwrap();
    let cfg = Config {
        agents: vec![AgentSpec {
            id: "mock".into(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["sh".into(), script.to_string_lossy().into_owned()],
            extra_args: vec![],
            env: common::magi_cli_env(tmp.path(), "ANSWER_BIN"),
            prompt_delivery: None,
            max_turn_secs: None,
        }],
        ..Config::default()
    };
    assert!(cfg.graph.question_routing);
    std::fs::write(repo.join("magi.toml"), toml::to_string(&cfg).unwrap()).unwrap();
    assert!(Config::discover(&repo, None).unwrap().0.graph.question_routing);
    let talks = Talks::at(tmp.path().join("talks"));
    let questions = Questions::at(tmp.path().join("questions"));
    let queue = magi::queue::Queue::at(tmp.path().join("queue"));
    let mut chat = talk::begin(&talks, &cfg, repo.clone(), Some("mock")).unwrap();
    let other = talk::begin(&talks, &cfg, repo.clone(), Some("mock")).unwrap();
    let mut task = magi::queue::Task::new(
        "Original task".into(),
        "Original instruction".into(),
        repo,
        magi::queue::Source::Human,
    );
    task.origin_chat = Some(chat.id.clone());
    task.runs.push("20260902-000000-beef".into());
    queue.put(&mut task).unwrap();
    let mut q = Question::new(
        task.runs[0].clone(),
        magi::land::APPROVAL_NODE.into(),
        "land".into(),
        "Merge?".into(),
        "Review this change".into(),
        vec!["merge".into(), "hold".into()],
    );
    q.merge_target = Some(magi::merge_decision::Target {
        pr: "https://example.test/pull/1".into(),
        head: "head".into(),
        language: "en".into(),
    });
    questions.put(&mut q).unwrap();
    assert_eq!(q.routed_task.as_deref(), Some(task.id.as_str()));
    // Routing, not authority: a routed decision is delivered by its own chat.
    assert!(magi::consult::begin(&questions, &talks, &q, &other).is_err());
    assert!(questions.get(&q.id).unwrap().consult.is_none());
    assert!(talks.get(&other.id).unwrap().pending.is_empty());
    magi::task_chat::sweep(&queue, &talks, &questions, tmp.path());
    let delivered = questions.get(&q.id).unwrap();
    assert_eq!(delivered.routed_chat.as_deref(), Some(chat.id.as_str()));
    chat = talks.get(&chat.id).unwrap();
    assert_eq!(chat.pending_messages.len(), 1);
    talk::drain(&mut chat, &talks).unwrap();
    // The owner's own words are the approval: no head, quote or button.
    talk::record(&mut chat, &talks, "マージして", vec![]).unwrap();
    let mut cfg = cfg;
    cfg.agents[0].env.insert("QUESTION".into(), q.id.clone());
    let lease = talks.claim_turn(&chat.id).unwrap().unwrap();
    talk::respond(&lease, &mut chat, &talks, &cfg, "マージして")
        .await
        .unwrap();
    let saved = questions.get(&q.id).unwrap();
    assert_eq!(saved.resolution().as_deref(), Some("merge"), "{:?}", chat.turns.last());
    assert_eq!(saved.merge_target, delivered.merge_target);
    assert_eq!(saved.routed_chat, delivered.routed_chat);
    assert_eq!(
        talks.list().len(),
        2,
        "delivery must not create another seat's chat"
    );
}
}
