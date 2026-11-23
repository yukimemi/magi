//! A failing `verify.gate` gets a bounded fix round instead of ending the run
//! blocked - but only for an ordinary non-zero exit that printed something.
mod common;

use common::{Judges, fixture};
use magi::graph::Runner;
use magi::run::RunStatus;

/// Red until `gatefix.txt` exists, saying why on stdout: the shape of a real
/// lint failure, with the output a fixer can act on.
const GATE_NEEDS_FIX: &str = "test -f gatefix.txt || { echo 'gatefix.txt is missing'; exit 1; }";

fn gate_events(runner: &Runner) -> String {
    runner
        .state
        .events
        .iter()
        .filter(|e| e.node == "gate")
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

common::e2e! {
async fn a_gate_that_fails_once_is_fixed_and_the_run_proceeds_to_merge() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.gate_fixes.len(), 1, "{}", gate_events(&runner));
    assert!(state.gate_fixes[0].committed);
    assert!(state.gate_fixes[0].failed[0].output_tail.contains("gatefix.txt is missing"));
    assert!(state.gate_ran);
    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
    assert_eq!(state.status, RunStatus::Ready, "{}", gate_events(&runner));
    assert!(state.merge.as_ref().is_some_and(|m| m.ok), "the run must reach merge");

    // The fixer was told the failure was the gate's, not a reviewer's.
    let prompt = std::fs::read_to_string(state.dir().join("artifacts/gate-fix-1.prompt.md"))
        .expect("gate-fix prompt artifact");
    assert!(prompt.contains("failed the verification gate"));
    assert!(prompt.contains("gatefix.txt is missing"));
    assert!(!prompt.contains("Your patch was reviewed"));
}
}

common::e2e! {
async fn a_gate_that_keeps_failing_blocks_after_the_cap_with_its_last_output() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.gate_fix_rounds = 2;
    fx.config.verify.gate = vec!["echo still red; exit 1".to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.gate_fixes.len(), 2, "{}", gate_events(&runner));
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.gate_ran);
    assert!(state.gate.iter().any(|o| !o.ok()));
    assert!(state.gate[0].output_tail.contains("still red"));
    let events = gate_events(&runner);
    assert!(events.contains("2 gate-fix round(s)"), "{events}");
    assert!(events.contains("still red"), "{events}");
    assert!(state.merge.is_none());
}
}

common::e2e! {
async fn a_gate_timeout_is_not_a_code_failure_and_spends_no_fix_round() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.timeout_verify = Some(1);
    fx.config.verify.gate = vec!["sleep 5".to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.gate_fixes.is_empty(), "{}", gate_events(&runner));
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.gate.iter().any(|o| o.code.is_none()));
}
}

common::e2e! {
async fn a_gate_fix_that_changes_nothing_ends_blocked_after_one_round() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.gate_fix_rounds = 2;
    for agent in &mut fx.config.agents {
        agent.env.insert("MOCK_GATE_FIX_NOOP".to_owned(), "1".to_owned());
    }
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.gate_fixes.len(), 1, "a no-op fix must not be retried");
    assert!(!state.gate_fixes[0].committed);
    assert_eq!(state.status, RunStatus::Blocked);
}
}

