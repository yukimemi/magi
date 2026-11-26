//! End-to-end regression tests for evidence-backed `VerifiedNoop` vs
//! incomplete, contradictory, or unsupported no-change claims across
//! `graph::Runner` and `daemon::finish_attempt`.
mod common;

use std::collections::BTreeMap;

use common::{Judges, fixture, home_lock};
use magi::{
    config::{AgentKind, AgentSpec},
    daemon,
    graph::Runner,
    queue::{Queue, Source, Task, TaskStatus},
    run::{Origin, RunState, RunStatus},
};

fn sh_agent(id: &str, script: &std::path::Path, env: BTreeMap<String, String>) -> AgentSpec {
    AgentSpec {
        id: id.to_owned(),
        kind: AgentKind::Command,
        model: None,
        command: vec!["sh".to_owned(), script.to_string_lossy().into_owned()],
        extra_args: Vec::new(),
        env,
        prompt_delivery: None,
        max_turn_secs: None,
    }
}

fn head_sha(repo: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("git rev-parse HEAD");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn script_printing_summary(
    dir: &std::path::Path,
    name: &str,
    summary_body: &str,
) -> std::path::PathBuf {
    let path = dir.join(name);
    let body_file = dir.join(format!("{name}.txt"));
    std::fs::write(&body_file, format!("## SUMMARY\n{summary_body}\n")).unwrap();
    let script = format!(
        "#!/bin/sh\ncat '{}'\nexit 0\n",
        body_file.to_string_lossy().replace('\'', "'\\''")
    );
    std::fs::write(&path, script).unwrap();
    path
}

#[tokio::test]
async fn operational_regression_incomplete_investigation_fails_and_retries_rather_than_verified_noop()
 {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);

    let quoted = "NO CHANGE NEEDED: Not established. No source changes, commit, or reproduction checks were completed. The reported Git configuration lock contention remains unresolved.";
    let script = script_printing_summary(fx.tmp.path(), "incomplete-noop.sh", quoted);
    fx.config.agents = vec![sh_agent("alpha", &script, BTreeMap::new())];
    fx.config.graph.implementers = 1;

    let mut runner = Runner::start(
        &fx.repo,
        "Investigate Git configuration lock contention".to_owned(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .expect("start runner");

    let run_result = runner.execute().await;
    assert!(
        run_result.is_err(),
        "incomplete investigation must fail the run rather than succeed as VerifiedNoop"
    );

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::Failed);
    assert_eq!(saved.candidates.len(), 1);
    assert!(saved.candidates[0].empty);
    assert!(saved.candidates[0].verified_noop.is_none());
    let cand_fail = saved.candidates[0]
        .failed
        .as_deref()
        .expect("rejected no-op claim must record candidate failure reason");
    assert!(cand_fail.contains("no-op claim rejected"), "{cand_fail}");
    assert!(
        saved
            .events
            .iter()
            .any(|e| e.message.contains("rejected NO CHANGE NEEDED claim")),
        "expected rejection event in {:?}",
        saved.events
    );

    let queue = Queue::at(fx.tmp.path().join("queue"));
    let mut task = Task::new(
        "lock contention".to_owned(),
        "Investigate Git configuration lock contention".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    task.start(saved.id.clone());
    queue.put(&mut task).unwrap();

    daemon::finish_attempt(3, &queue, &mut task, &saved, &[], run_result);
    let settled = queue.get(&task.id).unwrap();
    assert_eq!(
        settled.status,
        TaskStatus::Failed,
        "incomplete investigation must settle as Failed (retrying under normal attempt budget), never Held as a no-change approval or Done"
    );
    assert_eq!(settled.attempts, 1);
    assert!(settled.hold_reason.is_none());
}

#[tokio::test]
async fn valid_evidence_backed_noop_reaches_verified_noop_and_holds_for_owner_without_auto_closing()
{
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let base = head_sha(&fx.repo);

    let valid = format!(
        r#"NO CHANGE NEEDED: {{"status":"already_satisfied","requirements":[{{"requirement":"fixture README is already committed on base","satisfied":true,"evidence":["c1","c2"]}}],"checks":[{{"id":"c1","kind":"commit","target":"{base}","method":"git merge-base --is-ancestor {base} HEAD","observed":"exit 0; initial commit is on base","passed":true}},{{"id":"c2","kind":"path","target":"README.md:1","method":"read README.md","observed":"contains # fixture header","passed":true}}],"unresolved":[]}}"#
    );
    let script = script_printing_summary(fx.tmp.path(), "valid-noop.sh", &valid);
    fx.config.agents = vec![sh_agent("alpha", &script, BTreeMap::new())];
    fx.config.graph.implementers = 1;

    let mut runner = Runner::start(
        &fx.repo,
        "Ensure README header exists".to_owned(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .expect("start runner");

    let run_result = runner.execute().await;
    assert!(
        run_result.is_ok(),
        "evidence-backed no-op must succeed as VerifiedNoop: {run_result:?}"
    );

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::VerifiedNoop);
    assert_eq!(saved.candidates.len(), 1);
    assert!(saved.candidates[0].empty);
    assert!(saved.candidates[0].failed.is_none());
    let evidence = saved.candidates[0]
        .verified_noop
        .as_deref()
        .expect("valid no-op must retain formatted evidence");
    assert!(evidence.contains(&base), "{evidence}");
    assert!(evidence.contains("README.md"), "{evidence}");
    assert!(matches!(
        magi::verdict::parse_verified_noop(evidence),
        Some(Ok(_))
    ));

    let queue = Queue::at(fx.tmp.path().join("queue"));
    let mut task = Task::new(
        "readme header".to_owned(),
        "Ensure README header exists".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    task.start(saved.id.clone());
    queue.put(&mut task).unwrap();

    daemon::finish_attempt(3, &queue, &mut task, &saved, &[], run_result);
    let settled = queue.get(&task.id).unwrap();
    assert_eq!(
        settled.status,
        TaskStatus::Held,
        "valid VerifiedNoop must hold for human confirmation and never auto-close to Done"
    );
    assert_eq!(settled.attempts, 1);
    assert_eq!(
        settled.hold_notice_cause.as_ref().map(|c| c.stage.as_str()),
        Some("verified_noop")
    );
    let diag = settled.diagnostic.as_deref().unwrap_or("");
    assert!(diag.contains("agent-verified no-op"), "{diag}");
}

#[tokio::test]
async fn candidate_disagreement_between_valid_noop_and_incomplete_noop_fails_the_run() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    let base = head_sha(&fx.repo);

    let valid = format!(
        r#"NO CHANGE NEEDED: {{"status":"already_satisfied","requirements":[{{"requirement":"fixture README exists","satisfied":true,"evidence":["c1"]}}],"checks":[{{"id":"c1","kind":"commit","target":"{base}","method":"git merge-base --is-ancestor {base} HEAD","observed":"exit 0; commit is on base","passed":true}}],"unresolved":[]}}"#
    );
    let incomplete = r#"NO CHANGE NEEDED: {"status":"incomplete","requirements":[{"requirement":"fixture README exists","satisfied":false,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"README.md","method":"read README.md","observed":"could not finish checking","passed":false}],"unresolved":["verification incomplete"]}"#;

    let script_a = script_printing_summary(fx.tmp.path(), "valid-a.sh", &valid);
    let script_b = script_printing_summary(fx.tmp.path(), "incomplete-b.sh", incomplete);
    fx.config.agents = vec![
        sh_agent("alpha", &script_a, BTreeMap::new()),
        sh_agent("beta", &script_b, BTreeMap::new()),
    ];
    fx.config.graph.implementers = 2;

    let mut runner = Runner::start(
        &fx.repo,
        "Check README".to_owned(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .expect("start runner");

    let run_result = runner.execute().await;
    assert!(
        run_result.is_err(),
        "disagreement between valid and incomplete no-op candidates must fail the run"
    );

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::Failed);
    assert!(saved.candidates[0].verified_noop.is_some());
    assert!(saved.candidates[1].verified_noop.is_none());
    assert!(saved.candidates[1].failed.is_some());
}

#[tokio::test]
async fn contradictory_commit_evidence_fails_run_and_does_not_hold_as_noop() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);

    let fabricated = r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"requested fix already landed","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"commit","target":"deadbeef1234","method":"git merge-base --is-ancestor deadbeef1234 HEAD","observed":"exit 0; commit is on base","passed":true}],"unresolved":[]}"#;
    let script = script_printing_summary(fx.tmp.path(), "fabricated-sha.sh", fabricated);
    fx.config.agents = vec![sh_agent("alpha", &script, BTreeMap::new())];
    fx.config.graph.implementers = 1;

    let mut runner = Runner::start(
        &fx.repo,
        "Verify fabricated commit rejection".to_owned(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .expect("start runner");

    let run_result = runner.execute().await;
    assert!(run_result.is_err());

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::Failed);
    assert!(saved.candidates[0].verified_noop.is_none());
    let cand_fail = saved.candidates[0].failed.as_deref().unwrap_or("");
    assert!(
        cand_fail.contains("does not exist in git history"),
        "{cand_fail}"
    );
}

#[tokio::test]
async fn candidate_with_real_changes_keeps_normal_review_path_despite_noop_marker() {
    let guard = home_lock().await;
    let mut fx = fixture(guard, Judges::Unanimous, false);
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    fx.config.graph.review_rounds = 1;

    // Replace only the first agent's implement branch by wrapping the existing
    // mock script: when called as the implementer, commit a real file AND emit
    // an invalid `NO CHANGE NEEDED:` summary. All other roles delegate to the
    // standard fixture mock script.
    let orig_script = fx.config.agents[0].command[1].clone();
    let wrapper = fx.tmp.path().join("changed-with-noop-marker.sh");
    let script = format!(
        r#"#!/bin/sh
p="$1"
if grep -q "You are implementing a change in an isolated git worktree" "$p"; then
  echo "real change" > changed.txt
  git add -A >/dev/null 2>&1
  git commit -q -m "real change" >/dev/null 2>&1
  printf '## SUMMARY\nNO CHANGE NEEDED: Not established.\n'
  exit 0
fi
exec sh '{}' "$1"
"#,
        orig_script.replace('\'', "'\\''")
    );
    std::fs::write(&wrapper, script).unwrap();
    fx.config.agents[0].command[1] = wrapper.to_string_lossy().into_owned();

    let mut runner = Runner::start(
        &fx.repo,
        "Make a real change".to_owned(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .expect("start runner");

    runner
        .execute()
        .await
        .expect("a candidate with real changes must proceed through review and gate");

    let saved = RunState::load(&runner.state.id).expect("reload run");
    assert_eq!(saved.status, RunStatus::Ready);
    assert!(!saved.candidates[0].empty);
    assert!(saved.candidates[0].failed.is_none());
    assert!(saved.candidates[0].verified_noop.is_none());
    assert!(
        !saved.reviews.is_empty(),
        "normal review loop must still run on a changed candidate"
    );
}
