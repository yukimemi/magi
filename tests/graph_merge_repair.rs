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
pr_state=OPEN
if test -f "$REPAIR_FORGE/closed"; then pr_state=CLOSED; fi
sha=$(git ls-remote --refs origin "refs/heads/$REPAIR_BRANCH" 2>/dev/null | cut -f1)
case "$1 $2" in
 'pr view') printf '{"url":"https://github.com/example/repo/pull/42","number":42,"state":"%s","title":"feat: existing work","reviews":[],"comments":[],"mergeStateStatus":"CLEAN","headRefOid":"%s","baseRefName":"main","autoMergeRequest":null,"headRefName":"%s","isCrossRepository":false}\n' "$pr_state" "$sha" "$REPAIR_BRANCH" ;;
 'api graphql') printf '{"data":{"repository":{"pullRequest":{"commits":{"nodes":[{"commit":{"oid":"%s","statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]}}}}}\n' "$sha" ;;
 'api repos/{owner}/{repo}/pulls/42/comments') echo '[]' ;;
 'pr merge') case "$*" in *--disable-auto*)
   if test -f "$REPAIR_FORGE/late-answer"; then
     rm "$REPAIR_FORGE/late-answer"
     (unset MAGI_RUN MAGI_NODE MAGI_SEAT MAGI_TURN_SCOPE; cd "$REPAIR_HOME" || exit; "$REPAIR_CLI" answer "$REPAIR_LATE_QUESTION" --reply "$REPAIR_LATE_CHOICE") > "$REPAIR_FORGE/late-answer-result" 2>&1 || exit 97
   fi
   exit 0 ;; *) exit 99 ;; esac ;;
 'pr comment') exit 0 ;;
 'pr close') test ! -f "$REPAIR_FORGE/refuse-close" || exit 7; touch "$REPAIR_FORGE/closed" ;;
 *) exit 98 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let cli = common::magi_cli(&magi::run::home());
    // SAFETY: each e2e test runs in an isolated child, before its driver starts.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("REPAIR_FORGE", &bin);
        std::env::set_var("REPAIR_BRANCH", branch);
        for (key, value) in cli.get_envs() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        std::env::set_var("REPAIR_CLI", cli.get_program());
        std::env::set_var("REPAIR_HOME", magi::run::home());
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
    held_pr_choice(noop, keep_findings, REPAIR).await
}

async fn held_pr_choice(
    noop: bool,
    keep_findings: bool,
    choice: &str,
) -> (common::Fixture, Runner, String) {
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
    // Persist the exact legacy card shape, rather than manufacturing a
    // third button in memory. The real answer binary must migrate it.
    Questions::open()
        .update(&id, |q| {
            q.choices = vec!["merge".into(), "hold".into()];
            q.actions.clear();
            Ok(())
        })
        .unwrap();
    assert!(
        Questions::open()
            .get(&id)
            .unwrap()
            .choices
            .iter()
            .any(|c| c == REPAIR)
    );
    let answer = common::magi_cli(&magi::run::home())
        .args(["answer", &id, "--reply", choice])
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
    if choice == REPAIR {
        assert!(acknowledgement.contains("this answer command has not started repair"));
        assert!(acknowledgement.contains("Without one"));
        assert!(acknowledgement.contains(&format!("magi run --resume {}", runner.state.id)));
    }
    assert!(
        !fx.tmp.path().join("forge/fixes").exists(),
        "no daemon or driver is running in this scenario"
    );
    assert_eq!(
        Questions::open().get(&id).unwrap().resolution().as_deref(),
        Some(choice)
    );
    let duplicate = common::magi_cli(&magi::run::home())
        .args(["answer", &id, "--reply", choice])
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
async fn merge_repair_legacy_chat_refreshes_choices_on_the_next_turn() {
    let (fx, runner, id) = held_pr(false, false).await;
    let questions = Questions::open();
    questions.update(&id, |q| {
        q.status = magi::ask::QuestionStatus::Open;
        q.answer = None;
        q.choices = vec!["merge".into(), "hold".into()];
        q.actions.clear();
        Ok(())
    }).unwrap();
    let talks = magi::talk::Talks::open();
    let mut cfg = fx.config.clone();
    cfg.agents[0].command = vec!["sh".into(), "-c".into(), "cat".into()];
    cfg.agents[0].env.clear();
    cfg.talk.allow_write = false;
    let mut talk = magi::talk::begin(&talks, &cfg, fx.repo.clone(), Some(&cfg.agents[0].id)).unwrap();
    magi::talk::say(&talks.claim_turn(&talk.id).unwrap().unwrap(), &mut talk, &talks, &cfg, "Discuss the decision", Vec::new()).await.unwrap();
    let legacy = questions.update(&id, |q| {
        q.choices = vec!["merge".into(), "hold".into()];
        q.actions.clear();
        Ok(())
    }).unwrap().0;
    assert!(magi::consult::begin(&questions, &talks, &legacy, &talk).unwrap());
    talk = talks.get(&talk.id).unwrap();
    assert!(!talk.pending.contains("--reply 'fix findings and re-review'"));
    magi::talk::say(&talks.claim_turn(&talk.id).unwrap().unwrap(), &mut talk, &talks, &cfg, "What actions are available?", Vec::new()).await.unwrap();
    assert!(talk.turns.last().unwrap().body.contains("--reply 'fix findings and re-review'"));
    assert!(questions.get(&id).unwrap().status.open());
    assert!(RunState::load(&runner.state.id).unwrap().merge_repairs.is_empty());
    assert!(!fx.tmp.path().join("forge/fixes").exists());
}
}

