//! End to end: a pull request text the posting gate withholds parks the run
//! on a question to the owner, and nothing is pushed or opened until it is
//! settled - by the owner's answer or by `answer_timeout` - including across
//! a restart that reloads the `RunState` from disk.
//!
//! `gh` is a shell script on `PATH` that logs its arguments and answers just
//! enough of `pr list` / `pr create` / `pr edit`; anything else exits 1, so a stray call
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
  "pr list")
    if [ -f "$FAKE_GH_PR_FILE" ]; then cat "$FAKE_GH_PR_FILE"; else echo '[]'; fi ;;
  "pr create") echo 'https://github.com/example/repo/pull/1' ;;
  "pr edit")
    if [ -f "$FAKE_GH_EDIT_FAIL_FILE" ]; then printf '%s\n' "$@" >&2; exit 1; fi ;;
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
        std::env::set_var("FAKE_GH_PR_FILE", log.with_extension("pr.json"));
        std::env::set_var("FAKE_GH_EDIT_FAIL_FILE", log.with_extension("edit-fail"));
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

/// Internal task text stays local. The independent public generator below
/// deliberately emits a credential to exercise the final posting gate.
const TASK: &str = "create note.txt; 内部タスクの報告";
const PUBLIC_DESCRIPTION: &str = "TITLE: feat(note): create note.txt\nAdds `note.txt` with the seat output. token=abcdef0123456789abcdef";

fn withheld_questions(run: &str) -> Vec<ask::Question> {
    ask::Questions::open()
        .list()
        .into_iter()
        .filter(|q| q.run == run && q.node == magi::github_text::ASK_NODE)
        .collect()
}

/// Run a fresh PR-mode run to the point where the gate parks it.
async fn park_on_question(fx: &mut common::Fixture, log: &Path) -> String {
    park_on_question_with_description(fx, log, PUBLIC_DESCRIPTION).await
}

async fn park_on_question_with_description(
    fx: &mut common::Fixture,
    log: &Path,
    description: &str,
) -> String {
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    for agent in &mut fx.config.agents {
        agent
            .env
            .insert("MOCK_CHANGE_DESCRIPTION".into(), description.into());
    }
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
    let public = state
        .change_summary
        .as_ref()
        .expect("independent public description");
    assert!(public.body.contains("token=abcdef0123456789abcdef"));
    assert!(!public.body.contains("内部タスク"));
    assert!(state.instruction.contains("内部タスク"));
    assert!(!state.candidates[0].summary.contains("abcdef0123456789"));

    let asked = withheld_questions(&state.id);
    assert_eq!(asked.len(), 1);
    assert!(asked[0].status.open());
    // The question is for the owner's eyes only: it carries the original.
    assert!(asked[0].detail.contains("abcdef0123456789"));
    assert!(asked[0].detail.contains("ORIGINAL") && asked[0].detail.contains("FALLBACK"));
    assert!(
        asked[0]
            .choices
            .contains(&magi::github_text::POST_ORIGINAL.to_owned())
    );
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
    assert_eq!(runner.state.status, RunStatus::Ready);
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
    assert_eq!(runner.state.status, RunStatus::Ready);
    assert!(runner.state.github_text.as_ref().unwrap().resolved);
    assert_eq!(withheld_questions(&run_id).len(), 1, "asked once");
}
}

common::e2e! {
async fn use_rewrite_posts_both_saved_fields_after_restart_without_the_gate() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let log = install_fake_gh(fx.tmp.path());
    let run_id = park_on_question(&mut fx, &log).await;
    // Seed a rejected rewrite to isolate the posting/resume path from the
    // implementation CLI. Unit tests exercise generation and question display.
    let mut state = RunState::load(&run_id).unwrap();
    let rewrite = magi::run::GithubTextRewrite {
        title: "日本語のリライトタイトル".into(),
        body: format!("日本語のリライト本文です。\n\nmagi:run/{run_id}"),
        failed: vec!["title-language".into(), "body-language".into()],
    };
    let g = state.github_text.as_mut().unwrap();
    assert!(!g.title, "the original title was not withheld");
    g.rewrite = Some(rewrite.clone());
    let id = g.question.clone().unwrap();
    state.save().unwrap();
    ask::Questions::open().update(&id, |q| {
        q.choices.push(magi::github_text::USE_REWRITE.into());
        q.answer(Answer::Choice(magi::github_text::USE_REWRITE.into()))
    }).unwrap();
    let mut runner = Runner::resume(&run_id).expect("resume");
    runner.execute().await.expect("execute");
    let made = creates(&log);
    assert_eq!(made.len(), 1, "calls: {:?}", gh_calls(&log));
    assert_eq!(arg_after(&made[0], "--title"), rewrite.title);
    let at = made[0].iter().position(|a| a == "--body").unwrap();
    assert_eq!(made[0][at + 1..].join("\n"), rewrite.body);
    assert_eq!(runner.state.github_text.as_ref().unwrap().choice, Some(magi::run::GithubTextChoice::Rewrite));
    assert_eq!(runner.state.status, RunStatus::Ready);
    assert!(!runner.state.events.iter().any(|e| e.message.contains(&rewrite.title) || e.message.contains("リライト本文")));
}
}

