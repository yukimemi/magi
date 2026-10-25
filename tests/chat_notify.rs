//! Completion notices to the chat a task came from, and `magi talk post`.
//!
//! Stores live in temp directories; no port is opened and no agent runs.

#[allow(unused_macros, unused_imports)]
mod common;

use std::collections::BTreeMap;
use std::process::Output;

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
            max_turn_secs: None,
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
    let saved = w.talks.get(&w.talk_id).expect("saved talk");
    assert_eq!(saved.pending_messages[0].sender, Some(talk::Sender::Magi));
    let display = saved.pending_messages[0]
        .display
        .as_ref()
        .expect("display snapshot");
    assert_eq!(display.task.as_deref(), Some(t.id.as_str()));
    assert!(display.facts.contains("done"));
    assert!(!display.facts.contains("Tell the operator"));
    assert_eq!(saved.completion_notices.len(), 1);
    let reference = &saved.completion_notices[0];
    assert_eq!(reference.task, t.id);
    assert_eq!(reference.run.as_deref(), Some("20260902-000000-aaaa"));
    assert_eq!(reference.notice, draft);
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
    let mut cmd = common::magi_cli(w.tmp.path());
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

// Keep raw records as well as decoded state: Queue::list deliberately skips
// unreadable tasks. A failure must distinguish a skipped notice from lost
// provenance or a later draft write, including leftover lock/break tickets.
fn notice_state(w: &World, stage: &str) -> String {
    let records = ["queue", "talks"].map(|dir| {
        let entries = std::fs::read_dir(w.tmp.path().join(dir)).map(|entries| {
            entries
                .map(|entry| {
                    entry.map(|entry| {
                        let path = entry.path();
                        let raw = std::fs::read_to_string(&path);
                        let due = if dir == "queue" && path.extension().is_some_and(|e| e == "json")
                        {
                            Some(
                                w.queue
                                    .get(path.file_stem().unwrap().to_str().unwrap())
                                    .map(|t| t.chat_notice_due()),
                            )
                        } else {
                            None
                        };
                        (entry.file_name(), raw, due)
                    })
                })
                .collect::<Vec<_>>()
        });
        (dir, entries)
    });
    format!("{stage}: {records:#?}")
}

#[derive(Clone)]
struct NoticeWarnings(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for NoticeWarnings {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn legacy_notice_case(w: &World, watch: bool, combined: bool, owner_after_notice: Option<bool>) {
    // This helper never awaits or spawns. The subscriber is local to this case,
    // so warnings from other test threads cannot contaminate the evidence.
    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer = NoticeWarnings(warnings.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut observations = Vec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            legacy_notice_steps(w, watch, combined, owner_after_notice, &mut observations);
        }));
        if let Err(panic) = result {
            observations.push(notice_state(w, "failure"));
            eprintln!(
                "watch={watch}, combined={combined}, owner_after_notice={owner_after_notice:?}\n{}\nwarnings:\n{}",
                observations.join("\n"),
                String::from_utf8_lossy(&warnings.lock().unwrap())
            );
            std::panic::resume_unwind(panic);
        }
    });
}

fn legacy_notice_steps(
    w: &World,
    watch: bool,
    combined: bool,
    owner_after_notice: Option<bool>,
    observations: &mut Vec<String>,
) {
    let first = finished_task(w, &w.talk_id, "20260902-000000-aaaa");
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()),
        vec![w.talk_id.clone()]
    );
    observations.push(notice_state(w, "first notice"));
    let first_saved = w.queue.get(&first.id).unwrap();
    let mut legacy = w.talks.get(&w.talk_id).expect("legacy draft");
    if let Some(after) = owner_after_notice {
        let owner = "unfinished operator thought";
        if after {
            let start = legacy.pending.len() + 2;
            legacy.pending.push_str("\n\n");
            legacy.pending.push_str(owner);
            legacy.pending_breaks = Some(vec![start]);
        } else {
            let prefix = format!("{owner}\n\n");
            legacy.pending.insert_str(0, &prefix);
            legacy.pending_breaks = Some(vec![prefix.len()]);
        }
    }
    if combined {
        // Legacy metadata can combine sends even though breaks still
        // records their boundaries. Preserve that wire shape directly
        // instead of using the current metadata compaction in put.
        legacy.pending_messages = vec![talk::Message {
            display: None,
            sender: None,
            body: legacy.pending.clone(),
            attachments: Vec::new(),
            reply: None,
        }];
        std::fs::write(
            w.talks.path_of(&legacy.id),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .expect("save combined legacy format");
    } else {
        legacy.pending_messages.clear();
        w.talks.put(&mut legacy).expect("save plain legacy format");
    }
    let talks = Talks::at(w.tmp.path().join("talks"));
    assert!(
        talks
            .get(&w.talk_id)
            .unwrap()
            .pending_messages
            .iter()
            .all(|m| m.sender.is_none())
    );

    observations.push(notice_state(w, "legacy draft"));
    if watch {
        let mut talk = talks.get(&w.talk_id).unwrap();
        let report = magi::prompt::chat_watch_notice("job", "finished", "output");
        assert!(talk::queue_once(&mut talk, &talks, &report, "watch:job:finished").unwrap());
        observations.push(format!("watch immediately after queue_once: {talk:#?}"));
    } else {
        let next = finished_task(w, &w.talk_id, "20260902-000000-bbbb");
        observations.push(notice_state(w, "second task saved, before sweep"));
        assert_ne!(next.id, first.id, "filing must reserve a distinct task id");
        assert!(next.chat_notice.is_none());
        assert!(next.chat_notices.is_empty());
        assert_eq!(next.chat_notice_due().as_deref(), Some("done"));
        assert_eq!(
            chat_notify::sweep(&w.queue, &talks, w.tmp.path()),
            vec![w.talk_id.clone()]
        );
        assert_eq!(w.queue.list().len(), 2);
        let first_after = w.queue.get(&first.id).unwrap();
        assert_eq!(first_after.chat_notice, first_saved.chat_notice);
        assert_eq!(first_after.chat_notices, first_saved.chat_notices);
        for task in [&first, &next] {
            let saved = w.queue.get(&task.id).unwrap();
            assert_eq!(saved.runs, task.runs);
            assert_eq!(saved.chat_notice_due(), None);
            assert_eq!(
                saved.chat_notice.unwrap().outcome,
                ChatNoticeOutcome::Posted
            );
        }
    }
    observations.push(notice_state(w, "immediately after notification"));
    let talks = Talks::at(w.tmp.path().join("talks"));
    let mut mixed = talks.get(&w.talk_id).unwrap();
    observations.push(format!("reloaded mixed draft: {mixed:#?}"));
    assert_eq!(mixed.pending_messages[0].sender, None);
    assert_eq!(
        mixed.pending_messages.last().unwrap().sender,
        Some(talk::Sender::Magi)
    );
    assert_eq!(
        chat_notify::talks_to_start(&talks).contains(&w.talk_id),
        owner_after_notice.is_none()
    );

    // Explicit operator provenance wins even when the text is a notice.
    let notice = mixed.pending_messages.last().unwrap().body.clone();
    talk::queue(&mut mixed, &talks, &notice, Vec::new()).unwrap();
    assert!(chat_notify::talks_to_start(&talks).is_empty());
}

