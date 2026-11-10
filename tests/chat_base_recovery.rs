//! Real standing-control subprocesses against isolated stores and Git repositories.
#![cfg(unix)]
#[allow(unused_macros, unused_imports)]
mod common;

use magi::{
    config::{AgentKind, AgentSpec, Config},
    queue::{Queue, Source, Task, TaskStatus},
    run::{RunState, RunStatus},
    talk::{self, Talks},
};
use std::{collections::BTreeMap, path::Path};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

struct Fixture {
    tmp: tempfile::TempDir,
    repo: std::path::PathBuf,
    queue: Queue,
    task: Task,
    run: RunState,
    head: String,
    talks: Talks,
    talk: String,
    config: Config,
}

impl Fixture {
    fn new(legacy: bool) -> Self {
        let tmp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        magi::run::set_home(tmp.path().to_owned());
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "fixture"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(repo.join("base"), "base").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        let origin = tmp.path().join("origin.git");
        git(&repo, &["init", "--bare", origin.to_str().unwrap()]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repo, &["push", "origin", "main"]);
        git(&repo, &["checkout", "-b", "fix/retained"]);
        std::fs::write(repo.join("implementation"), "retained implementation").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "implementation"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let script = tmp.path().join("control.py");
        std::fs::write(&script, r#"import os, json, subprocess
fds = tuple(int(os.environ[n]) for n in ['MAGI_CHAT_CONTROL_FD','MAGI_CHAT_CONTROL_RESPONSE_FD','MAGI_CHAT_CONTROL_LOCK_FD'])
p = subprocess.run([os.environ['TEST_BINARY'], 'task', 'control', *json.loads(os.environ['TEST_ARGS'])], pass_fds=fds, capture_output=True, text=True)
open(os.environ['TEST_RESULT'], 'w').write(json.dumps({'ok':p.returncode == 0,'out':p.stdout,'err':p.stderr}))
print('control completed')
"#).unwrap();
        let mut config = Config {
            agents: vec![AgentSpec {
                id: "fixture".into(),
                kind: AgentKind::Command,
                model: None,
                command: vec!["python3".into(), script.to_str().unwrap().into()],
                extra_args: vec![],
                env: BTreeMap::new(),
                prompt_delivery: None,
                max_turn_secs: None,
            }],
            ..Config::default()
        };
        config.agents[0]
            .env
            .extend(common::magi_cli_env(tmp.path(), "TEST_BINARY"));
        config.agents[0].env.insert(
            "TEST_RESULT".into(),
            tmp.path().join("result.json").to_str().unwrap().into(),
        );
        let talks = Talks::at(tmp.path().join("talks"));
        let talk = talk::begin(&talks, &config, repo.clone(), Some("fixture"))
            .unwrap()
            .id;
        let mut run = RunState::new(
            repo.clone(),
            "main".into(),
            base.clone(),
            "keep implementation".into(),
            config.clone(),
        );
        run.begin_pass();
        run.status = RunStatus::Blocked;
        run.driver_exited = true;
        run.candidates.push(serde_json::from_value(serde_json::json!({"index":0,"label":"A","agent":"fixture","branch":"fix/retained","worktree":repo})).unwrap());
        run.tally = Some(serde_json::from_value(serde_json::json!({"first_choice":{},"borda":{},"winner":"A","unanimous_initial":true,"deliberated":false,"changed_votes":0,"unanimous_final":true})).unwrap());
        let detail = if legacy {
            "main moved 1 commit(s) ahead of fix/retained after 4 rebase(s); rebasing again would only race it"
        } else {
            "main moved 1 commit(s) ahead of fix/retained after 4 synchronization(s); synchronizing again would only race it"
        };
        run.base_sync = Some(magi::run::BaseSync {
            tip: base,
            behind: 1,
            attempts: 4,
            conflict: Some(detail.into()),
            already_in: None,
        });
        let queue = Queue::at(tmp.path().join("queue"));
        let mut task = Task::new(
            "retained".into(),
            run.instruction.clone(),
            repo.clone(),
            Source::Human,
        );
        task.link_run(&run.id);
        task.hold_manual(Some("base stopped".into()));
        queue.put(&mut task).unwrap();
        run.origin = Some(magi::run::Origin::queue(&task.id));
        if !legacy {
            run.base_sync_recovery = serde_json::from_value(serde_json::json!({
                "stop": {"kind":"exhausted", "head":head, "tip":run.base_sync.as_ref().unwrap().tip,
                    "attempts":4, "detail":detail, "at":run.updated_at},
                "authorizations":[], "episodes":[{"id":1,"reason":"initial","used":4,"synced_head":null,"boundary":null}]
            })).unwrap();
        }
        run.save_under(tmp.path()).unwrap();
        Self {
            tmp,
            repo,
            queue,
            task,
            run,
            head,
            talks,
            talk,
            config,
        }
    }

    async fn control(&mut self, args: Vec<String>) -> serde_json::Value {
        self.config.agents[0]
            .env
            .insert("TEST_ARGS".into(), serde_json::to_string(&args).unwrap());
        let mut talk = self.talks.get(&self.talk).unwrap();
        let lease = self.talks.claim_turn(&self.talk).unwrap().unwrap();
        talk::say(
            &lease,
            &mut talk,
            &self.talks,
            &self.config,
            "apply explicit owner request",
            vec![],
        )
        .await
        .unwrap();
        serde_json::from_slice(&std::fs::read(self.tmp.path().join("result.json")).unwrap())
            .unwrap()
    }
    async fn recover(&mut self, request: &str) -> serde_json::Value {
        self.control(vec![
            "recover-base".into(),
            self.run.id.clone(),
            "--head".into(),
            self.head.clone(),
            "--request".into(),
            request.into(),
        ])
        .await
    }
    fn state(&self) -> RunState {
        RunState::load_under(&self.run.id, self.tmp.path()).unwrap()
    }
    fn receipt(&self, request: &str) -> std::path::PathBuf {
        self.tmp
            .path()
            .join("base-recovery-controls")
            .join(&self.talk)
            .join(format!("{request}.json"))
    }
}

async fn check_head_prefix(legacy: bool, length: usize) {
    let _home = common::home_lock().await;
    let mut f = Fixture::new(legacy);
    let full_head = f.head.clone();
    for invalid in [
        full_head[..6].to_owned(),
        format!("{full_head}0"),
        "xyz1234".into(),
        format!(
            "{}{}",
            if full_head.starts_with('0') { '1' } else { '0' },
            &full_head[1..7]
        ),
    ] {
        f.head = invalid;
        let response = f.recover("invalid").await;
        assert_eq!(response["ok"], false, "{response}");
    }
    f.head = full_head[..length].to_owned();
    if !legacy {
        let mut state = f.state();
        let stop = state.base_sync_recovery.stop.as_mut().unwrap();
        // Even a matching request prefix must not bless a HEAD
        // that differs from immutable stop evidence.
        stop.head = format!("{}0", &full_head[..39]);
        if stop.head == full_head {
            stop.head = format!("{}1", &full_head[..39]);
        }
        state.save_under(f.tmp.path()).unwrap();
        let response = f.recover("stale-stop").await;
        assert_eq!(response["ok"], false, "{response}");
        assert!(
            response["err"]
                .as_str()
                .unwrap()
                .contains("recorded stop head")
        );
        assert!(f.state().base_sync_recovery.authorizations.is_empty());
        assert!(!f.receipt("stale-stop").exists());
        state.base_sync_recovery.stop.as_mut().unwrap().head = full_head.clone();
        state.save_under(f.tmp.path()).unwrap();
    }
    let response = f.recover("prefix").await;
    assert_eq!(response["ok"], true, "{response}");
    let state = f.state();
    assert_eq!(state.base_sync_recovery.authorizations.len(), 1);
    assert_eq!(
        state.base_sync_recovery.stop.as_ref().unwrap().head,
        full_head
    );
    assert_eq!(f.queue.get(&f.task.id).unwrap().status, TaskStatus::Queued);
    let response = f.recover("prefix").await;
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
}

common::e2e! {
    async fn standing_base_recovery_accepts_7_digit_head_and_checks_stop() {
        check_head_prefix(false, 7).await;
    }
}

common::e2e! {
    async fn standing_base_recovery_accepts_12_digit_head_and_checks_stop() {
        check_head_prefix(false, 12).await;
    }
}

common::e2e! {
    async fn standing_base_recovery_accepts_40_digit_head_and_checks_stop() {
        check_head_prefix(false, 40).await;
    }
}

common::e2e! {
    async fn standing_base_recovery_accepts_7_digit_head_historical() {
        check_head_prefix(true, 7).await;
    }
}

common::e2e! {
    async fn standing_base_recovery_accepts_12_digit_head_historical() {
        check_head_prefix(true, 12).await;
    }
}

common::e2e! {
    async fn standing_base_recovery_accepts_40_digit_head_historical() {
        check_head_prefix(true, 40).await;
    }
}

#[test]
fn standing_base_recovery_queues_once_and_repeated_delivery_never_releases_a_new_hold() {
    common::isolated(
        "standing_base_recovery_queues_once_and_repeated_delivery_never_releases_a_new_hold",
        || async {
            let _home = common::home_lock().await;
            let mut f = Fixture::new(false);
            let response = f.control(vec!["resume".into(), f.run.id.clone()]).await;
            assert_eq!(response["ok"], false, "{response}");
            assert!(response["err"].as_str().unwrap().contains("recover-base"));
            let response = f.recover("once").await;
            assert_eq!(response["ok"], true, "{response}");
            let state = f.state();
            assert_eq!(state.base_sync_recovery.authorizations.len(), 1);
            assert_eq!(state.base_sync.as_ref().unwrap().attempts, 4);
            assert_eq!(state.candidates[0].branch, "fix/retained");
            let mut task = f.queue.get(&f.task.id).unwrap();
            assert_eq!(task.status, TaskStatus::Queued);
            assert_eq!(
                task.resume_override.as_ref().unwrap().pinned_run.as_deref(),
                Some(f.run.id.as_str())
            );
            assert_eq!(task.execution_runs(), [f.run.id.clone()]);
            let generation = task.resumption.as_ref().unwrap().generation;
            task.hold_manual(Some("new owner decision".into()));
            f.queue.put(&mut task).unwrap();
            let response = f.recover("once").await;
            assert_eq!(response["ok"], true, "{response}");
            assert!(
                response["out"]
                    .as_str()
                    .unwrap()
                    .contains("already applied")
            );
            assert_eq!(f.queue.get(&task.id).unwrap().status, TaskStatus::Held);
            assert_eq!(
                f.queue
                    .get(&task.id)
                    .unwrap()
                    .resumption
                    .unwrap()
                    .generation,
                generation
            );
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
            f.head = "a".repeat(40);
            assert_eq!(f.recover("once").await["ok"], false);
        },
    );
}

#[test]
fn standing_base_recovery_restarts_between_authorization_and_queue_publication() {
    common::isolated(
        "standing_base_recovery_restarts_between_authorization_and_queue_publication",
        || async {
            let _home = common::home_lock().await;
            let mut f = Fixture::new(true);
            let full_head = f.head.clone();
            f.head.truncate(7);
            let obstruction = f.queue.path_of(&f.task.id).with_extension("json.tmp");
            std::fs::create_dir(&obstruction).unwrap();
            let response = f.recover("interrupted").await;
            assert_eq!(response["ok"], false, "{response}");
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
            assert_eq!(f.queue.get(&f.task.id).unwrap().status, TaskStatus::Held);
            std::fs::remove_dir(obstruction).unwrap();
            let response = f.recover("interrupted").await;
            assert_eq!(response["ok"], true, "{response}");
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
            let path = f.receipt("interrupted");
            let mut receipt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            receipt["applied"] = false.into();
            std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            let mut state = f.state();
            state.base_sync_recovery.authorizations[0].consumed = true;
            state.save_under(f.tmp.path()).unwrap();
            assert_eq!(f.recover("interrupted").await["ok"], true);
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
            assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), full_head);
        },
    );
}