common::e2e! {
async fn merge_repair_legacy_cli_answer_dispatches_through_parked_task_owner() {
    let (fx, mut runner, old_question) = held_pr(false, false).await;
    let queue = magi::queue::Queue::open();
    let mut task = magi::queue::Task::new("Existing PR".into(), runner.state.instruction.clone(), fx.repo.clone(), magi::queue::Source::Human);
    task.status = magi::queue::TaskStatus::Parked;
    task.runs.push(runner.state.id.clone());
    runner.state.origin = Some(Origin::queue(&task.id));
    runner.state.save().unwrap();
    queue.put(&mut task).unwrap();
    // Recreate an unanswered legacy card, then hold its parked owner before
    // the operator selects repair through the real CLI.
    Questions::open().update(&old_question, |q| {
        q.status = magi::ask::QuestionStatus::Open;
        q.answer = None;
        q.answered_at = None;
        q.choices = vec!["merge".into(), "hold".into()];
        q.actions.clear();
        Ok(())
    }).unwrap();
    task.hold_manual(Some("Pause this PR".into()));
    queue.put(&mut task).unwrap();
    let answer = common::magi_cli(&magi::run::home())
        .args(["answer", &old_question, "--reply", REPAIR])
        .env_remove("MAGI_RUN").env_remove("MAGI_NODE")
        .output().unwrap();
    assert!(answer.status.success(), "{}", String::from_utf8_lossy(&answer.stderr));
    let config = fx.tmp.path().join("dispatch.toml");
    let mut cfg = runner.state.config.clone();
    cfg.disk.min_free_bytes = 0;
    cfg.disk.auto_fold = false;
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    magi::daemon::serve_until(magi::daemon::Opts {
        repo: fx.repo.clone(), config: Some(config), once: true,
        worktrees_root: Some(fx.tmp.path().join("worktrees")),
        ..Default::default()
    }, magi::daemon::Stop::new()).await.unwrap();
    let disk = RunState::load(&runner.state.id).unwrap();
    assert_eq!(disk.merge_repairs.len(), 1);
    assert_eq!(disk.merge_repairs[0].question, old_question);
    assert_eq!(disk.merge_repairs[0].phase, MergeRepairPhase::Complete);
    assert!(disk.instruction.starts_with(&runner.state.instruction));
    assert!(disk.parked);
    assert_ne!(disk.land_approval.as_ref().unwrap().question, old_question);
    assert!(disk.gate_status().ok());
    assert_eq!(queue.list().len(), 1);
    let owner = queue.get(&task.id).unwrap();
    assert!(owner.runs.iter().all(|id| id == &disk.id));
    assert_eq!(owner.instruction, task.instruction);
    assert!(owner.action_applied(&old_question));
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(calls.lines().filter(|l| l.starts_with("pr merge")).all(|l| l.contains("--disable-auto")));
}
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

async fn decision_owner(runner: &mut Runner, fx: &common::Fixture) -> magi::queue::Task {
    let mut task = magi::queue::Task::new(
        "Existing PR".into(),
        runner.state.instruction.clone(),
        fx.repo.clone(),
        magi::queue::Source::Human,
    );
    task.runs.push(runner.state.id.clone());
    task.status = magi::queue::TaskStatus::Parked;
    runner.state.origin = Some(Origin::queue(&task.id));
    runner.state.save().unwrap();
    magi::queue::Queue::open().put(&mut task).unwrap();
    task
}

async fn serve_decision(fx: &common::Fixture, state: &RunState) {
    let config = fx.tmp.path().join("decision.toml");
    let mut cfg = state.config.clone();
    cfg.disk.min_free_bytes = 0;
    cfg.disk.auto_fold = false;
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    magi::daemon::serve_until(
        magi::daemon::Opts {
            repo: fx.repo.clone(),
            config: Some(config),
            once: true,
            worktrees_root: Some(fx.tmp.path().join("worktrees")),
            ..Default::default()
        },
        magi::daemon::Stop::new(),
    )
    .await
    .unwrap();
}

// Re-executed by the contention tests so the lease belongs to a different
// live process. Closing stdin releases it and lets the child exit normally.
#[test]
fn landing_verification_cache_holder() {
    use std::io::BufRead as _;
    let Some(home) = std::env::var_os("LANDING_CACHE_TEST_HOME") else {
        return;
    };
    magi::run::set_home(home.into());
    let cache = std::path::PathBuf::from(std::env::var_os("LANDING_CACHE_TEST_DIR").unwrap());
    let owner = magi::cache::Owner::here("other-run", "gate", "gate", &cache, "other-head");
    let guard = match magi::cache::try_acquire(&magi::run::home(), &cache, &owner).unwrap() {
        magi::cache::AcquireOutcome::Acquired(guard) => guard,
        magi::cache::AcquireOutcome::Busy(_) => panic!("test cache was already busy"),
    };
    std::fs::write(cache.join("holder-ready"), "").unwrap();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    guard.release();
}

async fn landing_cache_contention_refunds_attempt(e2e: bool) {
    let (fx, mut runner, question) = held_pr_choice(false, false, "merge").await;
    Questions::open()
        .update(&question, |q| {
            q.status = magi::ask::QuestionStatus::Open;
            q.answer = None;
            q.answered_at = None;
            Ok(())
        })
        .unwrap();
    let cache = fx.tmp.path().join("verification-cache");
    std::fs::create_dir(&cache).unwrap();
    let started = fx.tmp.path().join("verification-started");
    let command = format!(
        "CARGO_TARGET_DIR={} touch {}",
        cache.display(),
        started.display()
    );
    runner.state.config.graph.timeout_resource_wait = 1;
    runner.state.parked = false;
    runner.state.status = RunStatus::Blocked;
    runner.state.merge.as_mut().unwrap().ok = false;
    if e2e {
        runner.state.config.verify.e2e = vec![command];
        runner.state.gate_repair_e2e = Some(magi::run::GateRepairE2e {
            head: runner.state.land_approval.as_ref().unwrap().head.clone(),
            passed: false,
            outcomes: Vec::new(),
        });
    } else {
        runner.state.config.verify.gate = vec![command];
        runner.state.gate_ran = false;
        runner.state.gate_head = None;
    }
    let mut task = decision_owner(&mut runner, &fx).await;
    task.release_operator("test", None);
    magi::queue::Queue::open().put(&mut task).unwrap();
    let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "landing_verification_cache_holder",
            "--nocapture",
        ])
        .env("LANDING_CACHE_TEST_HOME", magi::run::home())
        .env("LANDING_CACHE_TEST_DIR", &cache)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !cache.join("holder-ready").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if ready.is_ok() {
        serve_decision(&fx, &runner.state).await;
    }
    drop(holder.stdin.take());
    let output = holder.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ready.expect("cache holder did not become ready");

    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    let disk = RunState::load(&runner.state.id).unwrap();
    assert_eq!(
        owner.attempts, task.attempts,
        "resource waits must refund the attempt"
    );
    assert_eq!(owner.status, magi::queue::TaskStatus::Held);
    assert_eq!(owner.hold_source, Some(magi::queue::HoldSource::Machine));
    assert_eq!(owner.runs, task.runs);
    assert_eq!(
        disk.resource_block.as_ref().unwrap().node,
        if e2e { "e2e" } else { "gate" }
    );
    let outcomes = if e2e {
        &disk.gate_repair_e2e.as_ref().unwrap().outcomes
    } else {
        &disk.gate
    };
    assert!(outcomes.iter().any(|outcome| outcome.resource_blocked));
    let worktree = &disk.winner().unwrap().worktree;
    assert!(!started.exists());
    assert_eq!(
        magi::git::rev_parse(worktree, "HEAD").await.unwrap(),
        disk.pr_pushed_head.as_ref().unwrap().as_str()
    );

    // Once the real lease is free, releasing the task retries the same run.
    let mut released = owner;
    released.release_operator("test", None);
    magi::queue::Queue::open().put(&mut released).unwrap();
    serve_decision(&fx, &disk).await;
    let resumed = RunState::load(&disk.id).unwrap();
    assert!(resumed.resource_block.is_none(), "{:?}", resumed.events);
    assert!(resumed.parked, "{:?}", resumed.events);
    assert!(started.exists());
    assert_eq!(
        magi::queue::Queue::open().get(&task.id).unwrap().runs,
        task.runs
    );
}

