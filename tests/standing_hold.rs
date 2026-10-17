//! A real standing-chat turn grants question-store access and invokes the
//! actual answer CLI, including when no consultation was sent to this chat.

use std::collections::BTreeMap;

use magi::ask::{Question, Questions};
use magi::config::{AgentKind, AgentSpec, Config};
use magi::talk::{self, Talks};

#[tokio::test]
async fn standing_turn_grants_explicit_batch_decisions_and_answers_through_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    magi::run::set_home(tmp.path().to_path_buf());
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let talks = Talks::at(tmp.path().join("talks"));
    let questions = Questions::at(tmp.path().join("questions"));
    // Nested sandbox application can be forbidden even on macOS. Keep the
    // real CLI coverage there; exercise the write boundary when available.
    let sandbox_available = if cfg!(target_os = "macos") {
        let probe = std::process::Command::new("sandbox-exec")
            .args(["-p", "(version 1) (allow default)", "/usr/bin/true"])
            .output()
            .unwrap();
        if !probe.status.success() {
            eprintln!(
                "sandbox boundary checks unavailable: {}",
                String::from_utf8_lossy(&probe.stderr)
            );
        }
        probe.status.success()
    } else {
        false
    };
    let script = tmp.path().join("answer.sh");
    std::fs::write(
        &script,
        "cat >/dev/null\n\
         test \"$MAGI_ALLOW_WRITE\" = \"$EXPECTED_WRITE\" || exit 31\n\
         if test \"$DO_ANSWER\" = 1; then\n\
           if test -n \"$PROFILE\"; then\n\
             if sandbox-exec -f \"$BASE_PROFILE\" \"$ANSWER_BIN\" answer \"$QUESTION\" --reply \"$CHOICE\" --quote \"$OWNER_WORDS\" >\"$REJECT_LOG\" 2>&1; then exit 33; fi\n\
             cmp \"$QUESTION_BEFORE\" \"$QUESTION_PATH\" || exit 34\n\
             cmp \"$RUN_BEFORE\" \"$RUN_PATH\" || exit 35\n\
             if sandbox-exec -f \"$PROFILE\" sh -c 'echo forbidden > \"$1\"' sh \"$OUTSIDE\" 2>/dev/null; then exit 36; fi\n\
             sandbox-exec -f \"$PROFILE\" \"$ANSWER_BIN\" answer \"$QUESTION\" --reply \"$CHOICE\" --quote \"$OWNER_WORDS\" || exit 37\n\
           else\n\
             \"$ANSWER_BIN\" answer \"$QUESTION\" --reply \"$CHOICE\" --quote \"$OWNER_WORDS\" || exit 32\n\
           fi\n\
         fi\n\
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
            env: BTreeMap::from([
                ("ANSWER_BIN".into(), env!("CARGO_BIN_EXE_magi").into()),
                (
                    "MAGI_HOME".into(),
                    tmp.path().to_string_lossy().into_owned(),
                ),
                (
                    "MAGI_CONFIG_DIR".into(),
                    tmp.path().join("config").to_string_lossy().into_owned(),
                ),
            ]),
            prompt_delivery: None,
        }],
        ..Config::default()
    };
    assert!(!cfg.talk.allow_write);
    for choice in [
        "hold",
        magi::land::REPAIR,
        "retry",
        magi::merge_decision::DISMISS,
    ] {
        for destination in ["elsewhere", "none", "same"] {
            for authorized in [false, true] {
                // The same-chat unapproved turn already has its existing write
                // grant, but still must not manufacture an answer.
                let expected_write = authorized || destination == "same";
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
                    q.run = q.id.clone();
                    let mut state = magi::run::RunState::new(
                        repo.clone(),
                        "main".into(),
                        "head".into(),
                        "Original task".into(),
                        Config::default(),
                    );
                    state.id = q.run.clone();
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
                let words: String = if authorized {
                    t.turns.push(talk::Turn {
                        sender: Some(talk::Sender::Persona),
                        who: talk::Who::Agent,
                        body: format!("{}: {choice}", q.id),
                        at: jiff::Timestamp::now(),
                        breaks: None,
                        messages: vec![],
                        posted_by: None,
                        attachments: vec![],
                        usage: None,
                    });
                    talks.put(&mut t).unwrap();
                    "おっけーそれぞれ回答してー".into()
                } else {
                    "thanks".into()
                };
                talk::record(&mut t, &talks, &words, vec![]).unwrap();
                // Use the complete grant passed by the standing turn to its
                // invocation, rather than independently granting retirement access.
                let roots = magi::consult::answer_write_roots(&questions, &talks, &t);
                let mut expected_roots = if expected_write {
                    vec![questions.root().to_path_buf(), talks.answer_lock_root()]
                } else {
                    vec![]
                };
                if choice == magi::merge_decision::DISMISS && authorized {
                    expected_roots.extend([
                        tmp.path().join("queue"),
                        tmp.path().join("runs").join(&q.run),
                    ]);
                }
                expected_roots.sort();
                assert_eq!(
                    roots, expected_roots,
                    "only an authorized dismissal widens execution access"
                );
                let env = &mut cfg.agents[0].env;
                env.insert("PROFILE".into(), String::new());
                if sandbox_available && choice == magi::merge_decision::DISMISS && authorized {
                    let home = tmp.path().canonicalize().unwrap();
                    let base = [questions.root().to_path_buf(), talks.answer_lock_root()];
                    let profile = |extra: &[std::path::PathBuf]| {
                        let allowed = extra
                            .iter()
                            .map(|path| {
                                // Seatbelt matches resolved paths, including
                                // macOS's /var -> /private/var temp alias.
                                let path = home.join(path.strip_prefix(tmp.path()).unwrap());
                                format!(
                                    "(subpath {})",
                                    serde_json::to_string(&path.to_string_lossy()).unwrap()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!(
                            "(version 1) (allow default) (deny file-write* (subpath {})) (allow file-write* {allowed})",
                            serde_json::to_string(&home.to_string_lossy()).unwrap()
                        )
                    };
                    let base_profile = tmp.path().join("base.sb");
                    let full_profile = tmp.path().join("answer.sb");
                    std::fs::write(&base_profile, profile(&base)).unwrap();
                    std::fs::write(&full_profile, profile(&roots)).unwrap();
                    let question_before = tmp.path().join("question-before");
                    let run_path = tmp.path().join("runs").join(&q.run).join("run.json");
                    let run_before = tmp.path().join("run-before");
                    std::fs::copy(questions.path_of(&q.id), &question_before).unwrap();
                    std::fs::copy(&run_path, &run_before).unwrap();
                    for (key, path) in [
                        ("PROFILE", full_profile),
                        ("BASE_PROFILE", base_profile),
                        ("QUESTION_BEFORE", question_before),
                        ("QUESTION_PATH", questions.path_of(&q.id)),
                        ("RUN_BEFORE", run_before),
                        ("RUN_PATH", run_path),
                        ("OUTSIDE", tmp.path().join("unrelated-data")),
                        ("REJECT_LOG", tmp.path().join("rejected.log")),
                    ] {
                        env.insert(key.into(), path.to_string_lossy().into_owned());
                    }
                }

                env.insert(
                    "EXPECTED_WRITE".into(),
                    if expected_write { "1" } else { "0" }.into(),
                );
                env.insert(
                    "DO_ANSWER".into(),
                    if authorized { "1" } else { "0" }.into(),
                );
                env.insert("CHOICE".into(), choice.into());
                env.insert("QUESTION".into(), q.id.clone());
                env.insert("OWNER_WORDS".into(), words.clone());
                let lease = talks.claim_turn(&t.id).unwrap().unwrap();
                talk::respond(&lease, &mut t, &talks, &cfg, &words)
                    .await
                    .unwrap_or_else(|error| {
                        let stderr =
                            std::fs::read_to_string(talks.artifacts_of(&t.id).join("turn-1.err"))
                                .unwrap_or_default();
                        panic!("{choice} ({destination}): {error}; stderr: {stderr}");
                    });
                let saved = questions.get(&q.id).unwrap();
                assert_eq!(
                    saved.resolution().as_deref(),
                    authorized.then_some(choice),
                    "{:?}",
                    t.turns.last()
                );
                if authorized {
                    assert_eq!(saved.answer_authority.unwrap().talk, t.id);
                }
                if let Some(task_id) = owner_task {
                    let state = magi::run::RunState::load_under(&q.run, tmp.path()).unwrap();
                    let task = magi::queue::Queue::at(tmp.path().join("queue"))
                        .get(&task_id)
                        .unwrap();
                    if authorized {
                        assert_eq!(
                            state.merge_decision.unwrap().choice,
                            magi::merge_decision::DISMISS
                        );
                        assert_eq!(state.status, magi::run::RunStatus::Superseded);
                        assert_eq!(task.status, magi::queue::TaskStatus::Done);
                    } else {
                        assert!(state.merge_decision.is_none());
                        assert_eq!(task.status, magi::queue::TaskStatus::Parked);
                    }
                    assert_eq!(state.pr.unwrap().state, "open");
                }
            }
        }
    }
}
