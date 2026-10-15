//! The chat approval gate, through the real `magi answer` binary.
//!
//! `consult::validate_answer` is unit-tested directly, but the gate only takes
//! effect inside `answer_cmd`'s `agent_env()` branch. These tests spawn the
//! binary with `MAGI_RUN` / `MAGI_NODE` set the way an agent seat gets them, so
//! dropping the env lookup, the node check, the `--quote` hand-over or the
//! validate call fails here and not only in review.

use std::collections::BTreeMap;
use std::process::{Command, Output};

use jiff::Timestamp;
use magi::ask::{QuestionStatus, Questions};
use magi::config::{AgentKind, AgentSpec, Config};
use magi::talk::{self, Talks};

struct Fixture {
    tmp: tempfile::TempDir,
    questions: Questions,
    qid: String,
    talk_id: String,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let talks = Talks::at(tmp.path().join("talks"));
    let questions = Questions::at(tmp.path().join("questions"));
    let cfg = Config {
        agents: vec![AgentSpec {
            id: "mock".to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["true".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }],
        ..Config::default()
    };
    let mut t = talk::begin(&talks, &cfg, tmp.path().to_path_buf(), Some("mock")).expect("talk");

    let mut q = magi::ask::Question::new(
        "20260902-000000-beef".to_owned(),
        magi::land::APPROVAL_NODE.to_owned(),
        "land".to_owned(),
        "Merge the pull request?".to_owned(),
        "All checks are green.".to_owned(),
        vec![magi::land::APPROVE.to_owned(), magi::land::HOLD.to_owned()],
    );
    questions.put(&mut q).expect("put question");
    assert!(magi::consult::begin(&questions, &talks, &q, &t).expect("begin"));

    // What the drain does: the queued hand-over becomes an operator turn, and
    // the owner's own reply follows it. `pending` ends up empty, so the gate
    // must read the stored turns.
    let owner = |body: &str| talk::Turn {
        sender: None,
        breaks: Some(Vec::new()),
        messages: Vec::new(),
        posted_by: None,
        who: talk::Who::Operator,
        body: body.into(),
        at: Timestamp::now(),
        attachments: Vec::new(),
        usage: None,
    };
    let queued = talks.get(&t.id).expect("talk").pending;
    t = talks.get(&t.id).expect("talk");
    t.turns.push(owner(&queued));
    t.turns.push(owner("merge it now"));
    t.pending.clear();
    t.pending_messages.clear();
    t.pending_breaks = None;
    talks.put(&mut t).expect("put talk");

    Fixture {
        qid: q.id.clone(),
        talk_id: t.id.clone(),
        questions,
        tmp,
    }
}

fn answer(f: &Fixture, run: &str, quote: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_magi"));
    cmd.arg("answer")
        .arg(&f.qid)
        .args(["--reply", magi::land::APPROVE])
        .env("MAGI_HOME", f.tmp.path())
        .env("MAGI_CONFIG_DIR", f.tmp.path().join("config"))
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .env("MAGI_RUN", run)
        .env("MAGI_NODE", magi::queue::CHAT_NODE);
    if let Some(q) = quote {
        cmd.args(["--quote", q]);
    }
    cmd.output().expect("spawn magi")
}

fn assert_untouched(f: &Fixture, out: &Output) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "should be refused: {err}");
    let q = f.questions.get(&f.qid).expect("question");
    assert!(q.status.open(), "question must stay open");
    assert!(q.answer.is_none(), "nothing may be written");
}

