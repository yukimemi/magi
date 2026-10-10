//! End to end: a pull request text the posting gate withholds parks the run
//! on a question to the owner, and nothing is pushed or opened until it is
//! settled - by the owner's answer or by `answer_timeout` - including across
//! a restart that reloads the `RunState` from disk.
//!
//! `gh` is a shell script on `PATH` that logs its arguments and answers just
//! enough of `pr list` / `pr create`; anything else exits 1, so a stray call
//! can never reach a real forge.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};

use common::{Judges, fixture, home_lock};
use magi::ask::{self, Answer};
use magi::config::MergeMode;
use magi::daemon;
use magi::graph::Runner;
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{Origin, RunState, RunStatus};

const FAKE_GH: &str = r#"#!/bin/sh
{
  printf '%s\n' '--call--'
  for a in "$@"; do printf '%s\n' "$a"; done
} >> "$FAKE_GH_LOG"
case "$1 $2" in
  "pr list") echo '[]' ;;
  "pr create") echo 'https://github.com/example/repo/pull/1' ;;
  *) echo "fake gh: unsupported: $*" >&2; exit 1 ;;
esac
"#;

/// Put the fake `gh` first on `PATH` and return its log.
fn install_fake_gh(dir: &Path) -> PathBuf {
    let bin = dir.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(&script, FAKE_GH).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.join("gh.log");
    std::fs::write(&log, "").unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&path));
    // SAFETY: this runs only inside the isolated child process `e2e!` spawns,
    // before the body starts any other thread that reads the environment.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("FAKE_GH_LOG", &log);
    }
    log
}

/// The `gh` invocations so far, each as its argument list.
fn gh_calls(log: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut calls: Vec<Vec<String>> = Vec::new();
    for line in text.lines() {
        if line == "--call--" {
            calls.push(Vec::new());
        } else if let Some(c) = calls.last_mut() {
            c.push(line.to_owned());
        }
    }
    calls
}

fn creates(log: &Path) -> Vec<Vec<String>> {
    gh_calls(log)
        .into_iter()
        .filter(|c| c.starts_with(&["pr".to_owned(), "create".to_owned()]))
        .collect()
}

fn arg_after(call: &[String], flag: &str) -> String {
    let i = call.iter().position(|a| a == flag).expect("flag present");
    call[i + 1].clone()
}

/// The quoted original task carries a credential, so the posting gate
/// withholds the body (sensitive data) and asks the owner.
const TASK: &str = "create note.txt, token=abcdef0123456789abcdef";

fn withheld_questions(run: &str) -> Vec<ask::Question> {
    ask::Questions::open()
        .list()
        .into_iter()
        .filter(|q| q.run == run && q.node == magi::github_text::ASK_NODE)
        .collect()
}

/// Run a fresh PR-mode run to the point where the gate parks it.
async fn park_on_question(fx: &mut common::Fixture, log: &Path) -> String {
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let mut runner = Runner::start(
        &fx.repo,
        TASK.to_owned(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .expect("start");
    runner.execute().await.expect("execute parks at merge");
    let state = &runner.state;
    assert!(state.parked, "events: {:?}", state.events);
    assert!(state.merge.is_none(), "nothing recorded while parked");
    let asked = withheld_questions(&state.id);
    assert_eq!(asked.len(), 1);
    assert!(asked[0].status.open());
    assert!(!asked[0].detail.contains("abcdef0123456789"));
    assert!(creates(log).is_empty(), "no pull request while parked");
    state.id.clone()
}

common::e2e! {
async fn an_answer_after_a_restart_opens_the_pull_request_once() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let log = install_fake_gh(fx.tmp.path());
    let run_id = park_on_question(&mut fx, &log).await;

    // The run is parked on the owner's word, so a hand-started run's
    // "hold what a retry would re-run" rule must leave its task alone.
    let queue = Queue::open();
    let mut task = Task::new(
        "withheld text".to_owned(),
        TASK.to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    task.start(run_id.clone());
    task.stall("parked on the owner's word about a withheld pull request text");
    queue.put(&mut task).unwrap();
    daemon::hold_if_runnable(&queue, &mut task);
    let after = queue.get(&task.id).unwrap();
    assert_ne!(after.status, TaskStatus::Held, "{:?}", after.hold_reason);

    // Resuming while the question is still open changes nothing.
    let mut runner = Runner::resume(&run_id).expect("resume");
    runner.execute().await.expect("execute");
    assert!(runner.state.parked);
    assert!(creates(&log).is_empty());
    assert_eq!(withheld_questions(&run_id).len(), 1);
    drop(runner);

    // The owner answers; the process restarts and rereads the run from disk.
    let store = ask::Questions::open();
    let id = withheld_questions(&run_id)[0].id.clone();
    store
        .update(&id, |q| q.answer(Answer::Choice("use fallback".to_owned())))
        .unwrap();
    let mut runner = Runner::resume(&run_id).expect("resume after restart");
    runner.execute().await.expect("execute");

    let made = creates(&log);
    assert_eq!(made.len(), 1, "calls: {:?}", gh_calls(&log));
    assert!(!arg_after(&made[0], "--body").contains("abcdef0123456789"));
    assert!(runner.state.github_text.as_ref().unwrap().resolved);
    assert_eq!(runner.state.status, RunStatus::Merged);
    assert!(runner.state.merge.as_ref().unwrap().ok);
    assert!(!runner.state.parked);
    assert_eq!(withheld_questions(&run_id).len(), 1, "asked once");
    let saved = RunState::load(&run_id).unwrap();
    assert!(saved.github_text.unwrap().resolved);
}
}

common::e2e! {
async fn silence_past_answer_timeout_opens_the_pull_request_with_redacted_text() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let log = install_fake_gh(fx.tmp.path());
    let run_id = park_on_question(&mut fx, &log).await;

    // Age the question past its deadline instead of abandoning it by hand:
    // the resume has to notice the lapse itself.
    let store = ask::Questions::open();
    let id = withheld_questions(&run_id)[0].id.clone();
    let timeout = RunState::load(&run_id).unwrap().config.graph.answer_timeout;
    store
        .update(&id, |q| {
            q.asked_at = jiff::Timestamp::from_second(
                jiff::Timestamp::now().as_second() - timeout as i64 - 5,
            )
            .unwrap();
            Ok(())
        })
        .unwrap();

    let mut runner = Runner::resume(&run_id).expect("resume");
    runner.execute().await.expect("execute");

    let q = withheld_questions(&run_id).remove(0);
    assert!(!q.status.open(), "the lapsed question is closed");
    let made = creates(&log);
    assert_eq!(made.len(), 1, "calls: {:?}", gh_calls(&log));
    assert!(!arg_after(&made[0], "--body").contains("abcdef0123456789"));
    assert_eq!(runner.state.status, RunStatus::Merged);
    assert!(runner.state.github_text.as_ref().unwrap().resolved);
    assert_eq!(withheld_questions(&run_id).len(), 1, "asked once");
}
}
