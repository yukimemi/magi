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
    let talks = Talks::at(tmp.path().join("talks"));
    let questions = Questions::at(tmp.path().join("questions"));
    let script = tmp.path().join("answer.sh");
    std::fs::write(
        &script,
        "cat >/dev/null\n\
         test \"$MAGI_ALLOW_WRITE\" = \"$EXPECTED_WRITE\" || exit 31\n\
         if test \"$DO_ANSWER\" = 1; then\n\
           \"$ANSWER_BIN\" answer \"$QUESTION\" --reply \"$CHOICE\" --quote \"$OWNER_WORDS\" || exit 32\n\
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
    for choice in ["hold", magi::land::REPAIR, "retry"] {
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
                    ],
                );
                questions.put(&mut q).unwrap();
                let mut t =
                    talk::begin(&talks, &cfg, tmp.path().to_path_buf(), Some("mock")).unwrap();
                if destination != "none" {
                    let origin = if destination == "same" {
                        t.clone()
                    } else {
                        talk::begin(&talks, &cfg, tmp.path().to_path_buf(), Some("mock")).unwrap()
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
                let env = &mut cfg.agents[0].env;
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
                talk::say(&lease, &mut t, &talks, &cfg, &words, vec![])
                    .await
                    .unwrap();
                let saved = questions.get(&q.id).unwrap();
                assert_eq!(saved.resolution().as_deref(), authorized.then_some(choice));
                if authorized {
                    assert_eq!(saved.answer_authority.unwrap().talk, t.id);
                }
            }
        }
    }
}
