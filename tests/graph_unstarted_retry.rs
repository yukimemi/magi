//! An implementation seat whose turn ends cleanly without starting the work
//! (no tool call, worktree unchanged) is asked once more, and only once.
//!
//! The implementer here is a `command` agent relaying an `omp --mode=json`
//! stream, which is where magi reads tool activity from. Everything else is
//! the standard fixture mock.
mod common;

use std::path::{Path, PathBuf};

use common::{Fixture, Judges, fixture, home_lock};
use magi::{
    graph::Runner,
    run::{Origin, RunState, RunStatus},
};

const INCOMPLETE: &str = "## SUMMARY\n\nImplementation incomplete. No files were changed, no commit was created, and no verification was performed.";
const DONE: &str = "## SUMMARY\nTITLE: feat(note): add the note\n- adds note.txt";

/// An omp event stream that ends on `text` after `tools` tool calls.
fn omp_stream(text: &str, tools: usize) -> String {
    let mut lines =
        vec![serde_json::json!({"type": "session", "version": 3, "id": "fixture"}).to_string()];
    for i in 0..tools {
        lines.push(
            serde_json::json!({
                "type": "tool_execution_start",
                "toolCallId": format!("t{i}"),
                "toolName": "read",
            })
            .to_string(),
        );
    }
    lines.push(
        serde_json::json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
                "stopReason": "stop",
            },
        })
        .to_string(),
    );
    lines.join("\n") + "\n"
}

fn sh_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Replace the first agent's implement turns with `body`, a shell fragment
/// that sees `$n` (which implement turn this is, from 1) and `$FIX_DIR`
/// (holding `idle.jsonl`, a tool-less reply, and `worked.jsonl`). Every
/// implement prompt is kept as `$FIX_DIR/turn-<n>.md`. Other nodes, and the
/// other agents, stay on the fixture mock.
fn wrap_implementer(fx: &mut Fixture, idle_text: &str, body: &str) -> PathBuf {
    let dir = fx.tmp.path().join("unstarted");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("idle.jsonl"), omp_stream(idle_text, 0)).unwrap();
    std::fs::write(dir.join("worked.jsonl"), omp_stream(DONE, 2)).unwrap();
    let script = format!(
        r#"#!/bin/sh
if [ "$MAGI_NODE" = "implement" ]; then
  n=0
  [ -f "$FIX_DIR/count" ] && n=$(cat "$FIX_DIR/count")
  n=$((n + 1))
  printf '%s' "$n" > "$FIX_DIR/count"
  cp "$MAGI_PROMPT_FILE" "$FIX_DIR/turn-$n.md"
  {body}
fi
exec sh "$FIX_MOCK"
"#
    );
    let wrapper = fx.tmp.path().join("unstarted-implementer.sh");
    std::fs::write(&wrapper, script).unwrap();
    let agent = &mut fx.config.agents[0];
    agent
        .env
        .insert("FIX_MOCK".to_owned(), sh_path(Path::new(&agent.command[1])));
    agent.env.insert("FIX_DIR".to_owned(), sh_path(&dir));
    agent.command[1] = wrapper.to_string_lossy().into_owned();
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    fx.config.graph.review_rounds = 1;
    dir
}

/// How many implement turns the wrapped agent was given.
fn turns(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("count"))
        .map(|s| s.trim().parse().expect("count"))
        .unwrap_or(0)
}

async fn start(fx: &Fixture) -> Runner {
    Runner::start(
        &fx.repo,
        "create note.txt".to_owned(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .expect("start runner")
}

fn implement_events(state: &RunState) -> Vec<&str> {
    state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect()
}

const IDLE_ALWAYS: &str = r#"cat "$FIX_DIR/idle.jsonl"; exit 0"#;

#[tokio::test]
async fn a_seat_that_answers_without_starting_is_resumed_once_and_then_works() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let dir = wrap_implementer(
        &mut fx,
        INCOMPLETE,
        r#"if [ "$n" -le 1 ]; then cat "$FIX_DIR/idle.jsonl"; exit 0; fi
  echo "the note" > note.txt
  cat "$FIX_DIR/worked.jsonl"
  exit 0"#,
    );

    let mut runner = start(&fx).await;
    runner
        .execute()
        .await
        .expect("the retried seat's work goes on");

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::Ready);
    assert_eq!(turns(&dir), 2, "one first turn and exactly one retry");
    let c = &saved.candidates[0];
    assert!(!c.empty && c.failed.is_none(), "{c:?}");
    assert_eq!(c.files, 1);
    assert!(saved.unstarted_retries.contains(&c.label));
    assert!(saved.handovers.is_empty(), "{:?}", saved.handovers);
    assert!(
        implement_events(&saved)
            .iter()
            .any(|m| m.contains("without starting the work")
                && m.contains("resuming the conversation once")),
        "{:?}",
        implement_events(&saved)
    );

    // The retry is the seat's own conversation, not the task sent again.
    let second = std::fs::read_to_string(dir.join("turn-2.md")).unwrap();
    assert!(
        second.contains("not accepted as this step's result"),
        "{second}"
    );
    assert!(
        !second.contains("You are implementing a change"),
        "{second}"
    );
}