#[test]
fn legacy_consult_repair_requires_exact_owner_choice_and_keeps_original_handoff() {
    let f = fixture();
    let q = f.questions.get(&f.qid).unwrap();
    let mut state = magi::run::RunState::new(
        f.tmp.path().into(),
        "main".into(),
        "reviewed-head".into(),
        "Original instruction".into(),
        Config::default(),
    );
    state.id = q.run.clone();
    state.status = magi::run::RunStatus::Landing;
    state.parked = true;
    state.land_approval = Some(magi::run::LandApproval {
        question: q.id.clone(),
        head: "reviewed-head".into(),
    });
    state.reviews.push(serde_json::from_value(serde_json::json!({
        "round": 1, "head": "reviewed-head", "reviews": [{"reviewer": 1, "agent": "mock", "vote": "reject", "summary": "Unresolved review"}],
        "blocking": 0, "answered": 1, "expected": 1
    })).unwrap());
    state.save_under(f.tmp.path()).unwrap();
    let talks = Talks::at(f.tmp.path().join("talks"));
    let mut t = talks.get(&f.talk_id).unwrap();
    let old_handoff = t.turns[0].body.clone();
    assert!(!old_handoff.contains(magi::land::REPAIR));
    let submit = || {
        Command::new(env!("CARGO_BIN_EXE_magi"))
            .args([
                "answer",
                &f.qid,
                "--reply",
                magi::land::REPAIR,
                "--quote",
                magi::land::REPAIR,
            ])
            .env("MAGI_HOME", f.tmp.path())
            .env("MAGI_CONFIG_DIR", f.tmp.path().join("config"))
            .env("MAGI_RUN", &f.talk_id)
            .env("MAGI_NODE", magi::queue::CHAT_NODE)
            .output()
            .unwrap()
    };
    t.pending = format!("maybe {}", magi::land::REPAIR);
    t.pending_breaks = Some(Vec::new());
    talks.put(&mut t).unwrap();
    assert_untouched(&f, &submit());
    t.pending = magi::land::REPAIR.into();
    talks.put(&mut t).unwrap();
    let out = submit();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let answered = f.questions.get(&f.qid).unwrap();
    assert_eq!(answered.resolution().as_deref(), Some(magi::land::REPAIR));
    assert_eq!(
        answered.chosen_action(),
        Some(&magi::ask::ChoiceAction::Resume { run: q.run })
    );
    assert_eq!(talks.get(&f.talk_id).unwrap().turns[0].body, old_handoff);
    let disk = magi::run::RunState::load_under(&state.id, f.tmp.path()).unwrap();
    assert!(disk.merge_repairs.is_empty());
    assert_eq!(disk.instruction, "Original instruction");
}

#[test]
fn a_chat_merge_without_a_quote_is_refused() {
    let f = fixture();
    let out = answer(&f, &f.talk_id, None);
    assert_untouched(&f, &out);
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        err.contains("quote"),
        "refusal should name the quote: {err}"
    );
}

#[test]
fn a_chat_merge_with_words_the_owner_never_wrote_is_refused() {
    let f = fixture();
    let out = answer(&f, &f.talk_id, Some("ship it immediately"));
    assert_untouched(&f, &out);
}

#[test]
fn a_chat_merge_from_a_different_chat_is_refused() {
    let f = fixture();
    let out = answer(&f, "20260902-000000-dead", Some("merge it now"));
    assert_untouched(&f, &out);
}

#[test]
fn a_chat_merge_quoting_the_owner_is_accepted() {
    let f = fixture();
    let out = answer(&f, &f.talk_id, Some("merge it now"));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "should be accepted: {err}");
    let q = f.questions.get(&f.qid).expect("question");
    assert_eq!(q.status, QuestionStatus::Answered);
}

#[test]
fn ask_chat_busy_exits_unsuccessfully_without_changing_state() {
    let f = fixture();
    let talks = Talks::at(f.tmp.path().join("talks"));
    let mut q = f.questions.get(&f.qid).expect("question");
    q.consult = None;
    f.questions.put(&mut q).expect("unconsulted question");
    let mut task = magi::queue::Task::new(
        "consult-task".to_owned(),
        "Do it".to_owned(),
        f.tmp.path().to_path_buf(),
        magi::queue::Source::Agent {
            run: f.talk_id.clone(),
            node: magi::queue::CHAT_NODE.to_owned(),
        },
    );
    task.start(q.run.clone());
    magi::queue::Queue::at(f.tmp.path().join("queue"))
        .put(&mut task)
        .expect("chat task");
    std::fs::write(
        f.tmp.path().join("magi.toml"),
        "[[agents]]\nid = \"mock\"\nkind = \"command\"\ncommand = [\"true\"]\n",
    )
    .expect("isolated config");
    let question_before = std::fs::read(f.questions.path_of(&q.id)).unwrap();
    let talk_before = std::fs::read(talks.path_of(&f.talk_id)).unwrap();
    let _lease = talks
        .claim_turn(&f.talk_id)
        .unwrap()
        .expect("held elsewhere");
    let out = Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["answer", &q.id, "--ask-chat"])
        .env("MAGI_HOME", f.tmp.path())
        .env("MAGI_CONFIG_DIR", f.tmp.path().join("config"))
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .output()
        .expect("run CLI starter");
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("a turn is already running"), "{error}");
    assert_eq!(
        std::fs::read(f.questions.path_of(&q.id)).unwrap(),
        question_before
    );
    assert_eq!(
        std::fs::read(talks.path_of(&f.talk_id)).unwrap(),
        talk_before
    );
}