common::e2e! {
async fn landing_gate_cache_contention_spends_no_attempt() {
    landing_cache_contention_refunds_attempt(false).await;
}
}

common::e2e! {
async fn landing_post_repair_e2e_cache_contention_spends_no_attempt() {
    landing_cache_contention_refunds_attempt(true).await;
}
}

common::e2e! {
async fn decision_cli_hold_survives_head_change_and_daemon_settles_manual_hold() {
    let (fx, mut runner, question) = held_pr_choice(false, false, "hold").await;
    let task = decision_owner(&mut runner, &fx).await;
    let w = runner.state.winner().unwrap();
    std::fs::write(w.worktree.join("external.txt"), "changed after card").unwrap();
    magi::git::commit_all(&w.worktree, "fix: change after decision").await.unwrap();
    assert!(magi::git::push(&w.worktree, "origin", &w.branch).await.unwrap().ok());
    serve_decision(&fx, &runner.state).await;
    let disk = RunState::load(&runner.state.id).unwrap();
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(disk.status, RunStatus::Blocked);
    assert_eq!(owner.status, magi::queue::TaskStatus::Held);
    assert!(owner.operator_held());
    assert!(owner.action_applied(&question));
    assert_eq!(disk.pr.as_ref().unwrap().state, "open");
    assert!(disk.merge_repairs.is_empty());
    assert!(!fx.tmp.path().join("forge/fixes").exists());
    assert!(!fx.tmp.path().join("forge/closed").exists());
    assert_eq!(Questions::open().get(&question).unwrap().resolution().as_deref(), Some("hold"));
}
}