common::e2e! {
async fn owner_resume_rechecks_the_retained_gate_repair_before_publication() {
    use magi::git::{git, rev_parse};
    use magi::queue::{Queue, Source, Task, TaskStatus};
    use magi::run::{Origin, RunState};

    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.gate_fix_rounds = 1;
    fx.config.graph.review_rounds = 1;
    fx.config.graph.rerun_failed_e2e = false;
    fx.config.graph.implementers = 1;
    let quote = |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\\', "/"));
    let log = fx.tmp.path().join("verification");
    let fixed = fx.tmp.path().join("e2e-ready");
    let gate_closed = fx.tmp.path().join("gate-closed");
    fx.config.verify.e2e = vec![format!(
        "echo \"e2e $(git rev-parse HEAD)\" >> {}; test ! -f gatefix.txt || test -f {}",
        quote(&log), quote(&fixed),
    )];
    fx.config.verify.gate = vec![format!(
        "echo \"gate $(git rev-parse HEAD)\" >> {}; {GATE_NEEDS_FIX}; test ! -f {}",
        quote(&log), quote(&gate_closed),
    )];
    let queue = Queue::open();
    let opts = magi::daemon::Opts { repo: fx.repo.clone(), ..Default::default() };

    for scenario in ["success", "repeated-failure", "moved-head", "moved-head-fails", "changed-checkpoint", "attempt-limit", "final-gate", "base-moved"] {
        let _ = std::fs::remove_file(&fixed);
        let mut task = Task::new("create note.txt".into(), "create note.txt".into(), fx.repo.clone(), Source::Human);
        let claim = queue.file_claimed(&mut task).unwrap();
        let mut runner = Runner::start(&fx.repo, task.instruction.clone(), fx.config.clone(), Origin::queue(&task.id))
            .await.unwrap();
        let run = runner.state.id.clone();
        task.start(run.clone());
        queue.put(&mut task).unwrap();
        let result = runner.execute().await;
        assert!(result.is_ok(), "{result:?}");
        magi::daemon::finish_attempt(opts.max_attempts, &queue, &mut task, &runner.state, &[], result);
        drop(runner);

        // This is a run created by the normal gate path, with no new receipt.
        let mut stopped = RunState::load(&run).unwrap();
        let winner = stopped.winner().unwrap().worktree.clone();
        let repaired = rev_parse(&winner, "HEAD").await.unwrap();
        let old_gate = stopped.gate_head.clone().unwrap();
        assert_eq!(stopped.status, RunStatus::Blocked);
        assert!(stopped.reviews.last().unwrap().clean && stopped.gate_ran);
        assert_ne!(old_gate, repaired);
        let e2e = stopped.gate_repair_e2e.as_ref().unwrap();
        assert_eq!(e2e.head, repaired);
        assert!(!e2e.passed && e2e.outcomes.iter().any(|o| !o.ok()));
        assert_eq!(task.status, TaskStatus::Held);
        assert_eq!(task.attempts, 1);

        // An ordinary retry tick must not run either command or mint a successor.
        if scenario == "success" {
            std::fs::write(&log, "").unwrap();
            task.status = TaskStatus::Failed;
            queue.put(&mut task).unwrap();
            magi::daemon::run_claimed(&opts, &queue, &mut task).await;
            assert_eq!(task.status, TaskStatus::Held);
            assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
            assert_eq!(task.runs, [run.clone()]);
        }

        drop(claim);
        if scenario == "attempt-limit" {
            task.attempt_limit = std::num::NonZeroUsize::new(1);
            queue.put(&mut task).unwrap();
        }
        if scenario == "success" {
            std::fs::write(&fixed, "").unwrap();
            std::fs::write(&log, "").unwrap();
            let out = common::magi_cli(&magi::run::home())
                .current_dir(&fx.repo)
                .args(["run", "--resume", &run])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "CLI resume: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let saved = RunState::load(&run).unwrap();
            let observed = std::fs::read_to_string(&log).unwrap();
            assert_eq!(observed, format!("e2e {repaired}\ngate {repaired}\n"));
            assert_eq!(saved.gate_head.as_deref(), Some(repaired.as_str()));
            assert_eq!(saved.gate_fixes.len(), 1);
            assert_eq!(saved.status, RunStatus::Ready);
            assert!(saved.merge.is_some_and(|m| m.ok));
            let settled = queue.get(&task.id).unwrap();
            assert_eq!(settled.status, TaskStatus::Done);
            assert_eq!(settled.runs, [run]);
            assert_eq!(settled.attempts, 2);
            continue;
        }
        task = magi::chat_control::resume(&queue, &magi::run::home(), &run).unwrap();
        let claim = queue.claim(&task.id).unwrap();
        assert_eq!(task.attempts, 1, "a resume pin does not refill attempts");
        if !matches!(scenario, "repeated-failure" | "moved-head-fails") {
            std::fs::write(&fixed, "").unwrap();
        }
        if matches!(scenario, "moved-head" | "moved-head-fails") {
            git(&winner, &["commit", "--allow-empty", "-m", "External head movement"]).await.unwrap();
        } else if scenario == "changed-checkpoint" {
            stopped.gate_repair_e2e.as_mut().unwrap().outcomes[0].output_tail = "different failure".into();
            stopped.save().unwrap();
        } else if scenario == "final-gate" {
            std::fs::write(&gate_closed, "").unwrap();
        } else if scenario == "base-moved" {
            std::fs::write(fx.repo.join("upstream.txt"), "independent base change\n").unwrap();
            for args in [
                &["add", "upstream.txt"][..],
                &["commit", "-m", "Advance fixture base"],
                &["push", "origin", "main"],
            ] {
                git(&fx.repo, args).await.unwrap();
            }
        }
        std::fs::write(&log, "").unwrap();
        task = queue.get(&task.id).unwrap();
        magi::daemon::run_claimed(&opts, &queue, &mut task).await;
        drop(claim);
        let saved = RunState::load(&run).unwrap();
        let observed = std::fs::read_to_string(&log).unwrap();
        assert_eq!(task.runs, [run.clone()], "{scenario}");
        assert_eq!(saved.gate_fixes.len(), 1, "{scenario}");
        if scenario == "base-moved" {
            let synced = rev_parse(&winner, "HEAD").await.unwrap();
            assert_ne!(synced, repaired);
            assert_eq!(observed, format!("e2e {repaired}\ne2e {synced}\ngate {synced}\n"));
            assert_eq!(saved.gate_head.as_deref(), Some(synced.as_str()));
            assert_eq!(task.status, TaskStatus::Done);
            continue;
        }
        if matches!(scenario, "moved-head" | "moved-head-fails") {
            let moved = rev_parse(&winner, "HEAD").await.unwrap();
            assert_ne!(moved, repaired);
            assert!(saved.verification_history.iter().any(|record| {
                record.e2e.as_ref().is_some_and(|e2e| e2e.head == repaired && !e2e.passed)
            }));
            if scenario == "moved-head" {
                assert_eq!(observed, format!("e2e {moved}\ngate {moved}\n"));
                assert_eq!(saved.gate_head.as_deref(), Some(moved.as_str()));
                assert_eq!(task.status, TaskStatus::Done);
            } else {
                assert_eq!(observed, format!("e2e {moved}\n"));
                assert_eq!(task.status, TaskStatus::Held);
                assert!(saved.merge.is_none() && saved.pr.is_none());
                assert!(magi::chat_control::resume(&queue, &magi::run::home(), &run).is_err());
            }
            continue;
        }
        let expected = match scenario {
            "repeated-failure" => format!("e2e {repaired}\n"),
            "final-gate" => format!("e2e {repaired}\ngate {repaired}\n"),
            _ => String::new(),
        };
        assert_eq!(observed, expected, "{scenario}");
        assert!(saved.merge.is_none() && saved.pr.is_none(), "{scenario}");
        assert_eq!(task.status, TaskStatus::Held, "{scenario}");
        if matches!(scenario, "repeated-failure" | "final-gate") {
            assert!(magi::chat_control::resume(&queue, &magi::run::home(), &run).is_err());
            let _ = std::fs::remove_file(&gate_closed);
        }
    }
}
}
