//! Repair a held PR in place, retaining evidence and requiring a new decision.
#![cfg(unix)]
mod common;

use magi::config::MergeMode;
use magi::{
    ask::Questions,
    graph::Runner,
    land::{APPROVAL_NODE, REPAIR},
    run::{MergeOutcome, MergeRepairPhase, Origin, RunState, RunStatus},
};
use std::{os::unix::fs::PermissionsExt, path::Path};

fn forge(dir: &Path, branch: &str) {
    let bin = dir.join("forge");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(&script, r#"#!/bin/sh
printf '%s\n' "$*" >> "$REPAIR_FORGE/calls"
sha=$(git ls-remote --refs origin "refs/heads/$REPAIR_BRANCH" 2>/dev/null | cut -f1)
case "$1 $2" in
 'pr view') printf '{"url":"https://github.com/example/repo/pull/42","number":42,"state":"OPEN","title":"feat: existing work","reviews":[],"comments":[],"mergeStateStatus":"CLEAN","headRefOid":"%s","baseRefName":"main","autoMergeRequest":null,"headRefName":"%s","isCrossRepository":false}\n' "$sha" "$REPAIR_BRANCH" ;;
 'api graphql') printf '{"data":{"repository":{"pullRequest":{"commits":{"nodes":[{"commit":{"oid":"%s","statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]}}}}}\n' "$sha" ;;
 'api repos/{owner}/{repo}/pulls/42/comments') echo '[]' ;;
 'pr merge') case "$*" in *--disable-auto*) exit 0 ;; *) exit 99 ;; esac ;;
 'pr comment') exit 0 ;;
 *) exit 98 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    // SAFETY: each e2e test runs in an isolated child, before its driver starts.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("REPAIR_FORGE", &bin);
        std::env::set_var("REPAIR_BRANCH", branch);
    }
    let agent = dir.join("mock-agent.sh");
    let original = std::fs::read_to_string(&agent).unwrap();
    let prefix = r#"#!/bin/sh
if grep -q 'Land round' "$MAGI_PROMPT_FILE"; then
  cp "$MAGI_PROMPT_FILE" "$REPAIR_FORGE/evidence"
  echo fix >> "$REPAIR_FORGE/fixes"
  if test ! -f "$REPAIR_FORGE/noop"; then
    echo repaired >> repaired.txt
    git add repaired.txt
    git commit -q -m 'fix: repair held findings'
  fi
  echo 'repair turn completed'
  exit 0
fi
"#;
    std::fs::write(agent, format!("{prefix}\n{original}")).unwrap();
}

async fn held_pr(noop: bool, keep_findings: bool) -> (common::Fixture, Runner, String) {
    let home = common::home_lock().await;
    let fx = common::fixture_that_never_clears(home, 1);
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".into(),
        fx.config.clone(),
        Origin::operator(),
    )
    .await
    .unwrap();
    runner.execute().await.unwrap();
    assert_eq!(runner.state.status, RunStatus::Ready);
    let w = runner.state.winner().unwrap().clone();
    let push = magi::git::push(&w.worktree, "origin", &w.branch)
        .await
        .unwrap();
    assert!(push.ok());
    forge(fx.tmp.path(), &w.branch);
    if noop {
        std::fs::write(fx.tmp.path().join("forge/noop"), "").unwrap();
    }
    runner.state.config.graph.land_approval = false;
    runner.state.config.merge.mode = MergeMode::Pr;
    if !keep_findings {
        for a in &mut runner.state.config.agents {
            a.env.remove("MOCK_ALWAYS_FINDING");
        }
    }
    runner.state.merge = Some(MergeOutcome {
        mode: MergeMode::Pr,
        ok: true,
        detail: "https://github.com/example/repo/pull/42".into(),
        empty: false,
    });
    runner.state.status = RunStatus::Landing;
    runner.state.save().unwrap();
    runner = Runner::resume(&runner.state.id).unwrap();
    runner.execute().await.unwrap();
    assert!(
        runner.state.parked,
        "events: {:?}; calls: {}",
        runner.state.events,
        std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap()
    );
    let id = runner
        .state
        .land_approval
        .as_ref()
        .unwrap()
        .question
        .clone();
    assert!(
        Questions::open()
            .get(&id)
            .unwrap()
            .choices
            .iter()
            .any(|c| c == REPAIR)
    );
    let answer = std::process::Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["answer", &id, "--reply", REPAIR])
        .env("MAGI_HOME", magi::run::home())
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .output()
        .unwrap();
    assert!(
        answer.status.success(),
        "{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    let acknowledgement = String::from_utf8(answer.stdout).unwrap();
    assert!(acknowledgement.contains("this answer command has not started repair"));
    assert!(acknowledgement.contains("Without one"));
    assert!(acknowledgement.contains(&format!("magi run --resume {}", runner.state.id)));
    assert!(
        !fx.tmp.path().join("forge/fixes").exists(),
        "no daemon or driver is running in this scenario"
    );
    assert_eq!(
        Questions::open().get(&id).unwrap().resolution().as_deref(),
        Some(REPAIR)
    );
    let duplicate = std::process::Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["answer", &id, "--reply", REPAIR])
        .env("MAGI_HOME", magi::run::home())
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    (fx, runner, id)
}