#[test]
fn standing_base_recovery_refuses_stale_head_instructions_claims_and_ineligible_stops() {
    common::isolated(
        "standing_base_recovery_refuses_stale_head_instructions_claims_and_ineligible_stops",
        || async {
            let _home = common::home_lock().await;
            let mut f = Fixture::new(false);
            let original_head = f.head.clone();
            f.head = "a".repeat(40);
            assert_eq!(f.recover("head").await["ok"], false);
            f.head = original_head;
            let claim = f.queue.claim(&f.task.id).unwrap();
            assert_eq!(f.recover("claimed").await["ok"], false);
            drop(claim);
            std::fs::write(f.repo.join("dirty"), "do not lose").unwrap();
            assert_eq!(f.recover("dirty").await["ok"], false);
            std::fs::remove_file(f.repo.join("dirty")).unwrap();
            let mut task = f.queue.get(&f.task.id).unwrap();
            let original_instruction = task.instruction.clone();
            task.instruction = "different criteria".into();
            f.queue.put(&mut task).unwrap();
            assert_eq!(f.recover("instruction").await["ok"], false);
            task.instruction = original_instruction;
            f.queue.put(&mut task).unwrap();
            for (index, detail) in [
                "fix/retained conflicts with origin/main and did not rebase",
                "foreign commits: not rebasing over remote",
                "origin refused the push",
                "unknown stop",
            ]
            .iter()
            .enumerate()
            {
                let mut state = f.state();
                state.base_sync.as_mut().unwrap().conflict = Some((*detail).into());
                state.base_sync_recovery.stop = None;
                state.save_under(f.tmp.path()).unwrap();
                let response = f.recover(&format!("stop-{index}")).await;
                assert_eq!(response["ok"], false, "{detail}: {response}");
            }
            assert!(f.state().base_sync_recovery.authorizations.is_empty());
        },
    );
}