#[tokio::test]
async fn legacy_notices_resume_after_new_task_or_watch_notices_arrive() {
    let _home = common::home_lock().await;
    for (watch, combined) in [(false, false), (true, false), (false, true), (true, true)] {
        for owner_after_notice in [None, Some(false), Some(true)] {
            legacy_notice_case(&world(), watch, combined, owner_after_notice);
        }
    }
}

// Before filing reserved ids, the second task could inherit the first task's
// completion history and sweep would silently skip its notice. Exercise the
// collision through the legacy-draft path, not just the queue in isolation.
#[tokio::test]
async fn legacy_notices_resume_after_task_id_collision() {
    let _home = common::home_lock().await;
    for combined in [false, true] {
        for owner_after_notice in [None, Some(false), Some(true)] {
            let mut w = world();
            let draws = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let source = draws.clone();
            w.queue = w.queue.with_id_source(move || {
                match source.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 | 1 => "20260902-000000-aaaa".to_owned(),
                    2 => "20260902-000000-bbbb".to_owned(),
                    _ => panic!("unexpected additional id draw"),
                }
            });
            legacy_notice_case(&w, false, combined, owner_after_notice);
            assert_eq!(draws.load(std::sync::atomic::Ordering::SeqCst), 3);
        }
    }
}

#[cfg(unix)]
fn watch(w: &World, timeout: u64, command: &[&str], node: &str) -> Output {
    common::magi_cli(w.tmp.path())
        .args(["talk", "watch", "--timeout", &timeout.to_string(), "--"])
        .args(command)
        .current_dir(w.tmp.path())
        .env("MAGI_RUN", &w.talk_id)
        .env("MAGI_NODE", node)
        .output()
        .expect("watch")
}