// Persist through the real owner-send entry point, preserving sender and send
// boundaries rather than manufacturing an owner-looking assistant paragraph.
fn standing_owner(f: &Fixture, words: &str, recommendations: Option<&str>) -> String {
    let talks = Talks::at(f.tmp.path().join("talks"));
    let mut t = talks.get(&f.talk_id).unwrap();
    t.id = "20260903-000000-cafe".into();
    t.turns.clear();
    if let Some(body) = recommendations {
        let mut reply = talks.get(&f.talk_id).unwrap().turns[0].clone();
        reply.sender = Some(talk::Sender::Persona);
        reply.who = talk::Who::Agent;
        reply.body = body.into();
        reply.at = Timestamp::now();
        t.turns.push(reply);
    }
    talks.put(&mut t).unwrap();
    talk::record(&mut t, &talks, words, vec![]).unwrap();
    t.id
}

fn hold(f: &Fixture, run: &str, words: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["answer", &f.qid, "--reply", "hold", "--quote", words])
        .env("MAGI_HOME", f.tmp.path())
        .env("MAGI_CONFIG_DIR", f.tmp.path().join("config"))
        .env("MAGI_RUN", run)
        .env("MAGI_NODE", magi::queue::CHAT_NODE)
        .output()
        .unwrap()
}

#[test]
fn explicitly_bound_cross_chat_and_unconsulted_holds_record_owner_evidence() {
    for unconsulted in [false, true] {
        let f = fixture();
        let original = f.questions.get(&f.qid).unwrap().consult;
        if unconsulted {
            f.questions
                .update(&f.qid, |q| {
                    q.consult = None;
                    Ok(())
                })
                .unwrap();
        }
        let words = format!("{}: hold", f.qid);
        let run = standing_owner(&f, &words, None);
        let talks = Talks::at(f.tmp.path().join("talks"));
        // A running original consultation must not block an explicit hold.
        let _active = talks.claim_turn(&f.talk_id).unwrap().unwrap();
        let out = hold(&f, &run, &words);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let q = f.questions.get(&f.qid).unwrap();
        assert_eq!(q.resolution().as_deref(), Some("hold"));
        assert!(q.chosen_action().is_none());
        let evidence = q.answer_authority.unwrap();
        assert_eq!(evidence.talk, run);
        assert_eq!(evidence.question, f.qid);
        assert_eq!(evidence.instruction, words);
        assert_eq!(evidence.choice, "hold");
        assert!(evidence.recommendation_turn.is_none());
        assert_eq!(q.consult, if unconsulted { None } else { original });
        assert!(!magi::consult::pending_consults(&f.questions, &f.talk_id));
        // The old seat cannot override the decision; neither can a repeat.
        assert!(
            !answer(&f, &f.talk_id, Some("merge it now"))
                .status
                .success()
        );
        assert!(!hold(&f, &run, &words).status.success());
    }
}

#[test]
fn explicit_batch_binds_each_preceding_recommendation_without_approving_merge() {
    let f = fixture();
    let mut second = f.questions.get(&f.qid).unwrap();
    second.id = "20260904-000000-abcd".into();
    second.consult = None;
    f.questions.put(&mut second).unwrap();
    let words = "ありがと！じゃあそれぞれ回答してー！";
    let recommendations = format!(
        "| Question | PR | Recommendation |\n| --- | --- | --- |\n| `{}` | #1 | `hold` |\n| `abcd` | #2 | `hold` |",
        f.qid
    );
    let run = standing_owner(&f, words, Some(&recommendations));
    assert!(!answer(&f, &run, Some(words)).status.success());
    let out = hold(&f, &run, words);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first = f.questions.get(&f.qid).unwrap().answer_authority.unwrap();
    assert_eq!(first.recommendation_turn, Some(0));
    assert!(first.recommendation_fingerprint.is_some());
    let other = Fixture {
        qid: second.id,
        ..f
    };
    assert!(hold(&other, &run, words).status.success());
}