#[test]
fn standing_base_recovery_pending_request_refuses_new_base_and_owner_decision() {
    common::isolated(
        "standing_base_recovery_pending_request_refuses_new_base_and_owner_decision",
        || async {
            let _home = common::home_lock().await;
            let mut f = Fixture::new(false);
            let obstruction = f.queue.path_of(&f.task.id).with_extension("json.tmp");
            std::fs::create_dir(&obstruction).unwrap();
            assert_eq!(f.recover("stale").await["ok"], false);
            std::fs::remove_dir(obstruction).unwrap();
            git(&f.repo, &["checkout", "main"]);
            std::fs::write(f.repo.join("new-base"), "upstream moved").unwrap();
            git(&f.repo, &["add", "."]);
            git(&f.repo, &["commit", "-m", "base advanced"]);
            git(&f.repo, &["push", "origin", "main"]);
            git(&f.repo, &["checkout", "fix/retained"]);
            let response = f.recover("stale").await;
            assert_eq!(response["ok"], false, "{response}");
            assert!(response["err"].as_str().unwrap().contains("stale"));
            let mut task = f.queue.get(&f.task.id).unwrap();
            task.hold_manual(Some("later owner decision".into()));
            f.queue.put(&mut task).unwrap();
            assert_eq!(f.recover("stale").await["ok"], false);
            task.runs.push("20260101-000000-ffff".into());
            f.queue.put(&mut task).unwrap();
            assert_eq!(f.recover("stale").await["ok"], false);
            assert_eq!(f.queue.get(&task.id).unwrap().status, TaskStatus::Held);
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
        },
    );
}