common::e2e! {
async fn decision_cli_withdraw_closes_and_terminates_only_after_confirmation() {
    let (fx, mut runner, question) = held_pr_choice(false, false, magi::merge_decision::WITHDRAW).await;
    let task = decision_owner(&mut runner, &fx).await;
    serve_decision(&fx, &runner.state).await;
    let disk = RunState::load(&runner.state.id).unwrap();
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(owner.status, magi::queue::TaskStatus::Done);
    assert_eq!(disk.pr.as_ref().unwrap().state, "closed");
    assert!(disk.merge_decision.as_ref().unwrap().closed);
    assert!(owner.action_applied(&question));
    assert!(Runner::resume(&disk.id).is_err(), "retired execution must never resume");
    assert_eq!(owner.instruction, task.instruction);
    assert_eq!(owner.runs, task.runs);
}
}

common::e2e! {
async fn decision_withdraw_failure_holds_without_terminating_or_repairing() {
    let (fx, mut runner, _) = held_pr_choice(false, false, magi::merge_decision::WITHDRAW).await;
    let task = decision_owner(&mut runner, &fx).await;
    std::fs::write(fx.tmp.path().join("forge/refuse-close"), "").unwrap();
    serve_decision(&fx, &runner.state).await;
    let disk = RunState::load(&runner.state.id).unwrap();
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(owner.status, magi::queue::TaskStatus::Held);
    assert!(owner.operator_held());
    assert!(!disk.merge_decision.as_ref().unwrap().closed);
    assert_eq!(disk.pr.as_ref().unwrap().state, "open");
    assert!(!fx.tmp.path().join("forge/fixes").exists());
}
}