#[test]
fn standing_holds_reject_missing_ambiguous_conditional_and_retracted_intent() {
    for words in [
        "thanks",
        "ありがとう",
        "maybe answer each as recommended",
        "answer each as recommended if checks pass",
        "answer each as recommended, actually wait",
        "それぞれ回答してもいい？",
        "それぞれ回答して、やっぱり待って",
        "\"answer each as recommended\"",
    ] {
        let f = fixture();
        let run = standing_owner(&f, words, Some(&format!("- `{}`: hold", f.qid)));
        assert_untouched(&f, &hold(&f, &run, words));
    }
    for recommendation in [
        "choices: merge / hold",
        "hold if checks fail",
        "hold / merge",
    ] {
        let f = fixture();
        let words = "answer each as recommended";
        let run = standing_owner(&f, words, Some(&format!("{}: {recommendation}", f.qid)));
        assert_untouched(&f, &hold(&f, &run, words));
    }
}

#[test]
fn standing_evidence_requires_real_sends_fresh_questions_and_current_instructions() {
    for mode in [
        "generated owner",
        "generated recommendation",
        "queued retraction",
        "old instruction",
        "old recommendation",
        "answered",
        "abandoned",
        "already replied",
        "wrong quote",
        "hold action",
    ] {
        let f = fixture();
        let words = "answer each as recommended";
        let run = standing_owner(&f, words, Some(&format!("{}: hold", f.qid)));
        let talks = Talks::at(f.tmp.path().join("talks"));
        let mut t = talks.get(&run).unwrap();
        match mode {
            "generated owner" => t.turns[1].messages[0].sender = Some(talk::Sender::Magi),
            "generated recommendation" => t.turns[0].sender = Some(talk::Sender::Magi),
            "queued retraction" => {
                talk::queue(&mut t, &talks, "wait", vec![]).unwrap();
            }
            "old instruction" => t.turns[1].at = Timestamp::UNIX_EPOCH,
            "old recommendation" => t.turns[0].at = Timestamp::UNIX_EPOCH,
            "already replied" => t.turns.push(t.turns[0].clone()),
            _ => {}
        }
        talks.put(&mut t).unwrap();
        f.questions
            .update(&f.qid, |q| {
                match mode {
                    "answered" => q.answer(magi::ask::Answer::Choice("hold".into()))?,
                    "abandoned" => q.abandon("gone"),
                    "hold action" => {
                        q.actions
                            .insert("hold".into(), magi::ask::ChoiceAction::Done);
                    }
                    _ => {}
                }
                Ok(())
            })
            .unwrap();
        let before = std::fs::read(f.questions.path_of(&f.qid)).unwrap();
        let quote = if mode == "wrong quote" {
            "never said"
        } else {
            words
        };
        assert!(!hold(&f, &run, quote).status.success(), "{mode}");
        assert_eq!(
            std::fs::read(f.questions.path_of(&f.qid)).unwrap(),
            before,
            "{mode}"
        );
    }
}

#[test]
fn ambiguous_short_ids_and_conflicting_recommendations_do_not_bind_a_batch() {
    for ambiguous_id in [false, true] {
        let f = fixture();
        let q = f.questions.get(&f.qid).unwrap();
        let mut duplicate = q.clone();
        duplicate.id = format!("20260905-000000-{}", q.short());
        if ambiguous_id {
            f.questions.put(&mut duplicate).unwrap();
        }
        let rows = if ambiguous_id {
            format!("{}: hold", q.short())
        } else {
            format!("{}: hold\n{}: merge", q.id, q.id)
        };
        let words = "answer each as recommended";
        let run = standing_owner(&f, words, Some(&rows));
        assert_untouched(&f, &hold(&f, &run, words));
    }
}

#[test]
fn same_chat_single_word_hold_remains_supported() {
    let f = fixture();
    let talks = Talks::at(f.tmp.path().join("talks"));
    let mut t = talks.get(&f.talk_id).unwrap();
    talk::record(&mut t, &talks, "hold", vec![]).unwrap();
    assert!(hold(&f, &t.id, "hold").status.success());
    assert_eq!(
        f.questions.get(&f.qid).unwrap().resolution().as_deref(),
        Some("hold")
    );
}

#[test]
fn concurrent_standing_answers_settle_once_and_keep_the_winning_authority() {
    let f = fixture();
    let words = format!("{}: hold", f.qid);
    let first = standing_owner(&f, &words, None);
    let talks = Talks::at(f.tmp.path().join("talks"));
    let mut t = talks.get(&first).unwrap();
    t.id = "20260906-000000-dec0".into();
    talks.put(&mut t).unwrap();
    let second = t.id;
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| hold(&f, &first, &words));
        let b = scope.spawn(|| hold(&f, &second, &words));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(a.status.success(), b.status.success());
    let saved = f.questions.get(&f.qid).unwrap();
    assert_eq!(saved.resolution().as_deref(), Some("hold"));
    assert_eq!(
        saved.answer_authority.unwrap().talk,
        if a.status.success() { first } else { second }
    );
}

