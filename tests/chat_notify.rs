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

#[cfg(unix)]
fn watch(w: &World, timeout: u64, command: &[&str], node: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["talk", "watch", "--timeout", &timeout.to_string(), "--"])
        .args(command)
        .current_dir(w.tmp.path())
        .env("MAGI_HOME", w.tmp.path())
        .env("MAGI_CONFIG_DIR", w.tmp.path().join("config"))
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
        std::thread::sleep(std::time::Duration::from_millis(1300));
        magi::chat_watch::sweep(w.tmp.path(), &w.talks);
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

fn held_chat_task(w: &World) -> Task {
    let mut task = Task::new(
        "recover measured work".into(),
        "keep the existing branch".into(),
        w.tmp.path().to_owned(),
        Source::Agent {
            run: w.talk_id.clone(),
            node: CHAT_NODE.into(),
        },
    );
    task.start("run-conflict".into());
    task.fail("cannot apply referenced commit", 1);
    task.record_answer(
        "question-recovery".into(),
        "keep held while considering recovery".into(),
    );
    task.hold_machine(Some("the operator will consider recovery".into()));
    w.queue.put(&mut task).expect("held task");
    task
}

/// Mock the scheduler consuming the automatic turn, retaining its durable keys.
fn consume_automatic_turns(w: &World) -> usize {
    let ready = chat_notify::talks_to_start(&w.talks);
    for id in &ready {
        let mut talk = w.talks.get(id).expect("talk");
        talk::clear_pending(&mut talk, &w.talks).expect("consume notice");
    }
    ready.len()
}

#[tokio::test]
async fn unresolved_recovery_reports_once_across_paraphrases_cycles_and_restart() {
    let _home = common::home_lock().await;
    let w = world();
    let task = held_chat_task(&w);
    let mut turns = 0;
    let mut drafts_added = 0;
    for i in 0..8 {
        if i > 0 {
            w.queue
                .modify(&task.id, |t| {
                    if i % 2 == 0 {
                        t.requeue();
                    }
                    t.hold_machine(Some(format!("recovery is still under consideration ({i})")));
                    true
                })
                .expect("reaffirm");
        }
        let queue = Queue::at(w.tmp.path().join("queue"));
        let talks = Talks::at(w.tmp.path().join("talks"));
        drafts_added += chat_notify::sweep(&queue, &talks, w.tmp.path()).len();
        turns += consume_automatic_turns(&w);
    }
    assert_eq!(drafts_added, 1);
    assert_eq!(turns, 1, "no additional automatic chat requests");
    w.queue
        .modify(&task.id, |t| {
            t.succeed();
            true
        })
        .expect("done");
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    assert_eq!(
        consume_automatic_turns(&w),
        1,
        "completion still starts a turn"
    );
}

#[tokio::test]
async fn blocked_causes_are_sets_and_a_new_question_still_notifies() {
    let _home = common::home_lock().await;
    let w = world();
    let task = held_chat_task(&w);
    for (ids, expected) in [
        (vec!["question-a", "dependency-a"], 1),
        (vec!["dependency-a", "question-a", "dependency-a"], 0),
        (vec!["question-b", "dependency-a"], 1),
        (vec!["question-a", "dependency-a"], 0),
    ] {
        w.queue
            .modify(&task.id, |t| {
                t.block(
                    ids.iter().map(|id| (*id).to_owned()).collect(),
                    Some(format!("regenerated reason: {ids:?}")),
                );
                true
            })
            .expect("block");
        assert_eq!(
            chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
            expected
        );
        assert_eq!(consume_automatic_turns(&w), expected);
    }
}

#[tokio::test]
async fn approved_recovery_alone_is_silent_but_a_different_failure_is_reported() {
    let _home = common::home_lock().await;
    let w = world();
    let task = held_chat_task(&w);
    chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
    consume_automatic_turns(&w);
    w.queue
        .modify(&task.id, |t| {
            t.release_operator("question", Some("approval-recovery"));
            t.hold_machine(Some("reaffirmed without work".into()));
            true
        })
        .expect("approval");
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
    w.queue
        .modify(&task.id, |t| {
            t.start("run-conflict".into());
            t.fail("gate command failed on the recovered tree", 1);
            true
        })
        .expect("new failure");
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    assert_eq!(consume_automatic_turns(&w), 1);
}

#[tokio::test]
async fn concurrent_stale_recovery_writes_keep_the_delivery_ledger_and_new_cause() {
    let _home = common::home_lock().await;
    let w = world();
    let task = held_chat_task(&w);
    let mut stale = w.queue.get(&task.id).expect("snapshot");
    chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
    consume_automatic_turns(&w);
    w.queue
        .modify(&task.id, |t| {
            t.start("run-new".into());
            t.fail("new gate failure", 1);
            true
        })
        .expect("fail");
    let new_cause = w.queue.get(&task.id).expect("current").hold_notice_cause;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let barrier2 = barrier.clone();
        let queue = &w.queue;
        scope.spawn(move || {
            barrier2.wait();
            stale.hold_machine(Some("old recovery paraphrase".into()));
            queue.put(&mut stale).expect("stale save");
        });
        barrier.wait();
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
    });
    chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
    let current = w.queue.get(&task.id).expect("current");
    assert_eq!(current.hold_notice_cause, new_cause);
    assert_eq!(current.chat_notices.len(), 2);
    assert_eq!(consume_automatic_turns(&w), 1);
    assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
}