#[cfg(unix)]
fn await_watch(w: &World) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        magi::chat_watch::sweep(w.tmp.path(), &w.talks);
        let draft = drafts(w);
        if draft.contains(magi::prompt::CHAT_WATCH_NOTICE_HEADING) {
            return draft;
        }
        assert!(std::time::Instant::now() < deadline, "watch never finished");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn watch_reports_once_after_reload_and_preserves_cwd_arguments_and_environment() {
    let _home = common::home_lock().await;
    let w = world();
    let out = watch(
        &w,
        5,
        &[
            "sh",
            "-c",
            "test -z \"$MAGI_RUN$MAGI_NODE\" || exit 9; test -f marker || exit 8; printf '%s' \"$1\"; printf ' stderr-tail' >&2; exit 7",
            "sh",
            "literal; $argument",
        ],
        "chat",
    );
    // The command checks its working directory; create the marker before the
    // next watch, since this first one deliberately exercises a failed check.
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(await_watch(&w).contains("status 8"));
    std::fs::write(w.tmp.path().join("marker"), "here").expect("marker");
    let out = watch(
        &w,
        5,
        &[
            "sh",
            "-c",
            "test -z \"$MAGI_RUN$MAGI_NODE\" || exit 9; test -f marker || exit 8; printf '%s' \"$1\"; printf ' stderr-tail' >&2; exit 7",
            "sh",
            "literal; $argument",
        ],
        "chat",
    );
    assert!(out.status.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let talks = Talks::at(w.tmp.path().join("talks"));
        magi::chat_watch::sweep(w.tmp.path(), &talks);
        if drafts(&w).contains("status 7") {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let draft = drafts(&w);
    assert_eq!(
        w.talks.get(&w.talk_id).unwrap().pending_messages[0].sender,
        Some(talk::Sender::Magi)
    );
    let saved = w.talks.get(&w.talk_id).expect("watch snapshot");
    let display = saved
        .pending_messages
        .last()
        .unwrap()
        .display
        .as_ref()
        .expect("watch display");
    assert!(display.facts.contains("status 7"));
    assert!(!display.facts.contains("Tell the operator"));
    assert!(display.output.as_ref().unwrap().contains("stderr-tail"));
    assert!(draft.contains("literal; $argument"));
    assert!(draft.contains("stderr-tail"));
    assert_eq!(
        chat_notify::talks_to_start(&w.talks),
        vec![w.talk_id.clone()]
    );
    magi::chat_watch::sweep(w.tmp.path(), &Talks::at(w.tmp.path().join("talks")));
    assert_eq!(drafts(&w), draft);
}

#[cfg(unix)]
#[tokio::test]
async fn watch_authorization_and_limits_are_enforced_before_launch() {
    let _home = common::home_lock().await;
    let w = world();
    assert!(!watch(&w, 5, &["true"], "implement").status.success());
    assert!(!watch(&w, 0, &["true"], "chat").status.success());
    assert!(!watch(&w, 3601, &["true"], "chat").status.success());
    assert!(magi::chat_watch::authorize(&w.talks, &w.talk_id[..8], "chat").is_err());
    assert!(magi::chat_watch::authorize(&w.talks, "", "chat").is_err());
    assert!(magi::chat_watch::authorize(&w.talks, &w.talk_id, "").is_err());
    let mut t = w.talks.get(&w.talk_id).expect("talk");
    talk::close(&mut t, &w.talks).expect("close");
    assert!(!watch(&w, 5, &["true"], "chat").status.success());
    assert!(!w.tmp.path().join("chat-watches").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn watch_timeout_output_cap_scrubbing_and_spawn_failure_are_reported() {
    let _home = common::home_lock().await;
    let w = world();
    assert!(
        watch(
            &w,
            1,
            &["sh", "-c", "printf timeout-tail; sleep 30"],
            "chat"
        )
        .status
        .success()
    );
    let draft = await_watch(&w);
    assert!(draft.contains("timed out after 1 second"));
    assert!(draft.contains("timeout-tail"));
    let w = world();
    assert!(watch(&w, 5, &["sh", "-c", "i=0; while [ $i -lt 10000 ]; do printf x; i=$((i+1)); done; printf ' ghp_123456789012345678901234 tail-end'"], "chat").status.success());
    let draft = await_watch(&w);
    assert!(draft.len() < 9500);
    assert!(draft.contains("tail-end"));
    assert!(!draft.contains("ghp_123456789012345678901234"));
    let w = world();
    assert!(
        watch(&w, 5, &["missing-watch-executable"], "chat")
            .status
            .success()
    );
    assert!(await_watch(&w).contains("startup failed"));
}

#[cfg(unix)]
#[tokio::test]
async fn watch_does_not_send_owner_drafts_or_reopen_closed_or_deleted_talks() {
    let _home = common::home_lock().await;
    let w = world();
    let mut t = w.talks.get(&w.talk_id).expect("talk");
    talk::queue(&mut t, &w.talks, "unsent owner words", Vec::new()).expect("draft");
    assert!(watch(&w, 5, &["true"], "chat").status.success());
    await_watch(&w);
    assert!(drafts(&w).starts_with("unsent owner words"));
    assert!(chat_notify::talks_to_start(&w.talks).is_empty());
    for deleted in [false, true] {
        let w = world();
        assert!(
            watch(&w, 1, &["sh", "-c", "sleep 30"], "chat")
                .status
                .success()
        );
        let mut t = w.talks.get(&w.talk_id).expect("talk");
        talk::close(&mut t, &w.talks).expect("close");
        if deleted {
            std::fs::remove_file(
                w.tmp
                    .path()
                    .join("talks")
                    .join(format!("{}.json", w.talk_id)),
            )
            .expect("delete");
        }
        await_all_watches(&w);
        if deleted {
            assert!(w.talks.get(&w.talk_id).is_err());
        } else {
            assert!(!w.talks.get(&w.talk_id).expect("talk").status.open());
            assert!(drafts(&w).is_empty());
        }
        let dir = std::fs::read_dir(w.tmp.path().join("chat-watches"))
            .expect("store")
            .filter_map(Result::ok)
            .find(|e| e.path().is_dir())
            .expect("job")
            .path();
        let rec: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("job.json")).expect("job"))
                .expect("json");
        assert_eq!(rec["delivered"], true);
    }
}

#[cfg(unix)]
fn watch_records(w: &World) -> Vec<(std::path::PathBuf, serde_json::Value)> {
    std::fs::read_dir(w.tmp.path().join("chat-watches"))
        .expect("store")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| {
            let file = e.path().join("job.json");
            let rec = serde_json::from_slice(&std::fs::read(&file).expect("job")).expect("json");
            (file, rec)
        })
        .collect()
}

#[cfg(unix)]
fn await_all_watches(w: &World) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        magi::chat_watch::sweep(w.tmp.path(), &w.talks);
        if watch_records(w).iter().all(|(_, r)| r["delivered"] == true) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "watches still running"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn watch_reservations_enforce_the_cap_across_competing_callers() {
    let _home = common::home_lock().await;
    let w = world();
    let accepted = std::thread::scope(|scope| {
        let callers: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| watch(&w, 4, &["sh", "-c", "sleep 30"], "chat")))
            .collect();
        callers
            .into_iter()
            .map(|t| t.join().expect("caller"))
            .filter(|o| o.status.success())
            .count()
    });
    assert_eq!(accepted, magi::chat_watch::MAX_CONCURRENT);
    assert_eq!(watch_records(&w).len(), accepted);
    await_all_watches(&w);
    assert_eq!(
        drafts(&w)
            .matches(magi::prompt::CHAT_WATCH_NOTICE_HEADING)
            .count(),
        accepted
    );
    assert!(
        watch(&w, 2, &["true"], "chat").status.success(),
        "completed slots are reusable"
    );
    await_all_watches(&w);
}

