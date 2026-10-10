//! A chat hears how the tasks it filed ended, through temp stores only: no
//! magi home, no port, no agent. `kick` is a recording closure.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use magi::chat_report::{self, HEADING};
use magi::config::{AgentKind, AgentSpec, Config};
use magi::queue::{CHAT_NODE, Queue, Source, Task, TaskStatus};
use magi::talk::{self, Talk, Talks};

struct World {
    tmp: tempfile::TempDir,
    queue: Queue,
    talks: Talks,
    talk: Talk,
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
    let talk = talk::begin(&talks, &cfg, tmp.path().to_path_buf(), Some("mock")).expect("talk");
    World {
        tmp,
        queue,
        talks,
        talk,
    }
}

fn chat_task(w: &World) -> Task {
    let mut t = Task::new(
        "Fix the thing".to_owned(),
        "Fix it".to_owned(),
        PathBuf::from("/repo"),
        Source::Agent {
            run: w.talk.id.clone(),
            node: CHAT_NODE.to_owned(),
        },
    );
    t.start("20260902-000000-beef".to_owned());
    w.queue.put(&mut t).expect("put");
    t
}

fn sweep(w: &World) -> (Vec<String>, Vec<String>) {
    let kicked = RefCell::new(Vec::new());
    let done = chat_report::sweep(&w.queue, &w.talks, w.tmp.path(), &|id| {
        kicked.borrow_mut().push(id.to_owned());
    });
    (done, kicked.into_inner())
}

fn pending(w: &World) -> String {
    w.talks.get(&w.talk.id).expect("talk").pending
}

#[test]
fn an_unfinished_task_is_not_reported() {
    let w = world();
    chat_task(&w);
    assert_eq!(sweep(&w), (vec![], vec![]));
    assert!(pending(&w).is_empty());
}

#[test]
fn a_done_task_is_reported_once() {
    let w = world();
    let mut t = chat_task(&w);
    t.succeed();
    w.queue.put(&mut t).unwrap();

    let (done, kicked) = sweep(&w);
    assert_eq!(done, vec![t.id.clone()]);
    assert_eq!(kicked, vec![w.talk.id.clone()]);
    let text = pending(&w);
    assert!(text.contains(HEADING) && text.contains(&t.id) && text.contains("done"));

    // The undrained draft is kicked again, but neither a second lap nor a stale snapshot written back posts again.
    assert_eq!(sweep(&w), (vec![], vec![w.talk.id.clone()]));
    let mut stale = t.clone();
    stale.chat_report = None;
    w.queue.put(&mut stale).unwrap();
    assert_eq!(sweep(&w), (vec![], vec![w.talk.id.clone()]));
    assert_eq!(pending(&w).matches(HEADING).count(), 1);
}

#[test]
fn a_failed_attempt_is_not_reported_but_the_hold_after_the_last_one_is() {
    let w = world();
    let mut t = chat_task(&w);
    t.fail("boom", 2);
    assert_eq!(t.status, TaskStatus::Failed);
    w.queue.put(&mut t).unwrap();
    assert_eq!(sweep(&w), (vec![], vec![]));

    t.start("20260902-000001-beef".to_owned());
    t.fail("boom again", 2);
    assert_eq!(t.status, TaskStatus::Held);
    w.queue.put(&mut t).unwrap();
    assert_eq!(sweep(&w).0, vec![t.id.clone()]);
    let text = pending(&w);
    assert!(text.contains("held") && text.contains("boom again"));
}

#[test]
fn a_closed_or_missing_talk_is_skipped_silently() {
    let w = world();
    let mut t = chat_task(&w);
    t.succeed();
    w.queue.put(&mut t).unwrap();
    let mut talk = w.talk.clone();
    talk::close(&mut talk, &w.talks).unwrap();

    // Handled (recorded as skipped) but nobody is kicked.
    assert_eq!(sweep(&w), (vec![t.id.clone()], vec![]));
    assert!(w.talks.get(&w.talk.id).unwrap().pending.is_empty());
    let stored = w.queue.get(&t.id).unwrap();
    assert!(stored.chat_report.as_ref().is_some_and(|r| r.skipped));
    assert!(stored.chat_report_due().is_none());

    // A talk that does not exist at all behaves the same.
    let mut other = Task::new(
        "t".to_owned(),
        "i".to_owned(),
        PathBuf::from("/repo"),
        Source::Agent {
            run: "no-such-talk".to_owned(),
            node: CHAT_NODE.to_owned(),
        },
    );
    other.succeed();
    w.queue.put(&mut other).unwrap();
    assert_eq!(sweep(&w), (vec![other.id.clone()], vec![]));
    assert_eq!(sweep(&w), (vec![], vec![]));
    assert!(w.queue.get(&other.id).unwrap().chat_report.is_some());
}