#[test]
fn standing_base_recovery_refuses_live_unknown_and_held_execution_lease() {
    common::isolated(
        "standing_base_recovery_refuses_live_unknown_and_held_execution_lease",
        || async {
            use std::os::fd::AsRawFd;
            let _home = common::home_lock().await;
            let mut f = Fixture::new(false);
            let mut state = f.state();
            state.driver_exited = false;
            state.driver_pid = Some(std::process::id());
            state.driver_started_at = magi::proc::process_started_at(std::process::id());
            state.save_under(f.tmp.path()).unwrap();
            let response = f.recover("live").await;
            assert_eq!(response["ok"], false, "{response}");
            state.driver_pid = None;
            state.driver_started_at = None;
            state.save_under(f.tmp.path()).unwrap();
            assert_eq!(f.recover("unknown").await["ok"], false);
            state.driver_exited = true;
            state.save_under(f.tmp.path()).unwrap();
            let lease = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(
                    f.tmp
                        .path()
                        .join("runs")
                        .join(&state.id)
                        .join("driver.lock"),
                )
                .unwrap();
            // SAFETY: this test owns the file and releases the lock on close.
            assert_eq!(
                unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            assert_eq!(f.recover("lease").await["ok"], false);
            drop(lease);
            // A stopped driver may leave a tool holding its inherited lease.
            // Close the parent's copy so only the descendant holds this lock.
            use std::io::BufRead;
            let mut holder = std::process::Command::new("python3")
                .args(["-c", "import fcntl,subprocess,sys; f=open(sys.argv[1],'w'); fcntl.flock(f,fcntl.LOCK_EX); child=subprocess.Popen(['sleep','30'],pass_fds=(f.fileno(),)); f.close(); print(child.pid,flush=True); child.wait()"])
                .arg(f.tmp.path().join("runs").join(&state.id).join("driver.lock"))
                .stdout(std::process::Stdio::piped()).spawn().unwrap();
            let mut pid = String::new();
            std::io::BufReader::new(holder.stdout.take().unwrap())
                .read_line(&mut pid)
                .unwrap();
            let response = f.recover("descendant").await;
            // SAFETY: this fixture just reported its still-running child's PID.
            assert_eq!(
                unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGTERM) },
                0
            );
            assert!(holder.wait().unwrap().success());
            assert_eq!(response["ok"], false, "{response}");
            assert!(f.state().base_sync_recovery.authorizations.is_empty());
        },
    );
}