#[test]
fn generated_and_quoted_recommendations_do_not_authorize_but_hold_is_separate_from_closure() {
    for (format, accepted) in [
        ("> {}: hold", false),
        ("```\n{}: hold\n```", false),
        ("````\n```\n{}: hold\n```\n````", false),
        ("> Example\n{}: hold", false),
        ("    {}: hold", false),
        ("- {}: hold、その後PRを閉じる", true),
    ] {
        let f = fixture();
        let words = "answer each as recommended";
        let run = standing_owner(&f, words, Some(&format.replace("{}", &f.qid)));
        let out = hold(&f, &run, words);
        assert_eq!(
            out.status.success(),
            accepted,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if accepted {
            let saved = f.questions.get(&f.qid).unwrap();
            assert_eq!(saved.resolution().as_deref(), Some("hold"));
            assert!(saved.chosen_action().is_none());
        } else {
            assert_untouched(&f, &out);
        }
    }
}

#[test]
fn legacy_recorded_send_boundaries_support_existing_owner_authorization() {
    for boundaries in [false, true] {
        let f = fixture();
        let words = "ありがと！じゃあそれぞれ回答してー！";
        let run = standing_owner(&f, words, Some(&format!("{}: hold", f.qid)));
        let talks = Talks::at(f.tmp.path().join("talks"));
        let mut t = talks.get(&run).unwrap();
        t.turns[1].sender = None;
        t.turns[1].messages[0].sender = None;
        if !boundaries {
            t.turns[1].breaks = None;
        }
        talks.put(&mut t).unwrap();
        let out = hold(&f, &run, words);
        assert_eq!(
            out.status.success(),
            boundaries,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn a_same_chat_batch_is_not_a_specific_merge_confirmation() {
    let f = fixture();
    let words = "ありがと！じゃあそれぞれ回答してー！";
    let talks = Talks::at(f.tmp.path().join("talks"));
    let mut t = talks.get(&f.talk_id).unwrap();
    let mut recommendation = t.turns[0].clone();
    recommendation.who = talk::Who::Agent;
    recommendation.sender = Some(talk::Sender::Persona);
    recommendation.body = format!("{}: merge", f.qid);
    recommendation.at = Timestamp::now();
    t.turns.push(recommendation);
    talks.put(&mut t).unwrap();
    talk::record(&mut t, &talks, words, vec![]).unwrap();
    let out = answer(&f, &f.talk_id, Some(words));
    assert_untouched(&f, &out);
    assert!(String::from_utf8_lossy(&out.stderr).contains("specific reviewed PR and head"));
}

#[test]
fn generated_sends_do_not_replace_the_latest_real_owner_instruction() {
    for mode in ["queued", "drained", "coalesced", "posted"] {
        let f = fixture();
        let words = format!("{}: hold", f.qid);
        let run = standing_owner(&f, &words, None);
        let talks = Talks::at(f.tmp.path().join("talks"));
        let mut t = talks.get(&run).unwrap();
        if mode == "posted" {
            let mut post = t.turns[0].clone();
            post.who = talk::Who::Agent;
            post.sender = Some(talk::Sender::Magi);
            post.body = "merge it now".into();
            post.messages.clear();
            t.turns.push(post);
        } else if mode == "coalesced" {
            t.turns[0].body.push_str("\n\nmerge it now");
            t.turns[0].messages.push(talk::Message {
                sender: Some(talk::Sender::Magi),
                body: "merge it now".into(),
                attachments: vec![],
                reply: None,
            });
            t.turns[0].breaks = Some(vec![words.len() + 2]);
        } else {
            talk::queue_magi(&mut t, &talks, "merge it now").unwrap();
            if mode == "drained" {
                talk::drain(&mut t, &talks).unwrap();
            }
        }
        talks.put(&mut t).unwrap();
        let out = hold(&f, &run, &words);
        assert!(
            out.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let evidence = f.questions.get(&f.qid).unwrap().answer_authority.unwrap();
        assert_eq!(evidence.instruction, words);
        assert_eq!(evidence.turn, 0);
        assert_eq!(evidence.message, 0);
    }
}