common::e2e! {
async fn decision_reimplementation_retires_owner_before_requeue_and_preserves_selection() {
    let (fx, mut runner, question) = held_pr_choice(false, false, "reimplement:beta").await;
    let task = decision_owner(&mut runner, &fx).await;
    serve_decision(&fx, &runner.state).await;
    let disk = RunState::load(&runner.state.id).unwrap();
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(owner.status, magi::queue::TaskStatus::Queued);
    assert!(owner.fresh_start);
    let pin = owner.replacement.as_ref().unwrap();
    assert_eq!(pin.agent, "beta");
    assert_eq!(pin.question, question);
    assert_eq!(pin.run, disk.id);
    assert!(pin.context.contains("Review evidence"));
    assert_eq!(owner.instruction, task.instruction);
    assert!(disk.driver_exited);
    assert!(disk.merge_decision.as_ref().unwrap().closed);
    assert!(Runner::resume(&disk.id).is_err());
    // Replaying crash recovery must not mint an attempt or acquire ownership.
    let mut replay = owner.clone();
    magi::daemon::finish_attempt(3, &magi::queue::Queue::open(), &mut replay, &disk, &[], Ok(()));
    assert_eq!(replay.runs, task.runs);
    assert_eq!(replay.replacement.as_ref().unwrap().agent, "beta");
    let mut next = disk.clone();
    next.replaces = Some(pin.clone());
    assert_ne!(next.task_pr_branch(), disk.task_pr_branch(), "new PR uses a distinct public branch");
    // Real daemon dispatch must keep the pin even though other agents work.
    let mut dispatch = runner.state.clone();
    for agent in &mut dispatch.config.agents {
        if agent.id == "beta" { agent.env.insert("MOCK_QUOTA_SEAT".into(), "impl-A".into()); }
    }
    serve_decision(&fx, &dispatch).await;
    let after = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(after.status, magi::queue::TaskStatus::Held, "error={:?}; hold={:?}; runs={:?}; pin={:?}", after.last_error, after.hold_reason, after.runs, after.replacement);
    assert!(after.operator_held());
    assert_eq!(after.runs.len(), 2);
    let replacement = RunState::load(after.runs.last().unwrap()).unwrap();
    assert_eq!(replacement.candidates.len(), 1);
    assert_eq!(replacement.candidates[0].agent, "beta");
    assert!(replacement.handovers.is_empty());
    assert_eq!(replacement.task_instruction.as_deref(), Some(task.instruction.as_str()));
    assert!(replacement.config.graph.land_approval);
    let old = RunState::load(&disk.id).unwrap();
    assert_eq!(old.merge_decision.as_ref().unwrap().replacement_run.as_deref(), Some(replacement.id.as_str()));
    // Explicit release after quota recovery retries the same pinned attempt.
    let mut recovered = replacement.clone();
    for agent in &mut recovered.config.agents { agent.env.remove("MOCK_QUOTA_SEAT"); }
    recovered.config.merge.mode = MergeMode::None;
    recovered.save().unwrap();
    let mut released = after.clone();
    released.release_operator("quota-restored", None);
    magi::queue::Queue::open().put(&mut released).unwrap();
    serve_decision(&fx, &runner.state).await;
    let done = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(done.runs, after.runs, "release must not mint another replacement");
    let resumed = RunState::load(&replacement.id).unwrap();
    assert_eq!(resumed.candidates[0].agent, "beta");
    assert!(resumed.candidates[0].commits > 0);
    assert!(resumed.handovers.is_empty());

}
}

common::e2e! {
async fn decision_replacement_missing_agent_holds_before_minting_an_attempt() {
    let (fx, mut runner, _) = held_pr_choice(false, false, "reimplement:beta").await;
    let task = decision_owner(&mut runner, &fx).await;
    serve_decision(&fx, &runner.state).await;
    let mut dispatch = runner.state.clone();
    dispatch.config.agents.retain(|agent| agent.id != "beta");
    serve_decision(&fx, &dispatch).await;
    let after = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(after.status, magi::queue::TaskStatus::Held, "error={:?}; hold={:?}; runs={:?}; pin={:?}", after.last_error, after.hold_reason, after.runs, after.replacement);
    assert!(after.operator_held());
    assert_eq!(after.runs, task.runs);
    assert!(after.hold_reason.as_ref().unwrap().contains("beta"));
    assert!(after.hold_reason.as_ref().unwrap().contains("unavailable"));
}
}

