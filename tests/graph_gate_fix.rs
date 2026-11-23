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
    let deny_second = fx.tmp.path().join("deny-second");
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

    for scenario in ["success", "timeout", "mixed-outcomes", "moved-head-mixed", "repeated-failure", "moved-head", "moved-head-fails", "changed-checkpoint", "attempt-limit", "final-gate", "base-moved"] {
        let mixed = matches!(scenario, "mixed-outcomes" | "moved-head-mixed");
        let moved_head = matches!(scenario, "moved-head" | "moved-head-fails" | "moved-head-mixed");
        let _ = std::fs::remove_file(&deny_second);
        let _ = std::fs::remove_file(&fixed);
        let mut task = Task::new("create note.txt".into(), "create note.txt".into(), fx.repo.clone(), Source::Human);
        let claim = queue.file_claimed(&mut task).unwrap();
        let mut config = fx.config.clone();
        if scenario == "timeout" {
            config.graph.timeout_verify = Some(3);
            config.verify.e2e[0] = format!(
                "{}; rc=$?; if [ \"$rc\" -ne 0 ]; then sleep 20; fi; exit \"$rc\"",
                config.verify.e2e[0],
            );
        }
        if mixed {
            config.verify.e2e.push(format!(
                "test ! -f {} || {{ echo 'incremental cache admission blocked'; exit 1; }}",
                quote(&deny_second),
            ));
        }
        let mut runner = Runner::start(&fx.repo, task.instruction.clone(), config, Origin::queue(&task.id))
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
        if scenario == "timeout" {
            assert_eq!(e2e.outcomes[0].code, None);
            assert!(!e2e.outcomes[0].resource_blocked);
            assert!(e2e.outcomes[0].output_tail.contains("timed out after 3s"));
        }
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
            assert_eq!(task.runs.as_slice(), std::slice::from_ref(&run));
        }

        drop(claim);
        if scenario == "attempt-limit" {
            task.attempt_limit = std::num::NonZeroUsize::new(1);
            queue.put(&mut task).unwrap();
        }
        if matches!(scenario, "success" | "timeout") {
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
        if mixed {
            std::fs::write(&deny_second, "").unwrap();
        }
        if !mixed && !matches!(scenario, "repeated-failure" | "moved-head-fails") {
            std::fs::write(&fixed, "").unwrap();
        }
        if moved_head {
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
        assert_eq!(task.runs.as_slice(), std::slice::from_ref(&run), "{scenario}");
        assert_eq!(saved.gate_fixes.len(), 1, "{scenario}");
        if scenario == "base-moved" {
            let synced = rev_parse(&winner, "HEAD").await.unwrap();
            assert_ne!(synced, repaired);
            assert_eq!(observed, format!("e2e {repaired}\ne2e {synced}\ngate {synced}\n"));
            assert_eq!(saved.gate_head.as_deref(), Some(synced.as_str()));
            assert_eq!(task.status, TaskStatus::Done);
            continue;
        }
        if moved_head {
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
                if mixed {
                    let outcomes = &saved.head_e2e.as_ref().unwrap().outcomes;
                    assert_eq!(outcomes[0].code, Some(1));
                    assert!(outcomes[1].resource_blocked);
                    assert_eq!(task.attempts, 2);
                    continue;
                }
                let mut interrupted = saved;
                interrupted.status = RunStatus::Gating;
                interrupted.head_e2e.as_mut().unwrap().outcomes.clear();
                interrupted.save().unwrap();
                assert!(magi::chat_control::resume(&queue, &magi::run::home(), &run).is_err());
                assert_eq!(queue.get(&task.id).unwrap().attempts, 2);
                assert_eq!(std::fs::read_to_string(&log).unwrap(), observed);
            }
            continue;
        }
        let expected = match scenario {
            "mixed-outcomes" | "repeated-failure" => format!("e2e {repaired}\n"),
            "final-gate" => format!("e2e {repaired}\ngate {repaired}\n"),
            _ => String::new(),
        };
        assert_eq!(observed, expected, "{scenario}");
        assert!(saved.merge.is_none() && saved.pr.is_none(), "{scenario}");
        assert_eq!(task.status, TaskStatus::Held, "{scenario}");
        if scenario == "mixed-outcomes" {
            let outcomes = &saved.gate_repair_e2e.as_ref().unwrap().outcomes;
            assert_eq!(outcomes.len(), 2);
            assert_eq!(outcomes[0].code, Some(1));
            assert!(!outcomes[0].resource_blocked && outcomes[1].resource_blocked);
            assert_eq!(task.attempts, 2);
            assert!(magi::chat_control::resume(&queue, &magi::run::home(), &run).is_err());
        }
        if matches!(scenario, "repeated-failure" | "final-gate") {
            assert!(magi::chat_control::resume(&queue, &magi::run::home(), &run).is_err());
            let _ = std::fs::remove_file(&gate_closed);
        }
        if scenario == "repeated-failure" {
            task.review_of = Some(saved.winner().unwrap().branch.clone());
            task.requeue();
            queue.put(&mut task).unwrap();
            let claim = queue.claim(&task.id).unwrap();
            magi::daemon::run_claimed(&opts, &queue, &mut task).await;
            drop(claim);
            assert_eq!(task.status, TaskStatus::Held);
            assert_eq!(task.runs.as_slice(), std::slice::from_ref(&run));
            std::fs::write(&fixed, "").unwrap();
            let config = fx.tmp.path().join("successor.toml");
            std::fs::write(&config, toml::to_string(&fx.config).unwrap()).unwrap();
            let branch = &saved.winner().unwrap().branch;
            let out = common::magi_cli(&magi::run::home())
                .current_dir(&fx.repo)
                .args(["review", branch, "--task", &task.id, "--merge", "none", "--config"])
                .arg(config)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let successor = queue.get(&task.id).unwrap();
            assert_eq!(successor.runs.len(), 2);
            assert_eq!(successor.runs[0], run);
            assert_eq!(successor.status, TaskStatus::Done);

            let remote_only = "remote-only-review";
            let refspec = format!("{repaired}:refs/heads/{remote_only}");
            git(&fx.repo, &["push", "origin", &refspec]).await.unwrap();
            assert!(rev_parse(&fx.repo, &format!("refs/heads/{remote_only}")).await.is_err());
            let out = common::magi_cli(&magi::run::home())
                .current_dir(&fx.repo)
                .args(["review", remote_only, "--task", &task.id, "--merge", "none", "--config"])
                .arg(fx.tmp.path().join("successor.toml"))
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            let task = queue.get(&task.id).unwrap();
            assert_eq!(task.runs.len(), 3);
            let review = RunState::load(task.runs.last().unwrap()).unwrap();
            assert_eq!(review.winner().unwrap().branch, remote_only);
            assert_eq!(rev_parse(&review.winner().unwrap().worktree, "HEAD").await.unwrap(), repaired);
        }
    }
}
}