#[tokio::test]
async fn a_seat_that_never_starts_gets_one_retry_and_a_recorded_reason() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let dir = wrap_implementer(&mut fx, INCOMPLETE, IDLE_ALWAYS);
    // Nobody to hand the seat to.
    fx.config.agents.truncate(1);

    let mut runner = start(&fx).await;
    runner.execute().await.expect_err("nothing was implemented");

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::Failed);
    assert_eq!(turns(&dir), 2, "never a second retry");
    let c = &saved.candidates[0];
    let reason = c.failed.as_deref().expect("an explicit failure reason");
    assert!(reason.contains("without starting the work"), "{reason}");
    assert!(reason.contains("again when asked once more"), "{reason}");
    assert!(c.verified_noop.is_none());
    assert!(saved.quota.is_empty(), "{:?}", saved.quota);
}

#[tokio::test]
async fn a_seat_that_never_starts_is_handed_to_the_next_agent_after_its_retry() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let dir = wrap_implementer(&mut fx, INCOMPLETE, IDLE_ALWAYS);

    let mut runner = start(&fx).await;
    runner.execute().await.expect("the next agent implements");

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::Ready);
    assert_eq!(turns(&dir), 2, "the first agent: one turn, one retry");
    assert_eq!(saved.handovers.len(), 1, "{:?}", saved.handovers);
    let h = &saved.handovers[0];
    assert_eq!(
        (h.node.as_str(), h.from.as_str(), h.to.as_str()),
        ("implement", "alpha", "beta")
    );
    assert!(h.reason.contains("without starting the work"), "{h:?}");
    let c = &saved.candidates[0];
    assert!(!c.empty && c.failed.is_none(), "{c:?}");
    assert_eq!(c.agent, "beta");
}

/// The retry is recorded before the agent is asked, so a run that restarts
/// with it already spent does not grant another one.
#[tokio::test]
async fn a_retry_already_spent_before_a_restart_is_not_granted_again() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let dir = wrap_implementer(&mut fx, INCOMPLETE, IDLE_ALWAYS);
    fx.config.agents.truncate(1);

    let mut runner = start(&fx).await;
    runner.state.unstarted_retries.insert('A');
    runner.execute().await.expect_err("nothing was implemented");

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.candidates[0].label, 'A');
    assert_eq!(turns(&dir), 1, "the saved retry was the only one");
    let reason = saved.candidates[0].failed.as_deref().expect("a reason");
    assert!(reason.contains("without starting the work"), "{reason}");
}

/// A reply that used no tool but whose no-change claim verifies did answer
/// the task: it is never asked again.
#[tokio::test]
async fn a_verified_no_change_reply_is_never_retried() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let base = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&fx.repo)
            .output()
            .expect("git rev-parse HEAD")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let claim = format!(
        r#"## SUMMARY
NO CHANGE NEEDED: {{"status":"already_satisfied","requirements":[{{"requirement":"fixture README is already committed on base","satisfied":true,"evidence":["c1","c2"]}}],"checks":[{{"id":"c1","kind":"commit","target":"{base}","method":"git merge-base --is-ancestor {base} HEAD","observed":"exit 0; initial commit is on base","passed":true}},{{"id":"c2","kind":"path","target":"README.md:1","method":"read README.md","observed":"contains # fixture header","passed":true}}],"unresolved":[]}}"#
    );
    let dir = wrap_implementer(&mut fx, &claim, IDLE_ALWAYS);
    fx.config.agents.truncate(1);

    let mut runner = start(&fx).await;
    runner.execute().await.expect("a verified no-op");

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::VerifiedNoop);
    assert_eq!(turns(&dir), 1);
    assert!(saved.unstarted_retries.is_empty());
    assert!(saved.candidates[0].verified_noop.is_some());
}

/// Edits left in the tree are work, whatever the stream says about tools:
/// they are kept as the candidate and the seat is not asked again.
#[tokio::test]
async fn a_reply_that_left_edits_in_the_tree_is_never_retried() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let dir = wrap_implementer(
        &mut fx,
        INCOMPLETE,
        r#"echo "partial" > note.txt
  cat "$FIX_DIR/idle.jsonl"
  exit 0"#,
    );

    let mut runner = start(&fx).await;
    let _ = runner.execute().await;

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(turns(&dir), 1);
    assert!(saved.unstarted_retries.is_empty());
    let c = &saved.candidates[0];
    assert!(!c.empty && c.files == 1, "{c:?}");
}

/// A rate limit on the retry is a rate limit: the existing machine-outage
/// path takes it, and it is not rewritten into the seat's own failure.
#[tokio::test]
async fn a_quota_on_the_retry_stays_a_quota() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let dir = wrap_implementer(
        &mut fx,
        INCOMPLETE,
        r#"if [ "$n" -le 1 ]; then cat "$FIX_DIR/idle.jsonl"; exit 0; fi
  printf '{"is_error":true,"terminal_reason":"api_error","result":"You'\''ve hit your session limit","session_id":"quota-test"}\n'
  exit 1"#,
    );
    fx.config.agents.truncate(1);

    let mut runner = start(&fx).await;
    let _ = runner.execute().await;

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(turns(&dir), 2);
    assert!(
        saved
            .quota
            .iter()
            .any(|q| q.node == "implement" && q.seat == "impl-A"),
        "{:?}",
        saved.quota
    );
    assert!(saved.resource_block.is_some(), "the run stops resumably");
    assert!(saved.candidates[0].failed.is_none());
    assert!(
        !implement_events(&saved)
            .iter()
            .any(|m| m.contains("again when asked once more")),
        "{:?}",
        implement_events(&saved)
    );
}