#[test]
fn a_crash_between_record_and_queue_is_completed_not_lost_or_doubled() {
    let w = world();
    let mut t = chat_task(&w);
    t.succeed();
    w.queue.put(&mut t).unwrap();
    // Recorded, but the draft never reached the talk.
    let failed = w.queue.report_chat(
        &t.id,
        |task, key| format!("# {HEADING}\n\nmagi-report: {} {key}", task.id),
        |_| anyhow::bail!("down"),
    );
    assert!(failed.is_err());
    let key = w
        .queue
        .get(&t.id)
        .unwrap()
        .chat_report_unsent()
        .expect("unsent")
        .to_owned();
    assert_eq!(key, format!("done:{}", t.runs[0]));
    assert!(pending(&w).is_empty());

    let (done, kicked) = sweep(&w);
    assert_eq!(done, vec![t.id.clone()]);
    assert_eq!(kicked, vec![w.talk.id.clone()]);
    assert_eq!(pending(&w).matches(HEADING).count(), 1);

    // Drafted but never confirmed: found by its marker, not queued again.
    let mut again = w.queue.get(&t.id).unwrap();
    again.chat_report.as_mut().unwrap().sent = false;
    w.queue.put(&mut again).unwrap();
    sweep(&w);
    assert_eq!(pending(&w).matches(HEADING).count(), 1);
    assert_eq!(w.queue.get(&t.id).unwrap().chat_report_unsent(), None);
}

#[test]
fn a_draft_nobody_ran_is_kicked_again_on_every_lap() {
    let w = world();
    let mut t = chat_task(&w);
    t.succeed();
    w.queue.put(&mut t).unwrap();
    assert_eq!(sweep(&w).1.len(), 1);
    // The first kick failed to start a turn: the draft is still there.
    assert_eq!(sweep(&w), (vec![], vec![w.talk.id.clone()]));
    // Once drained there is nothing left to start.
    let mut talk = w.talks.get(&w.talk.id).unwrap();
    talk::drain(&mut talk, &w.talks).unwrap();
    assert_eq!(sweep(&w), (vec![], vec![]));
}

#[test]
fn hold_release_hold_is_two_reports() {
    let w = world();
    let mut t = chat_task(&w);
    t.hold_machine(Some("first".to_owned()));
    w.queue.put(&mut t).unwrap();
    assert_eq!(sweep(&w).0.len(), 1);
    t.release();
    w.queue.put(&mut t).unwrap();
    t.start("20260902-000002-beef".to_owned());
    t.hold_machine(Some("second".to_owned()));
    w.queue.put(&mut t).unwrap();
    assert_eq!(sweep(&w).0.len(), 1);
    assert_eq!(pending(&w).matches(HEADING).count(), 2);
}

#[test]
fn talk_post_is_restricted_to_the_chat_that_filed_the_task() {
    let w = world();
    let t = chat_task(&w);
    let run = t.runs[0].clone();
    let post = |talk: &str, run: Option<&str>, node: Option<&str>| {
        chat_report::post(&w.queue, &w.talks, talk, run, node, "found something")
    };

    assert!(post(&w.talk.id, None, None).is_err(), "no MAGI_RUN");
    assert!(post(&w.talk.id, Some(" "), None).is_err());
    assert!(post(&w.talk.id, Some(&run), Some("chat")).is_err());
    assert!(post(&w.talk.id, Some("unknown-run"), Some("impl-A")).is_err());

    // Another conversation is refused, whoever the seat is.
    let mut other = w.talk.clone();
    other.id = "20260903-000000-aaaa".to_owned();
    w.talks.put(&mut other).unwrap();
    assert!(post(&other.id, Some(&run), Some("impl-A")).is_err());

    let before = w.talks.get(&w.talk.id).unwrap().turns.len();
    post(&w.talk.id, Some(&run), Some("impl-A")).expect("own chat");
    let after = w.talks.get(&w.talk.id).unwrap();
    assert_eq!(after.turns.len(), before + 1);
    let last = after.turns.last().unwrap();
    assert_eq!(last.who, talk::Who::Agent);
    assert_eq!(last.body, "found something");
    // No turn was started: nothing queued, no lease taken.
    assert!(after.pending.is_empty());
    assert!(!w.talks.turn_held(&w.talk.id));
}

#[test]
fn an_unsent_report_says_what_was_true_when_the_task_ended() {
    let w = world();
    let mut t = chat_task(&w);
    t.hold_machine(Some("disk full".to_owned()));
    w.queue.put(&mut t).unwrap();
    let failed = w.queue.report_chat(
        &t.id,
        |task, _| format!("held: {}", task.hold_reason.clone().unwrap_or_default()),
        |_| anyhow::bail!("down"),
    );
    assert!(failed.is_err());

    // The operator releases it and a new run starts before the retry.
    t.release();
    w.queue.put(&mut t).unwrap();
    t.start("20260902-000009-beef".to_owned());
    w.queue.put(&mut t).unwrap();

    let delivered = RefCell::new(String::new());
    let out = w
        .queue
        .report_chat(
            &t.id,
            |_, _| "rebuilt from the running task".to_owned(),
            |text| {
                *delivered.borrow_mut() = text.to_owned();
                Ok(true)
            },
        )
        .unwrap();
    assert_eq!(out, Some(true));
    assert_eq!(delivered.into_inner(), "held: disk full");
    // Running again is no ending: nothing more is due.
    assert_eq!(sweep(&w), (vec![], vec![]));
}
