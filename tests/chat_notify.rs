//! Completion notices to the chat a task came from, and `magi talk post`.
//!
//! Stores live in temp directories; no port is opened and no agent runs.

#[allow(unused_macros, unused_imports)]
mod common;

use std::collections::BTreeMap;
use std::process::{Command, Output};

use magi::chat_notify;
use magi::config::{AgentKind, AgentSpec, Config};
use magi::queue::{CHAT_NODE, ChatNoticeOutcome, Queue, Source, Task};
use magi::talk::{self, Talks};

struct World {
    tmp: tempfile::TempDir,
    queue: Queue,
    talks: Talks,
    talk_id: String,
}

fn world() -> World {
    let tmp = tempfile::tempdir().expect("tempdir");
    let talks = Talks::at(tmp.path().join("talks"));
    let queue = Queue::at(tmp.path().join("queue"));
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
    let t = talk::begin(&talks, &cfg, tmp.path().to_path_buf(), Some("mock")).expect("talk");
    World {
        tmp,
        queue,
        talks,
        talk_id: t.id,
    }
}

fn finished_task(w: &World, source_talk: &str, run: &str) -> Task {
    let mut t = Task::new(
        "tidy".to_owned(),
        "tidy up".to_owned(),
        w.tmp.path().to_path_buf(),
        Source::Agent {
            run: source_talk.to_owned(),
            node: CHAT_NODE.to_owned(),
        },
    );
    t.start(run.to_owned());
    t.succeed();
    w.queue.put(&mut t).expect("put task");
    t
}

fn drafts(w: &World) -> String {
    w.talks.get(&w.talk_id).expect("talk").pending
}

#[tokio::test]
async fn a_finished_task_is_reported_once_however_often_it_is_swept() {
    let _home = common::home_lock().await;
    let w = world();
    let t = finished_task(&w, &w.talk_id, "20260902-000000-aaaa");
    let first = chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
    assert_eq!(first, vec![w.talk_id.clone()]);
    let draft = drafts(&w);
    assert!(draft.contains(magi::prompt::CHAT_TASK_NOTICE_HEADING));
    assert!(draft.contains(&t.id));
    assert!(draft.contains("done"));
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
    assert_eq!(drafts(&w), draft, "a second sweep adds nothing");
    let rec = w
        .queue
        .get(&t.id)
        .expect("task")
        .chat_notice
        .expect("recorded");
    assert_eq!(rec.outcome, ChatNoticeOutcome::Posted);
}

#[tokio::test]
async fn a_crash_between_the_talk_and_the_task_write_does_not_post_twice() {
    let _home = common::home_lock().await;
    let w = world();
    let t = finished_task(&w, &w.talk_id, "20260902-000000-aaaa");
    // What a sweep got done before dying: the talk accepted the key + draft.
    let mut talk = w.talks.get(&w.talk_id).expect("talk");
    let key = format!("{}:{}", t.id, t.chat_notice_due().expect("due"));
    assert!(talk::queue_once(&mut talk, &w.talks, "notice", &key).expect("queue"));
    let before = drafts(&w);
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
    assert_eq!(drafts(&w), before, "the draft must not grow");
    assert!(w.queue.get(&t.id).expect("task").chat_notice.is_some());
}

#[tokio::test]
async fn completion_stays_once_per_task_after_requeue_and_store_reload() {
    let _home = common::home_lock().await;
    for legacy in [false, true] {
        for held in [false, true] {
            let w = world();
            let t = finished_task(&w, &w.talk_id, "run-1");
            if legacy {
                // Simulate an older build dying after saving only the talk.
                let mut talk = w.talks.get(&w.talk_id).expect("talk");
                talk::queue_once(
                    &mut talk,
                    &w.talks,
                    "old completion",
                    &format!("{}:done#1", t.id),
                )
                .expect("legacy notice");
            } else {
                assert_eq!(
                    chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()),
                    vec![w.talk_id.clone()]
                );
            }
            w.queue
                .modify(&t.id, |task| {
                    task.requeue();
                    true
                })
                .expect("requeue");
            if held {
                w.queue
                    .modify(&t.id, |task| {
                        task.hold_manual(Some("decision".to_owned()));
                        true
                    })
                    .expect("hold");
                assert_eq!(
                    chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()),
                    vec![w.talk_id.clone()],
                    "a new hold still notifies"
                );
            }
            let before = drafts(&w);
            w.queue
                .modify(&t.id, |task| {
                    task.requeue();
                    task.start("run-2".to_owned());
                    task.succeed();
                    true
                })
                .expect("finish again");
            let queue = Queue::at(w.tmp.path().join("queue"));
            let talks = Talks::at(w.tmp.path().join("talks"));
            assert!(chat_notify::sweep(&queue, &talks, w.tmp.path()).is_empty());
            assert_eq!(drafts(&w), before, "completion must not grow the draft");
            assert_eq!(queue.get(&t.id).expect("task").chat_notice_due(), None);
        }
    }
}

#[tokio::test]
async fn completion_matching_requires_the_exact_task_and_a_valid_legacy_key() {
    let _home = common::home_lock().await;
    let w = world();
    let t = finished_task(&w, &w.talk_id, "run-1");
    let mut talk = w.talks.get(&w.talk_id).expect("talk");
    for key in [
        format!("{}x:done#1", t.id),
        format!("{}:done#", t.id),
        format!("{}:done#1extra", t.id),
        format!("{}:held#1", t.id),
    ] {
        talk::queue_once(&mut talk, &w.talks, "unrelated notice", &key).expect("queue");
    }
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()),
        vec![w.talk_id.clone()]
    );
    assert!(drafts(&w).contains(magi::prompt::CHAT_TASK_NOTICE_HEADING));
}