#[cfg(unix)]
#[tokio::test]
async fn watch_recovers_a_delivery_interruption_without_a_duplicate_draft() {
    let _home = common::home_lock().await;
    let w = world();
    assert!(watch(&w, 2, &["true"], "chat").status.success());
    let draft = await_watch(&w);
    let (file, mut record) = watch_records(&w).pop().expect("record");
    // Model a crash after the talk's keyed write but before delivery marking.
    record["delivered"] = false.into();
    std::fs::write(file, serde_json::to_vec(&record).expect("json")).expect("restore");
    magi::chat_watch::sweep(w.tmp.path(), &Talks::at(w.tmp.path().join("talks")));
    assert_eq!(drafts(&w), draft);
    assert_eq!(watch_records(&w)[0].1["delivered"], true);
}

#[cfg(unix)]
#[tokio::test]
async fn watch_retries_an_unreadable_talk_after_store_reload() {
    let _home = common::home_lock().await;
    let w = world();
    assert!(watch(&w, 2, &["true"], "chat").status.success());
    // Wait for the result and released supervisor lease without delivering
    // yet, so the first sweep must exercise the unreadable-record branch.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if watch_records(&w).iter().all(|(_, record)| {
            let pid = u32::try_from(record["supervisor"][0].as_u64().expect("supervisor pid"))
                .expect("pid");
            !record["result"].is_null() && magi::proc::pid_status(pid) == Some(false)
        }) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "watch still running");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let path = w.talks.path_of(&w.talk_id);
    let original = std::fs::read(&path).expect("original talk");
    // Malformed JSON deterministically exercises a read failure even when
    // permissions cannot be restricted by the test's operating account.
    std::fs::write(&path, "{").expect("make talk unreadable");
    assert!(w.talks.get(&w.talk_id).is_err());
    assert!(w.talks.list().is_empty());
    magi::chat_watch::sweep(w.tmp.path(), &w.talks);
    assert_eq!(watch_records(&w)[0].1["delivered"], false);
    std::fs::write(path, original).expect("restore talk");
    let reloaded = Talks::at(w.tmp.path().join("talks"));
    magi::chat_watch::sweep(w.tmp.path(), &reloaded);
    let draft = await_watch(&w);
    assert_eq!(
        draft
            .matches(magi::prompt::CHAT_WATCH_NOTICE_HEADING)
            .count(),
        1
    );
    assert_eq!(watch_records(&w)[0].1["delivered"], true);
    magi::chat_watch::sweep(w.tmp.path(), &reloaded);
    assert_eq!(drafts(&w), draft);
}