#[tokio::test]
async fn exact_legacy_hold_is_adopted_without_replay_and_completion_survives() {
    let _home = common::home_lock().await;
    for task_recorded in [false, true] {
        let w = world();
        let task = held_chat_task(&w);
        let legacy = format!("held#{}", task.held_at.expect("held time"));
        let mut talk = w.talks.get(&w.talk_id).expect("talk");
        talk::queue_once(
            &mut talk,
            &w.talks,
            "legacy notice",
            &format!("{}:{legacy}", task.id),
        )
        .expect("legacy delivery");
        talk::clear_pending(&mut talk, &w.talks).expect("already consumed");
        let path = w.tmp.path().join("queue").join(format!("{}.json", task.id));
        let mut record = serde_json::to_value(&task).expect("serialize");
        for field in [
            "hold_notice_cause",
            "hold_notice_evidence_at",
            "chat_notices",
            "conductor_hold_context",
        ] {
            record.as_object_mut().expect("object").remove(field);
        }
        record["schema"] = 18.into();
        if task_recorded {
            record["chat_notice"] =
                serde_json::json!({"key":legacy,"at":task.updated_at,"outcome":"posted"});
        }
        std::fs::write(path, serde_json::to_vec(&record).expect("json")).expect("legacy task");
        assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
        assert_eq!(consume_automatic_turns(&w), 0);
        w.queue
            .modify(&task.id, |t| {
                t.requeue();
                t.attempts += 1;
                t.fail("cannot apply referenced commit", 2);
                t.hold_machine(Some("still unresolved".into()));
                true
            })
            .expect("rehold");
        assert!(chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).is_empty());
        w.queue
            .modify(&task.id, |t| {
                t.succeed();
                true
            })
            .expect("done");
        assert_eq!(
            chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
            1
        );
    }
}

common::e2e! {
    async fn repeated_hold_notices_invoke_the_chat_command_only_once() {
        let _home = common::home_lock().await;
        let w = world();
        magi::run::set_home(w.tmp.path().to_owned());
        let counter = w.tmp.path().join("automatic-turns");
        let cfg = Config {
            agents: vec![AgentSpec {
                id: "mock".into(), kind: AgentKind::Command, model: None,
                command: vec!["sh".into(), "-c".into(),
                    "cat >/dev/null; echo turn >> \"$TURN_COUNTER\"; echo notice-response".into()],
                extra_args: Vec::new(),
                env: BTreeMap::from([("TURN_COUNTER".into(), counter.to_string_lossy().into_owned())]),
                prompt_delivery: None,
            }],
            ..Config::default()
        };
        let task = held_chat_task(&w);
        for i in 0..5 {
            if i > 0 {
                w.queue.modify(&task.id, |t| {
                    t.requeue();
                    t.hold_machine(Some(format!("keep held pending recovery decision {i}")));
                    true
                }).expect("rehold");
            }
            chat_notify::sweep(&w.queue, &w.talks, w.tmp.path());
            for id in chat_notify::talks_to_start(&w.talks) {
                let lease = w.talks.claim_turn(&id).expect("claim").expect("lease");
                magi::consult::drain_turns(&w.talks, &id, lease, &cfg).await.expect("automatic turn");
            }
        }
        assert_eq!(std::fs::read_to_string(&counter).expect("invocations").lines().count(), 1);
        let talk = w.talks.get(&w.talk_id).expect("talk");
        assert_eq!(talk.turns.iter().filter(|t| t.who == talk::Who::Agent).count(), 1);
        assert!(talk.pending.is_empty());
    }
}

#[tokio::test]
async fn interrupted_hold_delivery_repairs_the_ledger_after_the_turn_was_consumed() {
    let _home = common::home_lock().await;
    let w = world();
    let task = held_chat_task(&w);
    let key = task.chat_notice_due().expect("due");
    let mut talk = w.talks.get(&w.talk_id).expect("talk");
    talk::queue_once(
        &mut talk,
        &w.talks,
        "already delivered",
        &format!("{}:{key}", task.id),
    )
    .expect("delivery");
    talk::clear_pending(&mut talk, &w.talks).expect("consumed");
    let queue = Queue::at(w.tmp.path().join("queue"));
    assert!(chat_notify::sweep(&queue, &w.talks, w.tmp.path()).is_empty());
    assert!(
        queue
            .get(&task.id)
            .expect("repaired")
            .chat_notices
            .contains(&key)
    );
    assert_eq!(consume_automatic_turns(&w), 0);
}

#[tokio::test]
async fn a_legacy_alias_cannot_swallow_a_later_failure_with_the_same_hold_timestamp() {
    let _home = common::home_lock().await;
    let w = world();
    let task = held_chat_task(&w);
    let legacy = format!("held#{}", task.held_at.expect("time"));
    let mut talk = w.talks.get(&w.talk_id).expect("talk");
    talk::queue_once(
        &mut talk,
        &w.talks,
        "old hold",
        &format!("{}:{legacy}", task.id),
    )
    .expect("legacy");
    talk::clear_pending(&mut talk, &w.talks).expect("consumed");
    w.queue
        .modify(&task.id, |t| {
            t.chat_notice_legacy = Some((legacy, t.chat_notice_key().expect("cause")));
            t.fail("a different failure on the same held run", 1);
            true
        })
        .expect("new failure");
    assert_eq!(w.queue.get(&task.id).expect("task").held_at, task.held_at);
    assert_eq!(
        chat_notify::sweep(&w.queue, &w.talks, w.tmp.path()).len(),
        1
    );
    assert_eq!(consume_automatic_turns(&w), 1);
}
