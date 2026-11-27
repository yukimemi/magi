//! Deterministic queue + graph + scheduler verification for conclusive
//! `head_e2e` stops across scheduler laps, restarts, automatic recovery paths,
//! explicit one-shot owner approvals, changed base/head, and notifications.
mod common;

use std::path::Path;
use std::time::Duration;

use common::{Judges, fixture, home_lock};
use jiff::Timestamp;
use magi::ask::Questions;
use magi::conduct::{self, Decision, Recovery, Verdict};
use magi::config::MergeMode;
use magi::daemon::{self, Opts};
use magi::queue::{HoldSource, PassCost, Queue, Source, Task, TaskStatus};
use magi::run::{RunState, RunStatus};
use magi::talk::{self, Talks};
use magi::triage;

fn sh(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\\', "/"))
}

fn git_sync(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn count(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn e2e_command(counter: &Path, mode: &Path) -> String {
    let c = sh(counter);
    let m = sh(mode);
    format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         mode=$(cat {m} 2>/dev/null || echo fail-v1); \
         if [ \"$mode\" = pass ]; then echo 'test typography::spacing ... ok'; exit 0; \
         elif [ \"$mode\" = fail-v2 ]; then echo 'test typography::kerning ... FAILED'; exit 1; \
         else echo 'test typography::spacing ... FAILED'; exit 1; fi"
    )
}

fn serve_opts(repo: &Path, config_path: &Path, worktrees_root: &Path) -> Opts {
    Opts {
        repo: repo.to_path_buf(),
        config: Some(config_path.to_path_buf()),
        once: true,
        max_attempts: 3,
        poll: Duration::from_millis(20),
        worktrees_root: Some(worktrees_root.to_path_buf()),
        ..Opts::default()
    }
}

async fn run_serve_lap(opts: Opts) {
    tokio::time::timeout(Duration::from_secs(120), daemon::serve(opts))
        .await
        .expect("daemon lap finished in time")
        .expect("daemon serve");
}

fn chat_notice_count(talks: &Talks, chat_id: &str) -> usize {
    let talk = talks.get(chat_id).expect("load chat");
    talk.pending_messages.len() + talk.turns.len()
}

common::e2e! {
async fn conclusive_head_e2e_stop_holds_once_suppresses_automatic_retries_and_allows_changed_facts_and_owner_recovery() {
    let home_guard = home_lock().await;
    let mut fx = fixture(home_guard, Judges::Unanimous, false);
    let home = magi::run::home();

    let counter = fx.tmp.path().join("e2e-counter");
    let mode_file = fx.tmp.path().join("e2e-mode");
    std::fs::write(&mode_file, "fail-v1").unwrap();

    // Create a review-only branch off main.
    git_sync(&fx.repo, &["checkout", "-b", "feat/typography"]);
    git_sync(&fx.repo, &["commit", "--allow-empty", "-m", "intermediate base"]);
    let intermediate = git_sync(&fx.repo, &["rev-parse", "HEAD"]);
    std::fs::write(fx.repo.join("typography.txt"), "initial typography\n").unwrap();
    git_sync(&fx.repo, &["add", "typography.txt"]);
    git_sync(&fx.repo, &["commit", "-m", "feat: typography"]);
    git_sync(&fx.repo, &["checkout", "main"]);

    // Configure review-only run that enters gate() -> verify_current_head directly.
    fx.config.graph.review_rounds = 0;
    fx.config.graph.rerun_failed_e2e = false;
    fx.config.verify.e2e = vec![e2e_command(&counter, &mode_file)];
    fx.config.verify.gate = vec!["true".into()];
    fx.config.merge.mode = MergeMode::None;
    fx.config.daemon.max_concurrent_runs = 1;
    fx.config.disk.min_free_bytes = 0;
    fx.config.disk.auto_fold = false;
    fx.config.disk.cache_limit_bytes = 0;
    // Neutral conductor so periodic service ticks never invent unrelated holds.
    let mut conductor = fx.config.agents[0].clone();
    conductor.id = "neutral-conductor".into();
    conductor.command = vec![
        "sh".into(),
        "-c".into(),
        "printf '%s\\n' '{\"decisions\":[]}'".into(),
    ];
    fx.config.roles.conductor = Some(conductor.id.as_str().into());
    fx.config.agents.push(conductor);

    let config_path = fx.tmp.path().join("magi.toml");
    std::fs::write(&config_path, toml::to_string(&fx.config).unwrap()).unwrap();
    let wt_root = fx.tmp.path().join("wt");

    // Seed an originating chat (hold its turn lock so pending_messages stay inspectable
    // without spawning an external chat agent; we count exact queued semantic notices).
    let talks = Talks::at(home.join("talks"));
    let chat = talk::begin(&talks, &fx.config, fx.repo.clone(), None).unwrap();
    let _turn_lock = talks.claim_turn(&chat.id).unwrap().expect("turn lock");

    let queue = Queue::at(home.join("queue"));
    let mut task = Task::new(
        "fix typography".into(),
        "Review the branch `feat/typography` (review-only: no implementation, no judging).".into(),
        fx.repo.clone(),
        Source::Agent {
            run: chat.id.clone(),
            node: magi::queue::CHAT_NODE.into(),
        },
    );
    task.review_of = Some("feat/typography".into());
    queue.put(&mut task).unwrap();
    let task_id = task.id.clone();

    // --- Lap 1: executes verify_current_head once, fails conclusively, settles to one machine hold.
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    magi::chat_notify::sweep(&queue, &talks, &home);
    magi::notice_delivery::sweep(&queue, &talks, &home, &fx.repo);

    let t1 = queue.get(&task_id).unwrap();
    assert_eq!(t1.status, TaskStatus::Held, "must settle directly to Held: {:?}", t1.hold_reason);
    assert_eq!(t1.hold_source, Some(HoldSource::Machine));
    assert!(t1.has_head_e2e_cause(), "must carry typed head_e2e cause");
    assert_eq!(t1.attempts, 1, "legitimate first attempt stays spent");
    assert_eq!(t1.runs.len(), 1);
    assert_eq!(t1.passes.len(), 1);
    assert_eq!(t1.passes[0].cost, PassCost::Spent);
    assert_eq!(count(&counter), 1, "exactly one failing command sequence ran");
    let hold_reason = t1.hold_reason.as_deref().unwrap_or("");
    assert!(hold_reason.contains("[head-e2e]"), "hold reason: {hold_reason}");
    assert!(hold_reason.contains("typography::spacing"), "hold reason: {hold_reason}");
    assert!(hold_reason.contains("magi task release"), "recovery hint missing: {hold_reason}");
    assert!(
        t1.diagnostic
            .as_deref()
            .is_some_and(|d| d.contains("typography::spacing")),
        "diagnostic should preserve failed command excerpt: {:?}",
        t1.diagnostic
    );

    let run_id = t1.runs[0].clone();
    let s1 = RunState::load_under(&run_id, &home).unwrap();
    assert_eq!(s1.status, RunStatus::Blocked);
    assert!(!s1.gate_ran);
    assert!(s1.head_e2e.as_ref().is_some_and(|e| !e.passed));
    let stop1 = magi::head_e2e_stop::conclusive(&s1).expect("conclusive stop");
    assert_eq!(t1.head_e2e_stop_id(), Some(stop1.id.as_str()));
    assert_eq!(
        chat_notice_count(&talks, &chat.id),
        1,
        "run_ended, task_held, and chat_notify must collapse into one actionable chat turn"
    );

    // --- Laps 2 & 3 + simulated daemon restart + triage + conductor: zero extra work or chat pages.
    let questions = Questions::at(home.join("questions"));
    for _ in 0..2 {
        let reopened_queue = Queue::at(home.join("queue"));
        let reopened_talks = Talks::at(home.join("talks"));
        let report = triage::run_once(&reopened_queue, &questions, Some(&config_path), Timestamp::now());
        assert!(report.resumed.is_empty(), "triage must not resume unchanged stop: {report:?}");
        assert!(report.asked.is_empty(), "triage must not ask redundant question: {report:?}");

        // Conductor attempting automatic Recovery::Review and Recovery::Requeue must be suppressed.
        for recovery in [Recovery::Review, Recovery::Requeue] {
            conduct::apply(
                &reopened_queue,
                &questions,
                &Verdict {
                    decisions: vec![Decision {
                        id: task_id.clone(),
                        blocked_by: Vec::new(),
                        missing_dependencies: Vec::new(),
                        recovery: Some(recovery),
                        question: None,
                        choices: Vec::new(),
                        actions: Default::default(),
                        question_decision: None,
                        reason: Some("try again".into()),
                        evidence: Vec::new(),
                    }],
                },
            )
            .unwrap();
        }

        run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
        magi::chat_notify::sweep(&reopened_queue, &reopened_talks, &home);
        magi::notice_delivery::sweep(&reopened_queue, &reopened_talks, &home, &fx.repo);
    }

    // Even if a legacy/stale record sits at TaskStatus::Failed with unchanged facts,
    // claim_dispatch must hold it before spawning a worker or spending an attempt.
    queue
        .modify(&task_id, |t| {
            t.status = TaskStatus::Failed;
            true
        })
        .unwrap();
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    magi::chat_notify::sweep(&queue, &talks, &home);
    magi::notice_delivery::sweep(&queue, &talks, &home, &fx.repo);

    // Simulate a crash leaving the task at Running after verify_current_head saved
    // the conclusive failure: reclaim_orphaned_running must settle through the same
    // typed machine hold without charging extra attempts or re-paging chat.
    queue
        .modify(&task_id, |t| {
            t.status = TaskStatus::Running;
            true
        })
        .unwrap();
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    magi::chat_notify::sweep(&queue, &talks, &home);
    magi::notice_delivery::sweep(&queue, &talks, &home, &fx.repo);

    let t_after_laps = queue.get(&task_id).unwrap();
    let s_after_laps = RunState::load_under(&run_id, &home).unwrap();
    assert_eq!(t_after_laps.status, TaskStatus::Held);
    assert_eq!(t_after_laps.attempts, 1, "no extra attempt charged across laps/restart");
    assert_eq!(t_after_laps.runs, vec![run_id.clone()], "no replacement competition");
    assert_eq!(t_after_laps.passes.len(), 1, "no extra settled pass");
    assert_eq!(s_after_laps.passes.len(), 1, "no extra graph pass");
    assert_eq!(count(&counter), 1, "verify.e2e was not re-executed");
    assert_eq!(chat_notice_count(&talks, &chat.id), 1, "no repeated chat page");

    // --- Explicit owner release: allowed ONCE, does not bypass verify_current_head,
    //     consumes the approval so restarts never reuse it, and stays at one chat notice.
    queue
        .modify(&task_id, |t| {
            t.release_operator("cli", None);
            true
        })
        .unwrap();
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    magi::chat_notify::sweep(&queue, &talks, &home);
    magi::notice_delivery::sweep(&queue, &talks, &home, &fx.repo);

    let t_owner = queue.get(&task_id).unwrap();
    let s_owner = RunState::load_under(&run_id, &home).unwrap();
    assert_eq!(t_owner.status, TaskStatus::Held);
    assert_eq!(t_owner.runs, vec![run_id.clone()], "resumed same run, no new competition");
    assert_eq!(s_owner.passes.len(), 2, "explicit owner release ran one pass");
    assert_eq!(s_owner.head_e2e_resumptions.len(), 1, "owner approval durably consumed");
    assert_eq!(s_owner.head_e2e_resumptions[0].generation, 1);
    assert_eq!(count(&counter), 1, "verify_current_head kept old red without re-running or bypassing");
    assert_eq!(t_owner.passes.last().unwrap().cost, PassCost::Refunded);
    assert_eq!(t_owner.head_e2e_stop_id(), Some(stop1.id.as_str()));
    assert_eq!(chat_notice_count(&talks, &chat.id), 1, "same stop evidence does not re-page");

    // Re-entering runnable without a NEW owner release must NOT reuse generation 1.
    queue
        .modify(&task_id, |t| {
            t.status = TaskStatus::Failed;
            true
        })
        .unwrap();
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    let t_no_reuse = queue.get(&task_id).unwrap();
    let s_no_reuse = RunState::load_under(&run_id, &home).unwrap();
    assert_eq!(t_no_reuse.status, TaskStatus::Held);
    assert_eq!(s_no_reuse.passes.len(), 2, "consumed owner approval was not reused");
    assert_eq!(count(&counter), 1);

    // The new base is already an ancestor of the failed candidate. Sync
    // consumes the movement without changing HEAD or the failure identity.
    git_sync(&fx.repo, &["push", "origin", &format!("{intermediate}:main")]);
    let report = triage::run_once(&queue, &questions, Some(&config_path), Timestamp::now());
    assert_eq!(report.resumed, vec![task_id.clone()]);
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    let incorporated = RunState::load_under(&run_id, &home).unwrap();
    assert_eq!(incorporated.base_sync.as_ref().unwrap().tip, intermediate);
    assert_eq!(magi::head_e2e_stop::conclusive(&incorporated).unwrap().id, stop1.id);
    let incorporated_task = queue.get(&task_id).unwrap();
    assert_eq!(incorporated_task.status, TaskStatus::Held);
    let reopened = Queue::at(home.join("queue"));
    assert!(triage::run_once(&reopened, &questions, Some(&config_path), Timestamp::now()).resumed.is_empty());
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    assert_eq!(queue.get(&task_id).unwrap().passes.len(), incorporated_task.passes.len());
    assert_eq!(count(&counter), 1);
    magi::chat_notify::sweep(&queue, &talks, &home);
    magi::notice_delivery::sweep(&queue, &talks, &home, &fx.repo);
    assert_eq!(chat_notice_count(&talks, &chat.id), 1);
    git_sync(&fx.repo, &["merge", "--ff-only", &intermediate]);

    // --- Changed remote base SHA + distinct new failure -> automatic recovery runs fresh E2E
    //     and delivers a second actionable chat notice for the distinct new stop.
    std::fs::write(&mode_file, "fail-v2").unwrap();
    std::fs::write(fx.repo.join("base-moved.txt"), "base change\n").unwrap();
    git_sync(&fx.repo, &["add", "base-moved.txt"]);
    git_sync(&fx.repo, &["commit", "-m", "chore: advance base"]);
    git_sync(&fx.repo, &["push", "origin", "main"]);
    git_sync(&fx.repo, &["fetch", "origin", "main"]);

    let report = triage::run_once(&queue, &questions, Some(&config_path), Timestamp::now());
    assert_eq!(report.resumed, vec![task_id.clone()], "changed base unlocks triage resume");
    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    magi::chat_notify::sweep(&queue, &talks, &home);
    magi::notice_delivery::sweep(&queue, &talks, &home, &fx.repo);

    let t2 = queue.get(&task_id).unwrap();
    let s2 = RunState::load_under(&run_id, &home).unwrap();
    assert_eq!(t2.status, TaskStatus::Held);
    assert_eq!(count(&counter), 2, "fresh E2E ran on rebased head");
    assert!(t2.attempts >= 1, "fresh verification pass spent an attempt");
    let stop2 = magi::head_e2e_stop::conclusive(&s2).expect("new conclusive stop");
    assert_ne!(stop2.id, stop1.id, "distinct head/base/output must yield a new stop id");
    assert_eq!(
        chat_notice_count(&talks, &chat.id),
        2,
        "distinct new failure must deliver a new actionable chat turn"
    );

    // --- Changed candidate HEAD + passing E2E -> triage preserves attempts (does NOT reset to 0)
    //     and scheduler completes the task cleanly.
    let attempts_before_head_change = t2.attempts;
    assert!(attempts_before_head_change >= 1);
    std::fs::write(&mode_file, "pass").unwrap();
    let winner_wt = s2.winner().expect("winner").worktree.clone();
    std::fs::write(winner_wt.join("typography.txt"), "fixed typography\n").unwrap();
    git_sync(&winner_wt, &["add", "typography.txt"]);
    git_sync(&winner_wt, &["commit", "-m", "fix: typography kerning"]);

    let report = triage::run_once(&queue, &questions, Some(&config_path), Timestamp::now());
    assert_eq!(report.resumed, vec![task_id.clone()], "changed candidate HEAD unlocks triage resume");
    let t_resumed = queue.get(&task_id).unwrap();
    assert_eq!(t_resumed.status, TaskStatus::Queued);
    assert_eq!(
        t_resumed.attempts, attempts_before_head_change,
        "automatic head_e2e recovery must preserve attempts rather than calling Task::release"
    );

    run_serve_lap(serve_opts(&fx.repo, &config_path, &wt_root)).await;
    let t_done = queue.get(&task_id).unwrap();
    let s_done = RunState::load_under(&run_id, &home).unwrap();
    assert_eq!(
        t_done.status,
        TaskStatus::Held,
        "unlanded Ready settles to awaiting landing: {:?}",
        t_done.last_error
    );
    assert!(
        t_done.has_awaiting_landing_cause(),
        "expected awaiting-landing hold: {:?}",
        t_done.hold_reason
    );
    assert_eq!(s_done.status, RunStatus::Ready);
    assert_eq!(count(&counter), 3, "passing E2E ran on the updated candidate HEAD");
    assert!(s_done.head_e2e.as_ref().is_some_and(|e| e.passed));
}
}