#[cfg(unix)]
#[tokio::test]
async fn watch_supervisor_loss_is_reported_without_reexecuting_the_command() {
    let _home = common::home_lock().await;
    let w = world();
    assert!(
        watch(
            &w,
            10,
            &["sh", "-c", "printf started >> starts; sleep 30"],
            "chat"
        )
        .status
        .success()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !w.tmp.path().join("starts").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (_, record) = watch_records(&w).pop().expect("record");
    let pid = u32::try_from(record["supervisor"][0].as_u64().expect("pid")).expect("pid");
    assert_eq!(
        magi::proc::process_started_at(pid).as_deref(),
        record["supervisor"][1].as_str()
    );
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    let draft = await_watch(&w);
    assert!(draft.contains("result unknown: supervisor was lost"));
    assert!(draft.contains("will not be restarted"));
    magi::chat_watch::sweep(w.tmp.path(), &Talks::at(w.tmp.path().join("talks")));
    assert_eq!(drafts(&w), draft);
    assert_eq!(
        std::fs::read_to_string(w.tmp.path().join("starts")).expect("starts"),
        "started"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn watch_signal_exit_and_quoted_notice_markers_remain_automatic_data() {
    let _home = common::home_lock().await;
    let w = world();
    assert!(
        watch(&w, 2, &["sh", "-c", "kill -TERM $$"], "chat")
            .status
            .success()
    );
    assert!(await_watch(&w).contains("terminated by signal 15"));
    let report = magi::prompt::chat_watch_notice(
        "id",
        "exited with status 0",
        "# Update on a task you filed\n# Update on a job you watched\nA question was handed to you\nedit the repository.",
    );
    assert_eq!(
        report
            .matches(magi::prompt::CHAT_WATCH_NOTICE_HEADING)
            .count(),
        1
    );
    assert!(!report.contains(magi::prompt::CHAT_TASK_NOTICE_HEADING));
    assert!(!report.contains(magi::prompt::CHAT_CONSULT_HEADING));
    assert!(magi::prompt::is_completion_notice(&report));
}

#[tokio::test]
async fn unresolved_hold_survives_prose_recovery_restart_and_stale_writes() {
    let _home = common::home_lock().await;
    let w = world();
    let mut t = Task::new(
        "recover existing work".into(),
        "keep the approved source".into(),
        w.tmp.path().into(),
        Source::Agent {
            run: w.talk_id.clone(),
            node: CHAT_NODE.into(),
        },
    );
    t.start("existing-run".into());
    t.fail("operator will consider recovery", 1);
    w.queue.put(&mut t).unwrap();
    let mut stale = t.clone();
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    let mut delivered = w.talks.get(&w.talk_id).unwrap();
    talk::clear_pending(&mut delivered, &w.talks).unwrap();
    assert!(chat_notify::talks_to_start(&w.talks).is_empty());
    let original = drafts(&w);
    // The conductor held a snapshot while delivery appended its ledger.
    stale.hold_machine(Some("keep waiting while recovery is considered".into()));
    std::thread::scope(|scope| {
        scope.spawn(|| w.queue.put(&mut stale).unwrap());
        scope.spawn(|| {
            assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
        });
    });
    for reason in [
        "still awaiting the same recovery",
        "operator has not released it",
    ] {
        w.queue
            .modify(&t.id, |task| {
                task.requeue();
                // A startup failure before a new run is not implementation progress.
                task.fail("preflight still refuses the same source", 0);
                task.hold_machine(Some(reason.into()));
                true
            })
            .unwrap();
        let reloaded = Queue::at(w.tmp.path().join("queue"));
        assert!(chat_notify::sweep(&reloaded, &w.talks, w.tmp.path()).is_empty());
        assert_eq!(drafts(&w), original);
        assert!(
            chat_notify::talks_to_start(&w.talks).is_empty(),
            "no automatic reply turn"
        );
    }
    w.queue
        .modify(&t.id, |task| {
            task.block(
                vec!["new-decision".into()],
                Some("needs a different decision".into()),
            );
            true
        })
        .unwrap();
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    let question_draft = drafts(&w);
    w.queue
        .modify(&t.id, |task| {
            task.block(
                vec!["new-decision".into(), "new-decision".into()],
                Some("rephrased question context".into()),
            );
            true
        })
        .unwrap();
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
    assert_eq!(drafts(&w), question_draft);
    w.queue
        .modify(&t.id, |task| {
            task.hold_machine(Some("same underlying hold after blocking".into()));
            true
        })
        .unwrap();
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
    w.queue
        .modify(&t.id, |task| {
            task.release_operator("test", None);
            task.start("meaningful-new-run".into());
            task.fail("new implementation failure", 1);
            true
        })
        .unwrap();
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    w.queue
        .modify(&t.id, |task| {
            task.succeed();
            true
        })
        .unwrap();
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
}

#[tokio::test]
async fn question_targets_control_automatic_turns_independently_of_labels() {
    use magi::ask::{Answer, Questions};
    use magi::conduct::{
        self, Decision, QuestionDecision, QuestionOperation, QuestionTarget, Verdict,
    };
    let _home = common::home_lock().await;
    for choices in [vec!["yes".to_owned(), "no".to_owned()], Vec::new()] {
        let w = world();
        let questions = Questions::at(w.tmp.path().join("questions"));
        let mut t = Task::new(
            "recover existing work".into(),
            "consider recovery".into(),
            w.tmp.path().into(),
            Source::Agent {
                run: w.talk_id.clone(),
                node: CHAT_NODE.into(),
            },
        );
        t.runs = vec!["source-run-a".into(), "source-run-b".into()];
        t.hold_machine(Some("recovery is under consideration".into()));
        w.queue.put(&mut t).unwrap();
        assert_eq!(
            chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
            1
        );
        let mut talk = w.talks.get(&w.talk_id).unwrap();
        talk::clear_pending(&mut talk, &w.talks).unwrap();
        let mut d = Decision {
            id: t.id.clone(),
            question: Some("Recover from source A?".into()),
            choices: choices.clone(),
            question_decision: Some(QuestionDecision {
                operation: QuestionOperation::Recover,
                target: QuestionTarget::Run {
                    id: "source-run-a".into(),
                },
            }),
            ..Decision::default()
        };
        conduct::apply(
            &w.queue,
            &questions,
            &Verdict {
                decisions: vec![d.clone()],
            },
        )
        .unwrap();
        assert_eq!(
            chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
            1
        );
        talk = w.talks.get(&w.talk_id).unwrap();
        talk::clear_pending(&mut talk, &w.talks).unwrap();
        let mut q = questions.list().pop().unwrap();
        q.answer(if choices.is_empty() {
            Answer::Text("keep held".into())
        } else {
            // Unmapped conductor labels become an executable hold. Decision
            // identity still comes from the target, independently of labels.
            assert_eq!(q.choices.len(), 1);
            let label = q.choices[0].clone();
            assert_eq!(q.actions.get(&label), Some(&magi::ask::ChoiceAction::Hold));
            assert!(!choices.contains(&label));
            Answer::Choice(label)
        })
        .unwrap();
        questions.put(&mut q).unwrap();
        w.queue
            .modify(&t.id, |task| {
                task.record_answer(q.summary.clone(), "keep held".into());
                task.unblock(&q.id);
                true
            })
            .unwrap();
        assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
        d.question = Some("Use source A for recovery, or continue waiting?".into());
        if !choices.is_empty() {
            d.choices = vec!["recover using source A".into(), "remain on hold".into()];
        }
        let queue = Queue::at(w.tmp.path().join("queue"));
        let questions = Questions::at(w.tmp.path().join("questions"));
        conduct::apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![d.clone()],
            },
        )
        .unwrap();
        assert_eq!(questions.list().len(), 1);
        assert!(chat_notify::sweep(&queue, &w.talks, w.tmp.path()).is_empty());
        assert!(drafts(&w).is_empty());
        assert!(chat_notify::talks_to_start(&w.talks).is_empty());
        let previous_decision = d.question_decision.take();
        conduct::apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![d.clone()],
            },
        )
        .unwrap();
        assert_eq!(
            questions.list().len(),
            1,
            "omitted metadata is not a new decision"
        );
        d.question_decision = previous_decision;
        d.question = Some("Recover from source B?".into());
        d.choices = choices;
        d.question_decision.as_mut().unwrap().target = QuestionTarget::Run {
            id: "source-run-b".into(),
        };
        conduct::apply(&queue, &questions, &Verdict { decisions: vec![d] }).unwrap();
        assert_eq!(questions.list().len(), 2);
        assert_eq!(chat_notify::sweep(&queue, &w.talks, w.tmp.path()).len(), 1);
        assert_eq!(
            chat_notify::talks_to_start(&w.talks),
            vec![w.talk_id.clone()]
        );
    }
}