common::e2e! {
async fn post_repair_rebase_pushes_only_after_verification() {
    use magi::git::{git, remote_head, rev_parse};
    use magi::queue::{Queue, Source, Task};
    use magi::run::{Origin, RunState};

    let mut fx = fixture(common::home_lock().await, Judges::Unanimous, false);
    let quote = |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\\', "/"));
    let ready = fx.tmp.path().join("ready");
    let log = fx.tmp.path().join("verification-log");
    let bare = fx.tmp.path().join("fixture-origin.git");
    let branch = "review-source";
    git(&fx.repo, &["switch", "-c", branch]).await.unwrap();
    std::fs::write(fx.repo.join("note.txt"), "review this patch\n").unwrap();
    for args in [&["add", "note.txt"][..], &["commit", "-m", "Add review source"], &["push", "origin", branch], &["switch", "main"]] {
        git(&fx.repo, args).await.unwrap();
    }
    let old = remote_head(&fx.repo, "origin", branch).await.unwrap().unwrap();
    let remote_at_gate = fx.tmp.path().join("remote-at-gate");
    std::fs::write(&remote_at_gate, &old).unwrap();
    fx.config.merge.mode = magi::config::MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.github_text_guard = false;
    fx.config.graph.review_rounds = 1;
    fx.config.graph.gate_fix_rounds = 1;
    fx.config.graph.rerun_failed_e2e = false;
    fx.config.verify.e2e = vec![format!(
        "echo \"e2e $(git rev-parse HEAD)\" >> {}; test ! -f gatefix.txt || test -f {}",
        quote(&log), quote(&ready)
    )];
    fx.config.verify.gate = vec![format!(
        "{GATE_NEEDS_FIX}; remote=$(git --git-dir={} rev-parse refs/heads/{branch}) && test \"$remote\" = \"$(cat {})\" && echo \"gate $(git rev-parse HEAD)\" >> {}",
        quote(&bare), quote(&remote_at_gate), quote(&log)
    )];
    let queue = Queue::open();
    let mut task = Task::new("Review source".into(), "Review source".into(), fx.repo.clone(), Source::Human);
    let claim = queue.file_claimed(&mut task).unwrap();
    let mut runner = Runner::review(&fx.repo, branch, fx.config, Origin::queue(&task.id)).await.unwrap();
    task.instruction = runner.state.instruction.clone();
    let run = runner.state.id.clone();
    task.start(run.clone());
    queue.put(&mut task).unwrap();
    let result = runner.execute().await;
    assert!(result.is_ok(), "{result:?}");
    magi::daemon::finish_attempt(2, &queue, &mut task, &runner.state, &[], result);
    let checkout = runner.state.winner().unwrap().worktree.clone();
    let repaired = rev_parse(&checkout, "HEAD").await.unwrap();
    assert_eq!(runner.state.status, RunStatus::Blocked);
    assert_eq!(runner.state.gate_repair_e2e.as_ref().unwrap().head, repaired);
    drop(claim);

    std::fs::write(fx.repo.join("upstream.txt"), "base update\n").unwrap();
    for args in [&["add", "upstream.txt"][..], &["commit", "-m", "Advance base"], &["push", "origin", "main"]] {
        git(&fx.repo, args).await.unwrap();
    }
    std::fs::write(&ready, "").unwrap();
    std::fs::write(&log, "").unwrap();
    let bin = fx.tmp.path().join("fake-bin");
    std::fs::create_dir(&bin).unwrap();
    #[cfg(windows)]
    std::fs::copy(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/where.exe"),
        bin.join("gh.exe"),
    ).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap();
    let resume = || common::magi_cli(&magi::run::home())
        .current_dir(&fx.repo)
        .env("PATH", &path)
        .args(["run", "--resume", &run])
        .output()
        .unwrap();
    let out = resume();
    let mut saved = RunState::load(&run).unwrap();
    let head = rev_parse(&checkout, "HEAD").await.unwrap();
    assert_ne!(head, repaired, "{out:?}");
    assert_eq!(saved.gate_head.as_deref(), Some(head.as_str()), "{out:?}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("e2e {repaired}\ne2e {head}\ngate {head}\n"));
    assert!(!magi::git::is_ancestor(&fx.repo, &old, &head).await);
    assert_eq!(remote_head(&fx.repo, "origin", branch).await.unwrap().as_deref(), Some(head.as_str()), "{out:?}");
    assert!(saved.pr.is_none());
    assert_eq!(saved.pr_pushed_head.as_deref(), Some(head.as_str()));

    // Restore the boundary before the forge response, keeping the saved push receipt.
    saved.status = RunStatus::Gating;
    saved.merge = None;
    saved.save().unwrap();
    std::fs::write(&remote_at_gate, &head).unwrap();
    std::fs::write(&log, "").unwrap();
    std::fs::write(fx.repo.join("upstream.txt"), "second base update\n").unwrap();
    for args in [&["add", "upstream.txt"][..], &["commit", "-m", "Advance base again"], &["push", "origin", "main"]] {
        git(&fx.repo, args).await.unwrap();
    }
    let out = resume();
    let next = rev_parse(&checkout, "HEAD").await.unwrap();
    assert_ne!(next, head, "{out:?}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("e2e {next}\ngate {next}\n"));
    assert!(!magi::git::is_ancestor(&fx.repo, &head, &next).await);
    assert_eq!(remote_head(&fx.repo, "origin", branch).await.unwrap().as_deref(), Some(next.as_str()), "{out:?}");
    let saved = RunState::load(&run).unwrap();
    assert_eq!(saved.pr_pushed_head.as_deref(), Some(next.as_str()));
    assert!(saved.pr.is_none());
}
}