#[test]
fn standing_base_recovery_decision_requires_routed_owner_and_current_stop() {
    common::isolated(
        "standing_base_recovery_decision_requires_routed_owner_and_current_stop",
        || async {
            let _home = common::home_lock().await;
            let mut f = Fixture::new(false);
            let questions = magi::ask::Questions::at(f.tmp.path().join("questions"));
            let mut q = magi::ask::Question::new(
                f.run.id.clone(),
                "recover-base".into(),
                "owner".into(),
                "Recover bounded base movement?".into(),
                String::new(),
                vec!["recover".into()],
            );
            q.actions.insert(
                "recover".into(),
                magi::ask::ChoiceAction::RecoverBase {
                    run: f.run.id.clone(),
                    stop: f.state().base_sync_recovery.stop.unwrap(),
                    episode: 1,
                },
            );
            q.routed_task = Some("wrong-owner".into());
            questions.put_executable(&mut q).unwrap();
            let response = f
                .control(vec!["decide".into(), q.id.clone(), "recover".into()])
                .await;
            assert_eq!(response["ok"], true, "{response}");
            magi::daemon::apply_choice_actions(&f.queue, &questions, f.tmp.path()).await;
            assert!(f.state().base_sync_recovery.authorizations.is_empty());
            questions
                .update(&q.id, |q| {
                    q.routed_task = Some(f.task.id.clone());
                    Ok(())
                })
                .unwrap();
            // A changed stop never inherits approval from the old decision.
            let mut state = f.state();
            let saved_stop = state.base_sync_recovery.stop.clone();
            state.base_sync_recovery.stop.as_mut().unwrap().at = jiff::Timestamp::now();
            state.save_under(f.tmp.path()).unwrap();
            magi::daemon::apply_choice_actions(&f.queue, &questions, f.tmp.path()).await;
            assert!(f.state().base_sync_recovery.authorizations.is_empty());
            state.base_sync_recovery.stop = saved_stop;
            state.save_under(f.tmp.path()).unwrap();
            magi::daemon::apply_choice_actions(&f.queue, &questions, f.tmp.path()).await;
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
            assert_eq!(f.queue.get(&f.task.id).unwrap().status, TaskStatus::Queued);
            magi::daemon::apply_choice_actions(&f.queue, &questions, f.tmp.path()).await;
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
        },
    );
}

#[test]
fn standing_base_recovery_reuses_only_its_unconsumed_authorization_after_restart() {
    common::isolated(
        "standing_base_recovery_reuses_only_its_unconsumed_authorization_after_restart",
        || async {
            let _home = common::home_lock().await;
            let mut f = Fixture::new(false);
            let obstruction = f.queue.path_of(&f.task.id).with_extension("json.tmp");
            std::fs::create_dir(&obstruction).unwrap();
            assert_eq!(f.recover("pending").await["ok"], false);
            std::fs::remove_dir(obstruction).unwrap();
            let response = f.recover("pending").await;
            assert_eq!(response["ok"], true, "{response}");
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
            assert_eq!(
                f.queue
                    .get(&f.task.id)
                    .unwrap()
                    .base_recovery_requests
                    .len(),
                1
            );
            // A different request cannot borrow this pending approval.
            assert_eq!(f.recover("different").await["ok"], false);
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
        },
    );
}

#[test]
fn standing_base_recovery_stale_owner_decision_and_instruction_cannot_finish_queueing() {
    common::isolated(
        "standing_base_recovery_stale_owner_decision_and_instruction_cannot_finish_queueing",
        || async {
            let _home = common::home_lock().await;
            let mut f = Fixture::new(false);
            let obstruction = f.queue.path_of(&f.task.id).with_extension("json.tmp");
            std::fs::create_dir(&obstruction).unwrap();
            assert_eq!(f.recover("pending").await["ok"], false);
            std::fs::remove_dir(obstruction).unwrap();
            let original_task = f.queue.get(&f.task.id).unwrap();
            let mut changed = original_task.clone();
            changed.instruction = "changed criteria".into();
            f.queue.put(&mut changed).unwrap();
            assert_eq!(f.recover("pending").await["ok"], false);
            changed.instruction = original_task.instruction.clone();
            changed.hold_manual(Some("new decision".into()));
            f.queue.put(&mut changed).unwrap();
            let response = f.recover("pending").await;
            assert_eq!(response["ok"], false, "{response}");
            assert!(response["err"].as_str().unwrap().contains("stale"));
            assert_eq!(f.queue.get(&f.task.id).unwrap().status, TaskStatus::Held);
            assert_eq!(f.state().base_sync_recovery.authorizations.len(), 1);
        },
    );
}