common::e2e! {
async fn use_rewrite_updates_adopted_pr_and_blocks_on_edit_failure() {
    for (same_title, fail) in [(false, false), (true, false), (true, true)] {
        let home = home_lock().await;
        let mut fx = fixture(home, Judges::Unanimous, false);
        let log = install_fake_gh(fx.tmp.path());
        let run_id = park_on_question(&mut fx, &log).await;
        let mut state = RunState::load(&run_id).unwrap();
        let rewrite = magi::run::GithubTextRewrite {
            title: "日本語のリライトタイトル".into(),
            body: format!("日本語のリライト本文です。\n\nmagi:run/{run_id}"),
            failed: vec!["title-language".into(), "body-language".into()],
        };
        let g = state.github_text.as_mut().unwrap();
        assert!(!g.title, "the original title was not withheld");
        g.rewrite = Some(rewrite.clone());
        let id = g.question.clone().unwrap();
        state.save().unwrap();
        // An operator opens the PR while the posting question is standing.
        let open = serde_json::json!([{
            "url": "https://github.com/example/repo/pull/1",
            "title": if same_title { &rewrite.title } else { "fix: existing title" },
            "baseRefName": "main",
            "isCrossRepository": false,
        }]);
        std::fs::write(log.with_extension("pr.json"), open.to_string()).unwrap();
        if fail {
            std::fs::write(log.with_extension("edit-fail"), "").unwrap();
        }
        ask::Questions::open().update(&id, |q| {
            q.choices.push(magi::github_text::USE_REWRITE.into());
            q.answer(Answer::Choice(magi::github_text::USE_REWRITE.into()))
        }).unwrap();
        let mut runner = Runner::resume(&run_id).expect("resume");
        runner.execute().await.expect("execute");
        assert!(creates(&log).is_empty(), "adopt without a duplicate PR");
        let edits: Vec<_> = gh_calls(&log).into_iter()
            .filter(|c| c.starts_with(&["pr".to_owned(), "edit".to_owned()])).collect();
        assert_eq!(edits.len(), 1, "calls: {:?}", gh_calls(&log));
        assert_eq!(arg_after(&edits[0], "--title"), rewrite.title);
        let at = edits[0].iter().position(|a| a == "--body").unwrap();
        assert_eq!(edits[0][at + 1..].join("\n"), rewrite.body);
        assert_eq!(runner.state.status, if fail { RunStatus::Blocked } else { RunStatus::Ready });
        assert_eq!(runner.state.merge.as_ref().unwrap().ok, !fail);
        // The failed command echoes all arguments. Neither events nor the
        // merge outcome may retain that text.
        assert!(!runner.state.events.iter().any(|e| e.message.contains(&rewrite.title) || e.message.contains("リライト本文")));
        assert!(!runner.state.merge.as_ref().unwrap().detail.contains("リライト"));
    }
}
}