common::e2e! {
async fn decision_cli_stale_merge_answer_never_authorizes_a_changed_head() {
    let (fx, mut runner, question) = held_pr_choice(false, false, "merge").await;
    let task = decision_owner(&mut runner, &fx).await;
    let w = runner.state.winner().unwrap();
    std::fs::write(w.worktree.join("after-approval.txt"), "new head").unwrap();
    magi::git::commit_all(&w.worktree, "fix: move head after approval").await.unwrap();
    assert!(magi::git::push(&w.worktree, "origin", &w.branch).await.unwrap().ok());
    serve_decision(&fx, &runner.state).await;
    let disk = RunState::load(&runner.state.id).unwrap();
    assert!(disk.parked);
    assert_ne!(disk.land_approval.as_ref().unwrap().question, question);
    assert_ne!(disk.status, RunStatus::Merged);
    assert_eq!(magi::queue::Queue::open().get(&task.id).unwrap().runs, task.runs);
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(calls.lines().filter(|l| l.starts_with("pr merge")).all(|l| l.contains("--disable-auto")));
}
}

common::e2e! {
async fn decision_withdraw_answer_resumes_a_manually_held_owner_only_to_close() {
    let (fx, mut runner, question) = held_pr_choice(false, false, magi::merge_decision::WITHDRAW).await;
    let mut task = decision_owner(&mut runner, &fx).await;
    Questions::open().update(&question, |q| {
        q.status = magi::ask::QuestionStatus::Open;
        q.answer = None;
        q.answered_at = None;
        Ok(())
    }).unwrap();
    task.hold_manual(Some("Decision deferred".into()));
    task.set_implementers(2, &runner.state.config).unwrap();
    magi::queue::Queue::open().put(&mut task).unwrap();
    let answer = common::magi_cli(&magi::run::home())
        .args(["answer", &question, "--reply", magi::merge_decision::WITHDRAW])
        .env_remove("MAGI_RUN").env_remove("MAGI_NODE")
        .output().unwrap();
    assert!(answer.status.success(), "{}", String::from_utf8_lossy(&answer.stderr));
    serve_decision(&fx, &runner.state).await;
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(owner.status, magi::queue::TaskStatus::Done, "{:?}", owner.hold_reason);
    assert_eq!(owner.runs, task.runs);
    assert!(RunState::load(&runner.state.id).unwrap().merge_decision.unwrap().closed);
    assert!(!fx.tmp.path().join("forge/fixes").exists());
}
}

common::e2e! {
async fn decision_replacement_crash_checkpoint_does_not_start_a_second_attempt() {
    let (fx, mut runner, _) = held_pr_choice(false, false, "reimplement:beta").await;
    let task = decision_owner(&mut runner, &fx).await;
    serve_decision(&fx, &runner.state).await;
    let mut old = RunState::load(&runner.state.id).unwrap();
    // A driver minted the next run and crashed before linking the task.
    old.merge_decision.as_mut().unwrap().replacement_run = Some("unlinked-attempt".into());
    old.save().unwrap();
    serve_decision(&fx, &runner.state).await;
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(owner.status, magi::queue::TaskStatus::Held);
    assert!(owner.operator_held());
    assert_eq!(owner.runs, task.runs);
    assert!(owner.hold_reason.as_ref().unwrap().contains("ownership"));
    assert!(Runner::resume(&old.id).is_err());
}
}