#[tokio::test]
async fn different_settings_on_one_task_require_distinct_decisions_and_chat_turns() {
    use magi::ask::{Answer, Questions};
    use magi::conduct::{self, Decision, QuestionTarget, Verdict};
    let _home = common::home_lock().await;
    let w = world();
    std::fs::write(
        w.tmp.path().join("settings.toml"),
        "[storage]\nbackend = 'SQLite'\n[deployment]\nregion = 'east'\n",
    )
    .unwrap();
    let questions = Questions::at(w.tmp.path().join("questions"));
    let mut t = Task::new(
        "configure service".into(),
        "choose service settings".into(),
        w.tmp.path().into(),
        Source::Agent {
            run: w.talk_id.clone(),
            node: CHAT_NODE.into(),
        },
    );
    t.hold_machine(Some("configuration decisions needed".into()));
    w.queue.put(&mut t).unwrap();
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    let mut held_talk = w.talks.get(&w.talk_id).unwrap();
    talk::clear_pending(&mut held_talk, &w.talks).unwrap();
    let mut d: Decision = serde_json::from_value(serde_json::json!({
        "id": t.id,
        "question": "Which storage backend?",
        "choices": ["SQLite", "Redis"],
        "question_decision": {
            "operation": "configure",
            "target": {"kind": "setting", "path": "settings.toml", "keys": ["storage", "backend"]}
        }
    }))
    .unwrap();
    let apply = |decision: &Decision| {
        conduct::apply(
            &w.queue,
            &questions,
            &Verdict {
                decisions: vec![decision.clone()],
            },
        )
        .unwrap();
    };
    apply(&d);
    assert_eq!(questions.list().len(), 1);
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    let mut talk = w.talks.get(&w.talk_id).unwrap();
    talk::clear_pending(&mut talk, &w.talks).unwrap();
    let mut q = questions.list().pop().unwrap();
    // A setting target identifies the decision; it does not implement a
    // configuration edit. Only the safe hold can be offered without an action.
    assert_eq!(q.choices.len(), 1);
    let label = q.choices[0].clone();
    assert_eq!(q.actions.get(&label), Some(&magi::ask::ChoiceAction::Hold));
    assert!(!d.choices.contains(&label));
    q.answer(Answer::Choice(label)).unwrap();
    questions.put(&mut q).unwrap();
    w.queue
        .modify(&t.id, |task| {
            task.unblock(&q.id);
            task.hold_machine(Some("configuration decisions needed".into()));
            true
        })
        .unwrap();
    let unchanged = w.queue.get(&t.id).unwrap();
    d.question = Some("Please select the storage engine".into());
    apply(&d);
    assert_eq!(questions.list().len(), 1, "rewording is the same decision");
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
    assert!(chat_notify::talks_to_start(&w.talks).is_empty());

    // The next decision changes neither the task instruction nor held cause.
    d.question = Some("Which deployment region?".into());
    d.choices = vec!["east".into(), "west".into()];
    d.question_decision.as_mut().unwrap().target = QuestionTarget::Setting {
        path: "settings.toml".into(),
        keys: vec!["deployment".into(), "region".into()],
    };
    apply(&d);
    assert_eq!(
        questions.list().len(),
        2,
        "new field requires another decision"
    );
    let after = w.queue.get(&t.id).unwrap();
    assert_eq!(after.instruction, unchanged.instruction);
    assert_eq!(after.hold_notice_cause, unchanged.hold_notice_cause);
    assert_eq!(after.status, magi::queue::TaskStatus::Blocked);
    assert_ne!(after.blocked_by, vec![q.id]);
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    assert_eq!(
        chat_notify::talks_to_start(&w.talks),
        vec![w.talk_id.clone()]
    );
}