common::e2e! {
async fn post_original_opens_the_pull_request_with_the_unmodified_text() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let log = install_fake_gh(fx.tmp.path());
    let run_id = park_on_question(&mut fx, &log).await;

    let store = ask::Questions::open();
    let id = withheld_questions(&run_id)[0].id.clone();
    store
        .update(&id, |q| {
            q.answer(Answer::Choice(magi::github_text::POST_ORIGINAL.to_owned()))
        })
        .unwrap();
    let mut runner = Runner::resume(&run_id).expect("resume");
    runner.execute().await.expect("execute");

    let made = creates(&log);
    assert_eq!(made.len(), 1, "calls: {:?}", gh_calls(&log));
    // The fake gh logs one line per line of an argument, so look at the call.
    assert!(made[0].join("\n").contains("abcdef0123456789"), "{:?}", made[0]);
    let body = made[0].join("\n");
    assert!(!body.contains("内部タスク"));
    assert!(body.contains("## Final verification"));
    assert!(body.contains("Evidence: final diff"));
    assert!(body.contains(&format!("magi:run/{run_id}")));

    let g = runner.state.github_text.as_ref().unwrap();
    assert!(g.resolved);
    assert_eq!(g.choice, Some(magi::run::GithubTextChoice::Original));
    assert_eq!(runner.state.status, RunStatus::Ready);
    // The event log names fields, never the text.
    let events = runner
        .state
        .events
        .iter()
        .filter(|e| e.node == "github-text")
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(events.contains("posting the original text for"));
    assert!(!events.contains("abcdef0123456789"));
}
}

async fn replacement_title_reaches_publication(routed: bool) {
    let mut fx = fixture(home_lock().await, Judges::Unanimous, false);
    fx.config.graph.question_routing = routed;
    let log = install_fake_gh(fx.tmp.path());
    let description = "TITLE: 日本語のタイトルです
Adds note.txt. token=abcdef0123456789abcdef";
    let run_id = park_on_question_with_description(&mut fx, &log, description).await;
    let home = magi::run::home();
    let queue = Queue::open();
    let talks = magi::talk::Talks::open();
    let store = ask::Questions::open();
    let mut chat = magi::talk::begin(&talks, &fx.config, fx.repo.clone(), Some("alpha")).unwrap();
    let mut task = Task::new(
        "Publication".into(),
        TASK.into(),
        fx.repo.clone(),
        Source::Agent {
            run: chat.id.clone(),
            node: magi::queue::CHAT_NODE.into(),
        },
    );
    task.runs.push(run_id.clone());
    queue.put(&mut task).unwrap();
    if routed {
        magi::task_chat::sweep(&queue, &talks, &store, &home);
    }
    let q = withheld_questions(&run_id).remove(0);
    if routed {
        assert_eq!(q.routed_chat.as_deref(), Some(chat.id.as_str()));
    } else {
        assert!(q.routed_chat.is_none());
        assert!(q.routed_task.is_none());
    }
    assert!(
        q.choices
            .contains(&magi::github_text::USE_MY_TEXT.to_owned())
    );
    let title = "fix: preserve the owner's replacement title";
    magi::talk::record(
        &mut chat,
        &talks,
        &format!("このタイトルで公開して: {title}"),
        vec![],
    )
    .unwrap();
    let decide = |choice: &str, title: Option<&str>| {
        let mut cli = common::magi_cli(&home);
        cli.args(["task", "control", "decide", &q.id, choice])
            .env("MAGI_RUN", &chat.id)
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
        assert!(!decide(choice, title).status.success());
        assert!(store.get(&q.id).unwrap().status.open());
    }
    let output = decide(magi::github_text::USE_MY_TEXT, Some(title));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved = store.get(&q.id).unwrap();
    assert_eq!(saved.publication_title.as_deref(), Some(title));
    assert!(saved.answer_authority.is_none());
    assert!(
        saved.thread.is_empty(),
        "the title is data, not a fabricated owner turn"
    );
    assert_eq!(
        magi::github_text::read_reply(&saved),
        magi::github_text::Reply::Title(title.into())
    );
    let mut runner = Runner::resume(&run_id).unwrap();
    runner.execute().await.unwrap();
    let made = creates(&log);
    assert_eq!(made.len(), 1);
    assert_eq!(arg_after(&made[0], "--title"), title);
    assert!(!made[0].join("\n").contains("abcdef0123456789"));
    let gate = runner.state.github_text.as_ref().unwrap();
    assert!(gate.resolved);
    assert_eq!(gate.chosen_title.as_deref(), Some(title));
    assert_eq!(runner.state.status, RunStatus::Ready);
}

common::e2e! {
async fn routed_replacement_title_reaches_publication_after_restart_without_quote_evidence() {
    replacement_title_reaches_publication(true).await;
}
}

common::e2e! {
async fn non_routed_replacement_title_reaches_publication_after_restart() {
    replacement_title_reaches_publication(false).await;
}
}