fn run_cli(fx: &common::Fixture, args: &[&str]) {
    let output = common::magi_cli(&magi::run::home())
        .args(args)
        .current_dir(&fx.repo)
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn approval_gate_late_stop(choice: &str, during_gate: bool) {
    let (fx, mut runner, question) = held_pr_choice(false, false, choice).await;
    let task = decision_owner(&mut runner, &fx).await;
    Questions::open()
        .update(&question, |q| {
            q.status = magi::ask::QuestionStatus::Open;
            q.answer = None;
            q.answered_at = None;
            Ok(())
        })
        .unwrap();
    // The first owner_stop sees an open card. Disarming this saved arm is
    // the await between that read and approval_gate reading the card again.
    if during_gate {
        runner.state.gate_ran = false;
        runner.state.gate_head = None;
        runner.state.config.verify.gate = vec![r#"rm "$REPAIR_FORGE/late-answer" && (unset MAGI_RUN MAGI_NODE MAGI_SEAT MAGI_TURN_SCOPE; cd "$REPAIR_HOME" && "$REPAIR_CLI" answer "$REPAIR_LATE_QUESTION" --reply "$REPAIR_LATE_CHOICE")"#.into()];
    } else {
        runner.state.land_armed_head =
            Some(runner.state.land_approval.as_ref().unwrap().head.clone());
    }
    runner.state.save().unwrap();
    std::fs::write(fx.tmp.path().join("forge/late-answer"), "").unwrap();
    // SAFETY: this test owns an isolated child; the callback runs after these writes.
    unsafe {
        std::env::set_var("REPAIR_LATE_QUESTION", &question);
        std::env::set_var("REPAIR_LATE_CHOICE", choice);
    }
    run_cli(&fx, &["run", "--resume", &runner.state.id]);
    let disk = RunState::load(&runner.state.id).unwrap();
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert!(
        !fx.tmp.path().join("forge/late-answer").exists(),
        "CLI answer callback did not run"
    );
    assert_eq!(
        Questions::open()
            .get(&question)
            .unwrap()
            .resolution()
            .as_deref(),
        Some(choice)
    );
    let decision = disk
        .merge_decision
        .as_ref()
        .expect("late stop must be consumed");
    assert_eq!(decision.question, question);
    assert_eq!(decision.choice, choice);
    assert!(
        decision.closed,
        "closure must be confirmed before settling the action"
    );
    assert_eq!(disk.pr.as_ref().unwrap().state, "closed");
    assert_eq!(owner.runs, task.runs);
    if choice == magi::merge_decision::WITHDRAW {
        assert_eq!(owner.status, magi::queue::TaskStatus::Done);
    } else {
        assert_eq!(owner.status, magi::queue::TaskStatus::Queued);
        assert_eq!(owner.replacement.as_ref().unwrap().agent, "beta");
    }
    assert!(!fx.tmp.path().join("forge/fixes").exists());
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(
        calls
            .lines()
            .filter(|line| line.starts_with("pr merge"))
            .all(|line| line.contains("--disable-auto"))
    );
}

common::e2e! {
async fn decision_review_late_withdraw_at_approval_gate_closes_the_pr() {
    approval_gate_late_stop(magi::merge_decision::WITHDRAW, false).await;
}
}

common::e2e! {
async fn decision_review_late_reimplementation_at_approval_gate_requeues_the_owner() {
    approval_gate_late_stop("reimplement:beta", false).await;
}
}

common::e2e! {
async fn decision_review_late_withdraw_during_landing_gate_closes_the_pr() {
    approval_gate_late_stop(magi::merge_decision::WITHDRAW, true).await;
}
}

common::e2e! {
async fn decision_review_late_reimplementation_during_landing_gate_requeues_the_owner() {
    approval_gate_late_stop("reimplement:beta", true).await;
}
}

common::e2e! {
async fn decision_repair_answer_survives_a_landing_base_advance() {
    let (fx, runner, question) = held_pr(false, false).await;
    let old_head = runner.state.land_approval.as_ref().unwrap().head.clone();
    magi::git::git(&fx.repo, &["checkout", "main"]).await.unwrap();
    std::fs::write(fx.repo.join("base-advance.txt"), "new base").unwrap();
    magi::git::commit_all(&fx.repo, "test: advance base").await.unwrap();
    assert!(magi::git::push(&fx.repo, "origin", "main").await.unwrap().ok());

    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.id, runner.state.id);
    assert_eq!(resumed.state.merge_repairs.len(), 1, "{:?}", resumed.state.events);
    let repair = &resumed.state.merge_repairs[0];
    assert_eq!(repair.question, question);
    assert_eq!(repair.head, old_head);
    assert_eq!(repair.phase, MergeRepairPhase::Complete);
    assert!(fx.tmp.path().join("forge/fixes").exists());
    assert!(resumed.state.parked);
    let next = resumed.state.land_approval.as_ref().unwrap();
    assert_ne!(next.question, question);
    assert_ne!(next.head, old_head);
    assert!(Questions::open().get(&next.question).unwrap().status.open());
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(calls.lines().filter(|line| line.starts_with("pr merge")).all(|line| line.contains("--disable-auto")));
}
}

async fn resume_after_hold(via_daemon: bool) {
    let (fx, mut runner, old_question) = held_pr_choice(false, false, "hold").await;
    let task = decision_owner(&mut runner, &fx).await;
    serve_decision(&fx, &runner.state).await;
    let queue = magi::queue::Queue::open();
    let held = RunState::load(&runner.state.id).unwrap();
    assert_eq!(held.status, RunStatus::Blocked);
    assert!(held.merge_decision.is_some());
    assert!(queue.get(&task.id).unwrap().operator_held());
    // A quiet daemon cannot undo the owner's hold by itself.
    serve_decision(&fx, &held).await;
    assert!(queue.get(&task.id).unwrap().operator_held());
    assert_eq!(
        RunState::load(&held.id)
            .unwrap()
            .land_approval
            .as_ref()
            .unwrap()
            .question,
        old_question
    );
    if via_daemon {
        run_cli(&fx, &["task", "release", &task.id]);
        serve_decision(&fx, &held).await;
    } else {
        run_cli(&fx, &["run", "--resume", &held.id]);
    }
    let resumed = RunState::load(&held.id).unwrap();
    let owner = queue.get(&task.id).unwrap();
    assert_eq!(resumed.status, RunStatus::Landing, "{:?}", resumed.events);
    assert!(resumed.parked);
    assert!(resumed.merge_decision.is_none());
    let fresh = resumed.land_approval.as_ref().unwrap();
    assert_ne!(fresh.question, old_question);
    assert!(
        Questions::open()
            .get(&fresh.question)
            .unwrap()
            .status
            .open()
    );
    assert_eq!(
        Questions::open()
            .get(&old_question)
            .unwrap()
            .resolution()
            .as_deref(),
        Some("hold")
    );
    assert_eq!(owner.runs, task.runs, "resume must keep the same attempt");
    assert_eq!(owner.status, magi::queue::TaskStatus::Parked);
    assert_eq!(
        resumed.pr.as_ref().unwrap().url,
        held.pr.as_ref().unwrap().url
    );
    assert_eq!(resumed.reviews.len(), held.reviews.len());
    assert_eq!(resumed.gate_head, held.gate_head);
    assert!(resumed.merge_repairs.is_empty());
    assert!(!fx.tmp.path().join("forge/fixes").exists());
    assert!(!fx.tmp.path().join("forge/closed").exists());
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(
        calls
            .lines()
            .filter(|line| line.starts_with("pr merge"))
            .all(|line| line.contains("--disable-auto"))
    );
}

common::e2e! {
async fn decision_review_task_release_after_hold_requests_a_fresh_decision() {
    resume_after_hold(true).await;
}
}

common::e2e! {
async fn decision_review_cli_resume_after_hold_requests_a_fresh_decision() {
    resume_after_hold(false).await;
}
}

common::e2e! {
async fn decision_cli_hold_release_invalidates_prior_merge_consent_on_daemon_resume() {
    let (fx, mut runner, old_question) = held_pr_choice(false, false, "merge").await;
    let task = decision_owner(&mut runner, &fx).await;
    // The daemon has not consumed the merge answer. Hold and release through
    // the real CLI while it is stopped, without changing the reviewed head.
    run_cli(&fx, &["task", "hold", &task.id]);
    run_cli(&fx, &["task", "release", &task.id]);
    serve_decision(&fx, &runner.state).await;

    let resumed = RunState::load(&runner.state.id).unwrap();
    let fresh = resumed.land_approval.as_ref().unwrap();
    assert_ne!(fresh.question, old_question, "{:?}", resumed.events);
    assert_eq!(fresh.head, runner.state.land_approval.as_ref().unwrap().head);
    assert!(Questions::open().get(&fresh.question).unwrap().status.open());
    assert_eq!(Questions::open().get(&old_question).unwrap().resolution().as_deref(), Some("merge"));
    assert_eq!(resumed.status, RunStatus::Landing);
    assert!(resumed.parked);
    let owner = magi::queue::Queue::open().get(&task.id).unwrap();
    assert_eq!(owner.runs, task.runs);
    assert_eq!(owner.status, magi::queue::TaskStatus::Parked);
    assert!(!fx.tmp.path().join("forge/fixes").exists());
    assert!(!fx.tmp.path().join("forge/closed").exists());
    let calls = std::fs::read_to_string(fx.tmp.path().join("forge/calls")).unwrap();
    assert!(calls.lines().filter(|line| line.starts_with("pr merge")).all(|line| line.contains("--disable-auto")));
}
}