async fn assert_fresh_decision(noop: bool) {
    let (fx, runner, old_question) = held_pr(noop, false).await;
    let id = runner.state.id.clone();
    let old_head = runner.state.land_approval.as_ref().unwrap().head.clone();
    let mut resumed = Runner::resume(&id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.id, id);
    assert_eq!(
        resumed.state.status,
        RunStatus::Landing,
        "{:?}",
        resumed.state.events
    );
    assert!(resumed.state.parked);
    assert_eq!(
        resumed.state.merge_repairs[0].phase,
        MergeRepairPhase::Complete
    );
    assert_eq!(resumed.state.merge_repairs[0].question, old_question);
    assert!(
        !resumed.state.merge_repairs[0].reviews[0].reviews[0]
            .findings
            .is_empty()
    );
    assert!(resumed.state.reviews[0].clean);
    assert!(resumed.state.gate_status().ok());
    let next = resumed.state.land_approval.as_ref().unwrap();
    assert_ne!(next.question, old_question);
    assert_eq!(next.head == old_head, noop);
    assert!(Questions::open().get(&next.question).unwrap().status.open());
    assert_eq!(resumed.state.pr.as_ref().unwrap().number, 42);
    let evidence = std::fs::read_to_string(fx.tmp.path().join("forge/evidence")).unwrap();
    assert!(evidence.contains("R1-1-1"));
    assert!(evidence.contains("create fixed.txt"));
    assert!(evidence.contains("mock review"));
    assert_eq!(
        std::fs::read_to_string(fx.tmp.path().join("forge/fixes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(
        calls
            .lines()
            .filter(|l| l.starts_with("pr merge"))
            .all(|l| l.contains("--disable-auto"))
    );
    // Re-enter the new unanswered decision: no second repair or task/run.
    let mut again = Runner::resume(&id).unwrap();
    again.execute().await.unwrap();
    assert_eq!(again.state.merge_repairs.len(), 1);
    assert_eq!(
        again.state.land_approval.as_ref().unwrap().question,
        next.question
    );
    assert_eq!(
        std::fs::read_to_string(fx.tmp.path().join("forge/fixes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

common::e2e! {
async fn merge_repair_keeps_the_pr_and_run_and_requires_fresh_approval() {
    assert_fresh_decision(false).await;
}
}

common::e2e! {
async fn merge_repair_no_change_still_reviews_and_requires_fresh_approval() {
    assert_fresh_decision(true).await;
}
}

common::e2e! {
async fn merge_repair_exhausted_again_presents_new_findings_without_merging() {
    let (_fx, runner, old_question) = held_pr(false, true).await;
    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.parked, "{:?}", resumed.state.events);
    assert_eq!(resumed.state.merge_repairs[0].phase, MergeRepairPhase::Complete);
    assert!(!resumed.state.reviews[0].clean);
    let next = resumed.state.land_approval.as_ref().unwrap();
    assert_ne!(next.question, old_question);
    assert!(Questions::open().get(&next.question).unwrap().choices.iter().any(|c| c == REPAIR));
    assert_eq!(resumed.state.status, RunStatus::Landing);
}
}

common::e2e! {
async fn merge_repair_dirty_worktree_holds_with_recovery_guidance_without_dispatching_fixer() {
    let (fx, runner, _) = held_pr(false, false).await;
    std::fs::write(runner.state.winner().unwrap().worktree.join("dirty.txt"), "owner work").unwrap();
    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    let disk = RunState::load(&resumed.state.id).unwrap();
    assert_eq!(disk.merge_repairs[0].phase, MergeRepairPhase::Failed);
    assert!(disk.merge_repairs[0].failure.as_ref().unwrap().contains("dirty"));
    assert!(!fx.tmp.path().join("forge/fixes").exists());
    assert!(disk.land_approval.is_none());
    assert_eq!(Questions::open().list().iter().filter(|q| q.run == disk.id && q.node == APPROVAL_NODE && q.status.open()).count(), 0);
}
}

common::e2e! {
async fn merge_repair_obsolete_head_answer_is_not_consumed() {
    let (fx, runner, old_question) = held_pr(false, false).await;
    let winner = runner.state.winner().unwrap();
    magi::git::git(&winner.worktree, &["commit", "--allow-empty", "-m", "fix: newer head"]).await.unwrap();
    assert!(magi::git::push(&winner.worktree, "origin", &winner.branch).await.unwrap().ok());
    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.parked);
    assert!(resumed.state.merge_repairs.is_empty());
    assert_ne!(resumed.state.land_approval.as_ref().unwrap().question, old_question);
    assert!(!fx.tmp.path().join("forge/fixes").exists());
}
}

common::e2e! {
async fn merge_repair_interruption_rolls_back_commits_and_untracked_edits_without_retry() {
    let (fx, mut runner, question) = held_pr(false, false).await;
    let head = runner.state.land_approval.as_ref().unwrap().head.clone();
    runner.state.merge_repairs.push(magi::run::MergeRepair {
        question, head: head.clone(), pr: "https://github.com/example/repo/pull/42".into(),
        reviews: runner.state.reviews.clone(), gate: runner.state.gate.clone(),
        gate_head: runner.state.gate_head.clone(), gate_fixes: Vec::new(), pre_gate: Vec::new(),
        phase: MergeRepairPhase::Fixing, failure: None,
    });
    runner.state.land_approval = None;
    runner.state.save().unwrap();
    let wt = &runner.state.winner().unwrap().worktree;
    std::fs::write(wt.join("partial.txt"), "unfinished repair").unwrap();
    magi::git::git(wt, &["add", "partial.txt"]).await.unwrap();
    magi::git::git(wt, &["commit", "-m", "fix: partial repair"]).await.unwrap();
    std::fs::write(wt.join("untracked.txt"), "unfinished edit").unwrap();
    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(magi::git::rev_parse(wt, "HEAD").await.unwrap(), head);
    assert!(magi::git::is_clean(wt).await.unwrap());
    assert!(!wt.join("partial.txt").exists() && !wt.join("untracked.txt").exists());
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.merge_repairs[0].phase, MergeRepairPhase::Failed);
    assert!(resumed.state.merge_repairs[0].failure.as_ref().unwrap().contains("rolled back"));
    assert!(resumed.state.land_approval.is_none());
    assert!(!fx.tmp.path().join("forge/fixes").exists());
}
}

common::e2e! {
async fn merge_repair_commit_storage_failure_holds_without_review_or_push() {
    let (fx, runner, _) = held_pr(false, false).await;
    let agent = fx.tmp.path().join("mock-agent.sh");
    let script = std::fs::read_to_string(&agent).unwrap();
    std::fs::write(agent, script.replace("echo repaired >> repaired.txt", "touch \"$(git rev-parse --git-path index.lock)\"\necho repaired >> repaired.txt")).unwrap();
    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.merge_repairs[0].phase, MergeRepairPhase::Failed);
    assert!(resumed.state.merge_repairs[0].failure.as_ref().unwrap().contains("saving repair commit failed"));
    assert_eq!(resumed.state.reviews.len(), runner.state.reviews.len());
    assert_eq!(resumed.state.pr_pushed_head, runner.state.pr_pushed_head);
    assert!(resumed.state.land_approval.is_none());
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(calls.lines().filter(|l| l.starts_with("pr merge")).all(|l| l.contains("--disable-auto")));
}
}

common::e2e! {
async fn merge_repair_resume_refuses_a_commit_added_after_the_saved_gate() {
    let (fx, runner, _) = held_pr(false, false).await;
    let mut verified = Runner::resume(&runner.state.id).unwrap();
    verified.execute().await.unwrap();
    assert_eq!(verified.state.merge_repairs[0].phase, MergeRepairPhase::Complete);
    let winner = verified.state.winner().unwrap().clone();
    let checked = magi::git::rev_parse(&winner.worktree, "HEAD").await.unwrap();
    assert_eq!(verified.state.gate_head.as_deref(), Some(checked.as_str()));
    let remote_before = magi::git::remote_head(&winner.worktree, "origin", &winner.branch).await.unwrap();
    // Model an interruption after the gate save, before publication/Complete.
    let decision = verified.state.land_approval.take().unwrap();
    Questions::open().update(&decision.question, |q| { q.abandon("simulate an interruption before the new decision"); Ok(()) }).unwrap();
    verified.state.merge_repairs[0].phase = MergeRepairPhase::Reviewing;
    verified.state.save().unwrap();
    magi::git::git(&winner.worktree, &["commit", "--allow-empty", "-m", "fix: unverified later commit"]).await.unwrap();
    let added = magi::git::rev_parse(&winner.worktree, "HEAD").await.unwrap();
    assert_ne!(added, checked);
    let mut resumed = Runner::resume(&verified.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(resumed.state.merge_repairs[0].phase, MergeRepairPhase::Failed);
    assert!(resumed.state.merge_repairs[0].failure.as_ref().unwrap().contains("changed after its recorded gate"));
    assert_eq!(magi::git::remote_head(&winner.worktree, "origin", &winner.branch).await.unwrap(), remote_before);
    assert_eq!(magi::git::rev_parse(&winner.worktree, "HEAD").await.unwrap(), added);
    assert!(resumed.state.land_approval.is_none());
    assert_eq!(std::fs::read_to_string(fx.tmp.path().join("forge/fixes")).unwrap().lines().count(), 1);
}
}