// Attention reports reuse task-decision destinations and automatic-turn safety.
#[tokio::test]
async fn attention_reports_coalesce_reopen_and_retry_without_replaying_prose() {
    let _home = common::home_lock().await;
    let w = world();
    let mut task = Task::new(
        "attention".into(),
        "investigate".into(),
        w.tmp.path().into(),
        Source::Human,
    );
    task.chat = Some(w.talk_id.clone());
    task.hold_machine(Some("gate exited 1".into()));
    w.queue.put(&mut task).unwrap();
    let store = magi::notices::Notices::at(w.tmp.path().join("notifications"));
    let mut chat = w.talks.get(&w.talk_id).unwrap();
    talk::close(&mut chat, &w.talks).unwrap();
    let sweep = || magi::notice_delivery::sweep(&w.queue, &w.talks, w.tmp.path(), w.tmp.path());
    sweep();
    let delivered = w.talks.get(&w.talk_id).unwrap();
    assert!(delivered.status.open());
    assert!(delivered.pending.contains("gate exited 1"));
    assert!(delivered.pending.contains(&task.id));
    assert_eq!(
        chat_notify::talks_to_start(&w.talks),
        vec![w.talk_id.clone()]
    );
    let notice = store
        .list()
        .into_iter()
        .find(|n| n.key == format!("task:{}", task.id))
        .unwrap();
    assert_eq!(notice.delivered, notice.generation);
    // Simulate termination after the talk write but before the outbox receipt.
    let path = store.root().join(format!("{}.json", notice.id));
    let mut value = serde_json::to_value(&notice).unwrap();
    value["delivered"] = 0.into();
    value["read_at"] = serde_json::Value::Null;
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    sweep();
    assert_eq!(drafts(&w), delivered.pending);
    store
        .raise(
            magi::notices::Notice::warn(&notice.key, "gate exited 1 with extra context")
                .since(notice.since)
                .link(magi::notices::Link::Task {
                    id: task.id.clone(),
                }),
        )
        .unwrap();
    sweep();
    assert_eq!(
        drafts(&w),
        delivered.pending,
        "wording cannot spend another turn"
    );
    // A second report queues beside the first: one talk to start.
    store
        .raise(
            magi::notices::Notice::warn("action:task", "retry action failed").link(
                magi::notices::Link::Task {
                    id: task.id.clone(),
                },
            ),
        )
        .unwrap();
    sweep();
    assert_eq!(chat_notify::talks_to_start(&w.talks).len(), 1);
    assert!(drafts(&w).contains("retry action failed"));
    let before = w.talks.get(&w.talk_id).unwrap().notified.len();
    store
        .raise(
            magi::notices::Notice::warn(&notice.key, "gate exited 1 again")
                .since(Some(
                    notice.since.unwrap() + std::time::Duration::from_secs(1),
                ))
                .link(magi::notices::Link::Task {
                    id: task.id.clone(),
                }),
        )
        .unwrap();
    sweep();
    assert_eq!(w.talks.get(&w.talk_id).unwrap().notified.len(), before + 1);
    sweep();
    assert_eq!(w.talks.get(&w.talk_id).unwrap().notified.len(), before + 1);
}

#[tokio::test]
async fn attention_reports_skip_released_tasks_and_delivered_questions() {
    let _home = common::home_lock().await;
    let w = world();
    let mut task = Task::new(
        "attention".into(),
        "investigate".into(),
        w.tmp.path().into(),
        Source::Human,
    );
    task.chat = Some(w.talk_id.clone());
    task.hold_machine(Some("old cause".into()));
    w.queue.put(&mut task).unwrap();
    let sweep = || magi::notice_delivery::sweep(&w.queue, &w.talks, w.tmp.path(), w.tmp.path());
    w.queue
        .modify(&task.id, |t| {
            t.release();
            true
        })
        .unwrap();
    sweep();
    assert!(drafts(&w).is_empty());
    let questions = magi::ask::Questions::at(w.tmp.path().join("questions"));
    let mut question = magi::ask::Question::new(
        task.id.clone(),
        "conduct".into(),
        "conductor".into(),
        "failure".into(),
        String::new(),
        vec![],
    );
    question.routed_chat = Some(w.talk_id.clone());
    question.abandon("forwarded to task chat");
    questions.put(&mut question).unwrap();
    w.queue
        .modify(&task.id, |t| {
            t.hold_machine(Some("new cause".into()));
            true
        })
        .unwrap();
    // Coverage is cause-aware: a question before the new hold cannot hide it.
    sweep();
    assert!(drafts(&w).contains("new cause"));
    assert!(magi::notices::covers(
        &question,
        &magi::notices::Notice::warn("task:t", "failure").about([task.id.clone()])
    ));
}

#[tokio::test]
async fn attention_reports_have_stable_run_and_global_destinations_and_preserve_owner_drafts() {
    let _home = common::home_lock().await;
    let w = world();
    let mut global = w.talks.get(&w.talk_id).unwrap();
    global.id = "notice-operations".into();
    w.talks.put(&mut global).unwrap();
    talk::queue(&mut global, &w.talks, "unfinished owner draft", Vec::new()).unwrap();
    let mut run_chat = w.talks.get(&w.talk_id).unwrap();
    run_chat.id = "notice-run-run-1".into();
    w.talks.put(&mut run_chat).unwrap();
    let dir = w.tmp.path().join("runs/run-1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("run.json"),
        serde_json::to_vec(&serde_json::json!({
            "repo": w.tmp.path(), "status": "blocked", "pr": {"url": "https://example.test/pr/1"}
        }))
        .unwrap(),
    )
    .unwrap();
    let store = magi::notices::Notices::at(w.tmp.path().join("notifications"));
    store
        .raise(magi::notices::Notice::warn(
            "global:cleanup",
            "cleanup failed",
        ))
        .unwrap();
    store
        .raise(
            magi::notices::Notice::error("run:run-1", "run blocked: missing gate")
                .link(magi::notices::Link::Run { id: "run-1".into() }),
        )
        .unwrap();
    magi::notice_delivery::sweep(&w.queue, &w.talks, w.tmp.path(), w.tmp.path());
    assert_eq!(
        chat_notify::talks_to_start(&w.talks),
        vec![run_chat.id.clone()]
    );
    assert!(
        w.talks
            .get(&global.id)
            .unwrap()
            .pending
            .contains("cleanup failed")
    );
    let draft = w.talks.get(&run_chat.id).unwrap().pending;
    assert!(draft.contains("missing gate"));
    assert!(draft.contains("https://example.test/pr/1"));
    assert!(!magi::prompt::is_completion_notice("owner text"));
    assert!(magi::prompt::is_completion_notice(&draft));
}

