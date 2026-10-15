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
