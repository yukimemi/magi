//! Owner decisions a chat carries out, through the real `magi answer` binary.
//!
//! A chat agent records the owner's request as an ordinary answer: no quote,
//! consent phrase or "only this conversation" ownership applies, a merge
//! included. These tests spawn the binary with `MAGI_RUN` / `MAGI_NODE` set
//! the way the standing chat seat gets them, so a guard that creeps back into
//! `answer_cmd` or the task commands fails here. What stays: an offered
//! choice, once-only answers, the saved merge target, and queue claims.

mod common;

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

fn mock_config() -> Config {
    Config {
        agents: vec![AgentSpec {
            id: "mock".to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["true".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
            max_turn_secs: None,
        }],
        ..Config::default()
    }
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let talks = Talks::at(tmp.path().join("talks"));
    let questions = Questions::at(tmp.path().join("questions"));
    let cfg = mock_config();
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
    // the owner's own reply follows it.
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

/// `magi answer <id> --reply <reply>` from the standing chat seat `run`.
fn choose_as(f: &Fixture, run: &str, id: &str, reply: &str) -> Output {
    common::magi_cli(f.tmp.path())
        .args(["answer", id, "--reply", reply])
        .env("MAGI_RUN", run)
        .env("MAGI_NODE", magi::queue::CHAT_NODE)
        .output()
        .expect("spawn magi")
}

fn choose(f: &Fixture, run: &str, reply: &str) -> Output {
    choose_as(f, run, &f.qid, reply)
}

fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "should go through: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_untouched(f: &Fixture, out: &Output) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "should be refused: {err}");
    let q = f.questions.get(&f.qid).expect("question");
    assert!(q.status.open(), "question must stay open");
    assert!(q.answer.is_none(), "nothing may be written");
}

/// A chat command run from the consulted conversation's seat.
fn chat_command(f: &Fixture, args: &[&str]) -> Output {
    common::magi_cli(f.tmp.path())
        .args(args)
        .current_dir(f.tmp.path())
        .env("MAGI_CONFIG_DIR", "")
        .env("MAGI_RUN", &f.talk_id)
        .env("MAGI_NODE", magi::queue::CHAT_NODE)
        .output()
        .unwrap()
}

/// Another open standing chat: not the one the question was handed to.
fn other_chat(f: &Fixture) -> String {
    let talks = Talks::at(f.tmp.path().join("talks"));
    talk::begin(
        &talks,
        &mock_config(),
        f.tmp.path().to_path_buf(),
        Some("mock"),
    )
    .expect("other talk")
    .id
}

#[test]
fn a_chat_merge_without_a_quote_is_recorded() {
    let f = fixture();
    let out = choose(&f, &f.talk_id, magi::land::APPROVE);
    assert_success(&out);
    let q = f.questions.get(&f.qid).expect("question");
    assert_eq!(q.status, QuestionStatus::Answered);
    assert_eq!(q.resolution().as_deref(), Some(magi::land::APPROVE));
    // Recorded once: a second decision is refused, the first one stands.
    assert!(!choose(&f, &f.talk_id, magi::land::HOLD).status.success());
    assert_eq!(
        f.questions.get(&f.qid).unwrap().resolution().as_deref(),
        Some(magi::land::APPROVE)
    );
}

#[test]
fn a_chat_merge_from_a_different_chat_is_recorded() {
    let f = fixture();
    let out = choose(&f, &other_chat(&f), magi::land::APPROVE);
    assert_success(&out);
    let q = f.questions.get(&f.qid).unwrap();
    assert_eq!(q.resolution().as_deref(), Some(magi::land::APPROVE));
    // The consultation record is delivery history, not an owner restriction.
    assert_eq!(q.consult.unwrap().talk, f.talk_id);
}

#[test]
fn a_chat_merge_keeps_the_saved_target_without_reciting_the_head() {
    let f = fixture();
    let target = magi::merge_decision::Target {
        pr: "https://github.com/example/repo/pull/42".into(),
        head: "reviewed-head".into(),
        language: "ja".into(),
    };
    f.questions
        .update(&f.qid, |q| {
            q.merge_target = Some(target.clone());
            Ok(())
        })
        .unwrap();
    assert_success(&choose(&f, &f.talk_id, magi::land::APPROVE));
    let q = f.questions.get(&f.qid).unwrap();
    assert_eq!(q.resolution().as_deref(), Some(magi::land::APPROVE));
    // The merge stays bound to the reviewed head the card named.
    assert_eq!(q.merge_target, Some(target));
}

#[test]
fn a_chat_answer_still_needs_an_offered_choice() {
    let f = fixture();
    assert_untouched(&f, &choose(&f, &f.talk_id, "merge everything"));
}

#[test]
fn legacy_consult_repair_records_the_owners_choice_and_keeps_original_handoff() {
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
    let old_handoff = talks.get(&f.talk_id).unwrap().turns[0].body.clone();
    assert!(!old_handoff.contains(magi::land::REPAIR));
    assert_success(&choose(&f, &f.talk_id, magi::land::REPAIR));
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
    let out = common::magi_cli(f.tmp.path())
        .args(["answer", &q.id, "--ask-chat"])
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

#[test]
fn concurrent_chat_answers_have_one_winner_and_no_second_execution() {
    let f = fixture();
    f.questions
        .update(&f.qid, |q| {
            q.choices
                .extend([magi::land::REPAIR.into(), "retry".into()]);
            Ok(())
        })
        .unwrap();
    let other = "20260907-000000-c0de";
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| choose(&f, &f.talk_id, magi::land::REPAIR));
        let b = scope.spawn(|| choose(&f, other, "retry"));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(a.status.success(), b.status.success());
    assert_eq!(
        f.questions.get(&f.qid).unwrap().resolution().as_deref(),
        Some(if a.status.success() {
            magi::land::REPAIR
        } else {
            "retry"
        })
    );
}

#[test]
fn a_chat_answer_after_a_manual_answer_is_refused_and_keeps_the_record() {
    let f = fixture();
    let (settled, ()) = f
        .questions
        .update(&f.qid, |q| {
            q.answer(magi::ask::Answer::Choice("hold".into()))
        })
        .unwrap();
    let out = choose(&f, &f.talk_id, magi::land::APPROVE);
    assert!(!out.status.success());
    assert_eq!(
        serde_json::to_value(f.questions.get(&f.qid).unwrap()).unwrap(),
        serde_json::to_value(settled).unwrap(),
        "the manual answer must survive"
    );
}

#[test]
fn a_late_consultation_cannot_undo_a_manual_cli_hold() {
    let f = fixture();
    let talks = Talks::at(f.tmp.path().join("talks"));
    let mut destination = talks.get(&f.talk_id).unwrap();
    talk::close(&mut destination, &talks).unwrap();
    f.questions
        .update(&f.qid, |q| {
            q.consult = None;
            Ok(())
        })
        .unwrap();
    let q = f.questions.get(&f.qid).unwrap();
    let out = common::magi_cli(f.tmp.path())
        .args(["answer", &f.qid, "--reply", "hold"])
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .output()
        .unwrap();
    assert_success(&out);
    let settled = f.questions.get(&f.qid).unwrap();
    assert_eq!(settled.resolution().as_deref(), Some("hold"));
    assert!(magi::consult::begin(&f.questions, &talks, &q, &destination).is_err());
    assert_eq!(
        serde_json::to_value(f.questions.get(&f.qid).unwrap()).unwrap(),
        serde_json::to_value(settled).unwrap()
    );
    let destination = talks.get(&destination.id).unwrap();
    assert_eq!(destination.status, talk::TalkStatus::Closed);
    assert!(destination.pending.is_empty());
    assert!(destination.pending_messages.is_empty());
    assert!(!magi::consult::pending_consults(
        &f.questions,
        &destination.id
    ));
}

#[test]
fn chat_dismisses_an_obsolete_execution_without_touching_its_pr() {
    for unconsulted in [false, true] {
        let f = fixture();
        let (q, ()) = f
            .questions
            .update(&f.qid, |q| {
                q.choices.push(magi::merge_decision::DISMISS.into());
                q.merge_target = Some(magi::merge_decision::Target {
                    pr: "https://example.test/pull/1".into(),
                    head: "reviewed-head".into(),
                    language: "en".into(),
                });
                if unconsulted {
                    q.consult = None;
                }
                Ok(())
            })
            .unwrap();
        let mut state = magi::run::RunState::new(
            f.tmp.path().into(),
            "main".into(),
            "base".into(),
            "Original task".into(),
            Config::default(),
        );
        state.id = q.run.clone();
        state.status = magi::run::RunStatus::Blocked;
        state.driver_exited = true;
        state.land_approval = Some(magi::run::LandApproval {
            question: q.id.clone(),
            head: "reviewed-head".into(),
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
        state.save_under(f.tmp.path()).unwrap();
        let out = choose(&f, &f.talk_id, magi::merge_decision::DISMISS);
        assert_success(&out);
        let stopped = magi::run::RunState::load_under(&q.run, f.tmp.path()).unwrap();
        // A terminal blocked status is retained as history; the persisted
        // dismissal, rather than a fabricated new run, retires its execution.
        assert_eq!(stopped.status, magi::run::RunStatus::Blocked);
        assert_eq!(stopped.pr.unwrap().state, "open");
        let decision = stopped.merge_decision.unwrap();
        assert_eq!(decision.choice, magi::merge_decision::DISMISS);
        assert!(!decision.closed);
        assert!(
            !choose(&f, &f.talk_id, magi::merge_decision::DISMISS)
                .status
                .success()
        );
    }
}

#[test]
fn chat_posts_the_original_or_the_rewrite_without_a_quote() {
    for (choice, reply) in [
        (
            magi::github_text::POST_ORIGINAL,
            magi::github_text::Reply::Original,
        ),
        (
            magi::github_text::USE_REWRITE,
            magi::github_text::Reply::Rewrite,
        ),
    ] {
        for consulted in [true, false] {
            let f = fixture();
            f.questions
                .update(&f.qid, |q| {
                    q.node = magi::github_text::ASK_NODE.into();
                    q.choices = vec![choice.into(), magi::github_text::USE_FALLBACK.into()];
                    if !consulted {
                        q.consult = None;
                    }
                    Ok(())
                })
                .unwrap();
            let prompt = magi::prompt::chat_consult(&f.questions.get(&f.qid).unwrap());
            assert!(prompt.contains("wait for the owner to choose"));
            assert!(!prompt.contains("answer it yourself"));
            assert_success(&choose(&f, &f.talk_id, choice));
            let answered = f.questions.get(&f.qid).unwrap();
            assert_eq!(answered.resolution().as_deref(), Some(choice));
            assert_eq!(magi::github_text::read_reply(&answered), reply);
        }
    }
}

#[test]
fn chat_use_my_text_publishes_the_owners_latest_title() {
    for old_thread in [false, true] {
        let f = fixture();
        f.questions
            .update(&f.qid, |q| {
                q.node = magi::github_text::ASK_NODE.into();
                q.choices = vec![
                    magi::github_text::USE_MY_TEXT.into(),
                    magi::github_text::USE_FALLBACK.into(),
                ];
                if old_thread {
                    q.say("Earlier English title")?;
                }
                Ok(())
            })
            .unwrap();
        let title = "Owner's replacement title";
        let talks = Talks::at(f.tmp.path().join("talks"));
        let mut chat = talks.get(&f.talk_id).unwrap();
        talk::record(&mut chat, &talks, title, vec![]).unwrap();
        let before = f.questions.get(&f.qid).unwrap();
        let prompt = magi::prompt::chat_consult(&before);
        assert!(prompt.contains("--title '<literal replacement title>'"));
        assert!(!before.thread.iter().any(|turn| turn.body == title));
        let answer = |choice: &str, title: Option<&str>| {
            let mut cli = common::magi_cli(f.tmp.path());
            cli.args(["answer", &f.qid, "--reply", choice])
                .env("MAGI_RUN", &f.talk_id)
                .env("MAGI_NODE", magi::queue::CHAT_NODE);
            if let Some(title) = title {
                cli.args(["--title", title]);
            }
            cli.output().unwrap()
        };
        for (choice, title) in [
            (magi::github_text::USE_MY_TEXT, None),
            (magi::github_text::USE_MY_TEXT, Some("   ")),
            (magi::github_text::USE_FALLBACK, Some(title)),
        ] {
            assert!(!answer(choice, title).status.success());
            assert_eq!(f.questions.get(&f.qid).unwrap(), before);
        }
        assert_success(&answer(magi::github_text::USE_MY_TEXT, Some(title)));
        let saved = f.questions.get(&f.qid).unwrap();
        assert_eq!(saved.publication_title.as_deref(), Some(title));
        assert_eq!(saved.thread, before.thread);
        assert!(saved.answer_authority.is_none());
        assert_eq!(
            magi::github_text::read_reply(&saved),
            magi::github_text::Reply::Title(title.into())
        );
        assert!(
            !answer(magi::github_text::USE_MY_TEXT, Some("Later title"))
                .status
                .success()
        );
        assert_eq!(f.questions.get(&f.qid).unwrap(), saved);
    }
}

#[test]
fn unconsulted_free_text_records_the_chats_answer() {
    let f = fixture();
    f.questions
        .update(&f.qid, |q| {
            q.choices.clear();
            q.node = "implement".into();
            q.consult = None;
            Ok(())
        })
        .unwrap();
    let text = "Use the existing local cache.";
    assert_success(&choose(&f, &other_chat(&f), text));
    assert_eq!(
        f.questions.get(&f.qid).unwrap().resolution().as_deref(),
        Some(text)
    );
}

fn posting_deputy_question(f: &Fixture) -> magi::ask::Question {
    f.questions
        .update(&f.qid, |q| {
            q.node = magi::github_text::ASK_NODE.into();
            q.seat = magi::github_text::ASK_SEAT.into();
            q.consult = None;
            q.choices = vec![
                magi::github_text::USE_FALLBACK.into(),
                magi::github_text::USE_MY_TEXT.into(),
            ];
            let mut deputy = magi::ask::Deputy::new("Posting question".into());
            deputy.seat = Some(magi::agent::SeatState::new("title-deputy", "mock", 1));
            q.deputy = Some(deputy);
            q.say("Earlier English title")?;
            Ok(())
        })
        .unwrap()
        .0
}

fn title_deputy(f: &Fixture) -> Output {
    common::magi_cli(f.tmp.path())
        .args([
            "ask",
            "--settle",
            &f.qid,
            "--choice",
            magi::github_text::USE_MY_TEXT,
        ])
        .env("MAGI_NODE", magi::deputy::NODE)
        .env("MAGI_RUN", &f.questions.get(&f.qid).unwrap().run)
        .env("MAGI_SEAT", "title-deputy")
        .output()
        .unwrap()
}

#[test]
fn deputy_settles_use_my_text_on_the_owners_literal_title_without_a_quote() {
    let f = fixture();
    posting_deputy_question(&f);
    f.questions
        .update(&f.qid, |q| q.say("Current literal title"))
        .unwrap();
    assert_success(&title_deputy(&f));
    let answered = f.questions.get(&f.qid).unwrap();
    assert_eq!(
        magi::github_text::read_reply(&answered),
        magi::github_text::Reply::Title("Current literal title".into())
    );
    let before = std::fs::read(f.questions.path_of(&f.qid)).unwrap();
    assert!(!title_deputy(&f).status.success(), "settled once");
    assert_eq!(std::fs::read(f.questions.path_of(&f.qid)).unwrap(), before);
}

#[test]
fn chat_decisions_on_operator_held_tasks_go_through_without_lifting_the_hold() {
    for node in [magi::triage::NODE, magi::triage::DEPS_NODE, "recovery"] {
        for choice in ["hold", "retry", "discard"] {
            let f = fixture();
            let (q, ()) = f
                .questions
                .update(&f.qid, |q| {
                    q.node = node.into();
                    q.choices = vec![
                        "retry".into(),
                        "wait".into(),
                        "discard".into(),
                        "hold".into(),
                    ];
                    Ok(())
                })
                .unwrap();
            let queue = magi::queue::Queue::at(f.tmp.path().join("queue"));
            let mut task = magi::queue::Task::new(
                "Held task".into(),
                "Original task".into(),
                f.tmp.path().into(),
                magi::queue::Source::Human,
            );
            task.runs.push(q.run.clone());
            task.hold_manual(Some("Owner says wait".into()));
            queue.put(&mut task).unwrap();
            let before = std::fs::read(queue.path_of(&task.id)).unwrap();
            let out = choose(&f, &f.talk_id, choice);
            assert!(
                out.status.success(),
                "{node}/{choice}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                f.questions.get(&f.qid).unwrap().resolution().as_deref(),
                Some(choice)
            );
            // Recording an answer never lifts a hold by itself.
            assert!(queue.get(&task.id).unwrap().operator_held());
            assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        }
    }
}

#[test]
fn chat_approvals_on_operator_held_tasks_are_recorded() {
    for node in [magi::land::APPROVAL_NODE, magi::bump::NOTICE_NODE] {
        for choice in [magi::land::APPROVE, magi::land::HOLD] {
            let f = fixture();
            let (q, ()) = f
                .questions
                .update(&f.qid, |q| {
                    q.node = node.into();
                    if node == magi::bump::NOTICE_NODE {
                        q.seat = "release-watch".into();
                    }
                    Ok(())
                })
                .unwrap();
            let queue = magi::queue::Queue::at(f.tmp.path().join("queue"));
            let mut task = magi::queue::Task::new(
                "Held approval".into(),
                "Original task".into(),
                f.tmp.path().into(),
                magi::queue::Source::Human,
            );
            task.runs.push(q.run.clone());
            task.hold_manual(Some("Owner says wait".into()));
            queue.put(&mut task).unwrap();
            let prompt = magi::prompt::chat_consult(&f.questions.get(&f.qid).unwrap());
            assert!(prompt.contains("the merge waits for the owner"));
            assert!(!prompt.contains("answer it yourself"));
            assert_success(&choose(&f, &f.talk_id, choice));
            assert_eq!(
                f.questions.get(&f.qid).unwrap().resolution().as_deref(),
                Some(choice)
            );
            assert!(queue.get(&task.id).unwrap().operator_held());
        }
    }
}

const RECOVERY_LABEL: &str = "daemon: 既存の実行を再開する";
const RECOVERY_HOLD: &str = "daemon: 保留を続ける";

fn recovery_fixture() -> Fixture {
    let f = fixture();
    f.questions
        .update(&f.qid, |q| {
            q.node = "internal-recovery".into();
            q.seat = "conductor".into();
            q.consult = None;
            q.choices = vec![RECOVERY_LABEL.into(), RECOVERY_HOLD.into()];
            q.actions = BTreeMap::from([
                (
                    RECOVERY_LABEL.into(),
                    magi::ask::ChoiceAction::Resume { run: q.run.clone() },
                ),
                (RECOVERY_HOLD.into(), magi::ask::ChoiceAction::Hold),
            ]);
            Ok(())
        })
        .unwrap();
    f
}

#[test]
fn chat_answers_accept_short_and_full_ids_with_stable_aliases_and_localized_labels() {
    for short in [true, false] {
        for alias in [
            "resume run",
            "resume-run",
            "再開",
            "実行を再開する",
            "Resume Run",
            "resume run beef",
            "resume",
            RECOVERY_LABEL,
        ] {
            let f = recovery_fixture();
            let q = f.questions.get(&f.qid).unwrap();
            let id = if short { q.short() } else { &f.qid };
            let out = choose_as(&f, &f.talk_id, id, alias);
            assert!(
                out.status.success(),
                "{alias}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                f.questions.get(&f.qid).unwrap().resolution().as_deref(),
                Some(RECOVERY_LABEL)
            );
            // Recording a decision does not start a fresh competition or release ownership.
            assert!(!f.tmp.path().join("runs").exists());
            assert!(!choose_as(&f, &f.talk_id, id, alias).status.success());
        }
    }
    let f = recovery_fixture();
    assert_success(&choose(&f, &f.talk_id, "保留"));
    assert_eq!(
        f.questions.get(&f.qid).unwrap().resolution().as_deref(),
        Some(RECOVERY_HOLD)
    );
}

#[test]
fn ambiguous_and_unknown_aliases_are_refused_but_prefixes_resolve() {
    let f = recovery_fixture();
    f.questions
        .update(&f.qid, |q| {
            q.actions.insert(
                RECOVERY_HOLD.into(),
                magi::ask::ChoiceAction::Resume { run: q.run.clone() },
            );
            Ok(())
        })
        .unwrap();
    assert_untouched(&f, &choose(&f, &f.talk_id, "resume-run"));
    let f = recovery_fixture();
    assert_untouched(&f, &choose(&f, &f.talk_id, "requeue-run"));
    let f = recovery_fixture();
    let prefix = &f.qid[..f.qid.len() - 2];
    assert_success(&choose_as(&f, &f.talk_id, prefix, "resume-run"));
    assert_eq!(
        f.questions.get(&f.qid).unwrap().resolution().as_deref(),
        Some(RECOVERY_LABEL)
    );
}

#[test]
fn chat_resume_answer_preserves_pending_merge_approval_and_existing_run_ownership() {
    let f = recovery_fixture();
    let mut state = magi::run::RunState::new(
        f.tmp.path().into(),
        "main".into(),
        "abc1234".into(),
        "update docs".into(),
        Config::default(),
    );
    state.status = magi::run::RunStatus::Blocked;
    let mut approval = magi::ask::Question::new(
        state.id.clone(),
        magi::land::APPROVAL_NODE.into(),
        "land".into(),
        "Merge?".into(),
        String::new(),
        vec!["merge".into(), "hold".into()],
    );
    f.questions.put(&mut approval).unwrap();
    state.land_approval = Some(magi::run::LandApproval {
        question: approval.id.clone(),
        head: "reviewed-head".into(),
    });
    state.save_under(f.tmp.path()).unwrap();
    let before = serde_json::to_value(&state).unwrap();
    f.questions
        .update(&f.qid, |q| {
            q.run = state.id.clone();
            q.actions.insert(
                RECOVERY_LABEL.into(),
                magi::ask::ChoiceAction::Resume {
                    run: state.id.clone(),
                },
            );
            Ok(())
        })
        .unwrap();
    let queue = magi::queue::Queue::at(f.tmp.path().join("queue"));
    let mut task = magi::queue::Task::new(
        "docs".into(),
        "update docs".into(),
        f.tmp.path().into(),
        magi::queue::Source::Human,
    );
    task.runs.push(state.id.clone());
    task.hold_machine(Some("mise github-cli has been repaired".into()));
    queue.put(&mut task).unwrap();
    let task_before = serde_json::to_value(&task).unwrap();
    assert_success(&choose(&f, &f.talk_id, "resume-run"));
    assert!(f.questions.get(&approval.id).unwrap().answer.is_none());
    let after = magi::run::RunState::load_under(&state.id, f.tmp.path()).unwrap();
    assert_eq!(serde_json::to_value(&after).unwrap(), before);
    assert!(!after.released());
    assert_eq!(queue.list().len(), 1);
    assert_eq!(
        serde_json::to_value(queue.get(&task.id).unwrap()).unwrap(),
        task_before
    );
    assert!(!choose(&f, &f.talk_id, "resume-run").status.success());
    assert!(f.questions.get(&approval.id).unwrap().answer.is_none());
}

#[test]
fn consulted_chat_files_tasks_with_ordinary_options_and_no_quote() {
    let f = fixture();
    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(f.tmp.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(magi::consult::pending_consults(&f.questions, &f.talk_id));
    let queue = magi::queue::Queue::at(f.tmp.path().join("queue"));
    let out = chat_command(&f, &["task", "add", "--solo", "Reimplement from scratch"]);
    assert_success(&out);
    let out = chat_command(
        &f,
        &[
            "task",
            "add",
            "--urgent",
            "--priority",
            "3",
            "Port the old code",
        ],
    );
    assert_success(&out);
    let tasks = queue.list();
    assert_eq!(tasks.len(), 2);
    let solo = tasks
        .iter()
        .find(|t| t.instruction == "Reimplement from scratch")
        .unwrap();
    assert!(solo.solo);
    assert_eq!(solo.chat_talk(), Some(f.talk_id.as_str()));
    let urgent = tasks
        .iter()
        .find(|t| t.instruction == "Port the old code")
        .unwrap();
    assert!(urgent.urgent);
    assert_eq!(urgent.priority, 3);
    // Filing work is not an answer.
    assert!(f.questions.get(&f.qid).unwrap().answer.is_none());
}

#[test]
fn consulted_chat_operates_a_task_it_did_not_file_and_keeps_queue_claims() {
    let f = fixture();
    let queue = magi::queue::Queue::at(f.tmp.path().join("queue"));
    let mut task = magi::queue::Task::new(
        "old work".into(),
        "old work".into(),
        f.tmp.path().into(),
        magi::queue::Source::Human,
    );
    queue.put(&mut task).unwrap();
    for args in [
        vec!["task", "hold", &task.id, "Wait for replacement"],
        vec!["task", "release", &task.id],
        vec!["task", "priority", &task.id, "5"],
        vec!["task", "done", &task.id],
    ] {
        // A running daemon's claim still wins over the chat's request.
        let claim = queue.claim(&task.id).unwrap();
        assert!(!chat_command(&f, &args).status.success(), "{args:?}");
        drop(claim);
        assert_success(&chat_command(&f, &args));
        let saved = queue.get(&task.id).unwrap();
        match args[1] {
            "hold" => assert!(saved.operator_held()),
            "release" => assert!(saved.status.runnable()),
            "priority" => assert_eq!(saved.priority, 5),
            "done" => assert_eq!(saved.status, magi::queue::TaskStatus::Done),
            _ => unreachable!(),
        }
    }
    assert!(f.questions.get(&f.qid).unwrap().answer.is_none());
}

#[test]
fn reply_other_is_a_note_without_answer_or_choice_effect() {
    let f = fixture();
    let words = "Reimplement from scratch instead of porting";
    let out = chat_command(&f, &["answer", &f.qid, "--reply-other", words]);
    assert_success(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("waiter remains held"));
    let q = f.questions.get(&f.qid).unwrap();
    assert!(q.status.open());
    assert!(q.answer.is_none());
    let outcome = q.consult_outcome.unwrap();
    assert_eq!(outcome.text, words);
    assert_eq!(outcome.talk, f.talk_id);
    assert!(!magi::consult::pending_consults(&f.questions, &f.talk_id));
    // The consultation has ended; a second note has nothing to end.
    assert!(
        !chat_command(&f, &["answer", &f.qid, "--reply-other", "Another idea"])
            .status
            .success()
    );
}

#[test]
fn ordinary_chat_task_add_does_not_require_an_existing_talk_record() {
    let f = fixture();
    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(f.tmp.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = common::magi_cli(f.tmp.path())
        .args(["task", "add", "Independent work"])
        .current_dir(f.tmp.path())
        .env("MAGI_CONFIG_DIR", "")
        .env("MAGI_RUN", "missing-standing-chat")
        .env("MAGI_NODE", magi::queue::CHAT_NODE)
        .output()
        .unwrap();
    assert_success(&out);
    let tasks = magi::queue::Queue::at(f.tmp.path().join("queue")).list();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].instruction, "Independent work");
    for args in [
        vec!["task", "priority", &tasks[0].id, "9"],
        vec!["task", "done", &tasks[0].id],
    ] {
        let out = common::magi_cli(f.tmp.path())
            .args(args)
            .env("MAGI_RUN", "missing-standing-chat")
            .env("MAGI_NODE", magi::queue::CHAT_NODE)
            .output()
            .unwrap();
        assert_success(&out);
    }
    let saved = magi::queue::Queue::at(f.tmp.path().join("queue"))
        .get(&tasks[0].id)
        .unwrap();
    assert_eq!(saved.priority, 9);
    assert_eq!(saved.status, magi::queue::TaskStatus::Done);
}