#[tokio::test]
async fn attention_reports_defer_unknown_runs_and_cancel_resumed_legacy_failures() {
    let _home = common::home_lock().await;
    let w = world();
    let store = magi::notices::Notices::at(w.tmp.path().join("notifications"));
    let n = store
        .raise(
            magi::notices::Notice::error("run:run-unknown", "old failure").link(
                magi::notices::Link::Run {
                    id: "run-unknown".into(),
                },
            ),
        )
        .unwrap();
    let sweep = || magi::notice_delivery::sweep(&w.queue, &w.talks, w.tmp.path(), w.tmp.path());
    sweep();
    assert!(
        store.get(&n.id).unwrap().unread(),
        "lookup failure never proves clearance"
    );
    let dir = w.tmp.path().join("runs/run-unknown");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("run.json"), serde_json::to_vec(&serde_json::json!({
        "repo": w.tmp.path(), "status": "implementing", "updated_at": n.last_at + std::time::Duration::from_secs(1)
    })).unwrap()).unwrap();
    // Legacy unread history has no receipt/generation fields.
    let path = store.root().join(format!("{}.json", n.id));
    let mut legacy = serde_json::to_value(&n).unwrap();
    legacy.as_object_mut().unwrap().remove("generation");
    legacy.as_object_mut().unwrap().remove("delivered");
    std::fs::write(path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    sweep();
    assert!(!store.get(&n.id).unwrap().unread());
    assert!(w.talks.get("notice-run-run-unknown").is_err());
    assert!(drafts(&w).is_empty());
}

#[tokio::test]
async fn attention_reports_preserve_pending_outbox_and_page_recurrences_once() {
    let _home = common::home_lock().await;
    let w = world();
    let store = magi::notices::Notices::at(w.tmp.path().join("notifications"));
    let start = jiff::Timestamp::now();
    let sent = std::cell::RefCell::new(Vec::new());
    let raise = |cause| {
        magi::notices::raise_in_with(
            w.tmp.path(),
            magi::notices::Notice::warn("global:failure", "same cause").since(Some(cause)),
            &|page| sent.borrow_mut().push(page),
        )
    };
    raise(start);
    raise(start);
    raise(start + std::time::Duration::from_secs(1));
    assert_eq!(sent.borrow().len(), 2, "one notify decision per cause");
    for i in 0..magi::notices::CAP {
        store
            .raise(magi::notices::Notice::warn(
                &format!("pending:{i}"),
                "pending",
            ))
            .unwrap();
    }
    assert_eq!(
        store.list().len(),
        magi::notices::CAP + 1,
        "pending deliveries cannot be evicted"
    );
}

#[tokio::test]
async fn attention_reports_keep_release_failures_after_merge_until_explicitly_cleared() {
    let _home = common::home_lock().await;
    let w = world();
    let mut chat = w.talks.get(&w.talk_id).unwrap();
    chat.id = "notice-run-release-run".into();
    w.talks.put(&mut chat).unwrap();
    let store = magi::notices::Notices::at(w.tmp.path().join("notifications"));
    let notice = store
        .raise(
            magi::notices::Notice::error("release-bump:release-run", "release command exited 9")
                .since(Some(jiff::Timestamp::now()))
                .link(magi::notices::Link::Url {
                    url: "https://example.test/pr/2".into(),
                }),
        )
        .unwrap();
    let dir = w.tmp.path().join("runs/release-run");
    std::fs::create_dir_all(&dir).unwrap();
    let mut record = serde_json::json!({"repo":w.tmp.path(), "status":"merged", "release_bump": {
        "problem":"command exited 9", "action_required":"retry release"},
        "updated_at":notice.last_at + std::time::Duration::from_secs(1)});
    std::fs::write(dir.join("run.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    magi::notice_delivery::sweep(&w.queue, &w.talks, w.tmp.path(), w.tmp.path());
    assert!(
        w.talks
            .get(&chat.id)
            .unwrap()
            .pending
            .contains("release command exited 9")
    );
    // A genuinely new failure can clear before its next delivery.
    let later = notice.last_at + std::time::Duration::from_secs(2);
    store
        .raise(magi::notices::Notice::error(&notice.key, "same failure").since(Some(later)))
        .unwrap();
    let pending = w.talks.get(&chat.id).unwrap().pending;
    record["release_bump"] = serde_json::json!({"problem":null,"action_required":null});
    record["updated_at"] = serde_json::to_value(later + std::time::Duration::from_secs(1)).unwrap();
    std::fs::write(dir.join("run.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    magi::notice_delivery::sweep(&w.queue, &w.talks, w.tmp.path(), w.tmp.path());
    assert_eq!(w.talks.get(&chat.id).unwrap().pending, pending);
    assert!(!store.get(&notice.id).unwrap().unread());
}