#[tokio::test]
async fn a_closed_or_deleted_talk_is_skipped_and_never_reopened() {
    let _home = common::home_lock().await;
    let w = world();
    let mut talk = w.talks.get(&w.talk_id).expect("talk");
    talk::close(&mut talk, &w.talks).expect("close");
    let t = finished_task(&w, &w.talk_id, "20260902-000000-aaaa");
    let ghost = finished_task(&w, "20260101-000000-gone", "20260902-000000-bbbb");
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
    for id in [&t.id, &ghost.id] {
        let n = w
            .queue
            .get(id)
            .expect("task")
            .chat_notice
            .expect("recorded");
        assert_eq!(n.outcome, ChatNoticeOutcome::Skipped);
    }
    assert!(!w.talks.get(&w.talk_id).expect("talk").status.open());
}

#[tokio::test]
async fn a_parking_upgrade_leaves_the_draft_and_starts_nothing() {
    let _home = common::home_lock().await;
    let w = world();
    finished_task(&w, &w.talk_id, "20260902-000000-aaaa");
    chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
    assert_eq!(
        chat_notify::talks_to_start(&w.talks),
        vec![w.talk_id.clone()]
    );
    assert_eq!(
        chat_notify::start_turns(&w.talks, true, &mut tokio::task::JoinSet::new()),
        0
    );
    assert!(drafts(&w).contains(magi::prompt::CHAT_TASK_NOTICE_HEADING));
    assert!(
        w.talks.claim_turn(&w.talk_id).expect("claim").is_some(),
        "no lease was taken while parking"
    );
}

#[tokio::test]
async fn an_operators_own_draft_is_not_started_by_the_daemon() {
    let _home = common::home_lock().await;
    let w = world();
    let mut talk = w.talks.get(&w.talk_id).expect("talk");
    talk::queue(&mut talk, &w.talks, "half a thought", Vec::new()).expect("queue");
    assert!(chat_notify::talks_to_start(&w.talks).is_empty());
}

fn post(w: &World, talk: &str, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_magi"));
    cmd.args(["talk", "post", talk, "found it"])
        .env("MAGI_HOME", w.tmp.path())
        .env("MAGI_CONFIG_DIR", w.tmp.path().join("config"))
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn magi")
}

#[tokio::test]
async fn talk_post_appends_for_the_chats_own_task_and_starts_no_turn() {
    let _home = common::home_lock().await;
    let w = world();
    let run = "20260902-000000-aaaa";
    finished_task(&w, &w.talk_id, run);
    let out = post(
        &w,
        &w.talk_id,
        &[("MAGI_RUN", run), ("MAGI_NODE", "implement")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let talk = w.talks.get(&w.talk_id).expect("talk");
    let last = talk.turns.last().expect("turn");
    assert_eq!(last.who, talk::Who::Agent);
    assert_eq!(last.body, "found it");
    assert_eq!(last.posted_by.as_deref(), Some(run));
    assert!(talk.pending.is_empty(), "no draft");
    assert!(
        w.talks.claim_turn(&w.talk_id).expect("claim").is_some(),
        "no lease"
    );
}

#[tokio::test]
async fn talk_post_is_refused_without_a_traceable_seat() {
    let _home = common::home_lock().await;
    let w = world();
    let run = "20260902-000000-aaaa";
    finished_task(&w, &w.talk_id, run);
    let other = world();
    let refused: Vec<(&str, Output)> = vec![
        ("no run", post(&w, &w.talk_id, &[])),
        ("no node", post(&w, &w.talk_id, &[("MAGI_RUN", run)])),
        (
            "the chat seat itself",
            post(
                &w,
                &w.talk_id,
                &[("MAGI_RUN", run), ("MAGI_NODE", CHAT_NODE)],
            ),
        ),
        (
            "an unknown run",
            post(
                &w,
                &w.talk_id,
                &[("MAGI_RUN", "nope"), ("MAGI_NODE", "implement")],
            ),
        ),
    ];
    for (why, out) in refused {
        assert!(!out.status.success(), "{why} should be refused");
    }
    // A task filed by another talk cannot post here.
    let theirs = "20260902-000000-cccc";
    finished_task(&w, &other.talk_id, theirs);
    let out = post(
        &w,
        &w.talk_id,
        &[("MAGI_RUN", theirs), ("MAGI_NODE", "implement")],
    );
    assert!(!out.status.success(), "another talk's task");
    assert!(w.talks.get(&w.talk_id).expect("talk").turns.is_empty());
}

#[tokio::test]
async fn a_notice_appended_to_an_operators_draft_is_not_started() {
    let _home = common::home_lock().await;
    let w = world();
    let mut talk = w.talks.get(&w.talk_id).expect("talk");
    talk::queue(&mut talk, &w.talks, "half a thought", Vec::new()).expect("queue");
    finished_task(&w, &w.talk_id, "20260902-000000-aaaa");
    chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
    assert!(drafts(&w).contains(magi::prompt::CHAT_TASK_NOTICE_HEADING));
    assert!(chat_notify::talks_to_start(&w.talks).is_empty());
}
