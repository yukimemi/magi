//! 実 CLI と隔離したストア・偽の forge によるチャット制御の回帰検証。
#[allow(unused_macros, unused_imports)]
mod common;

use std::collections::BTreeMap;
use std::process::Output;

use magi::config::{AgentKind, AgentSpec, Config};
use magi::queue::{CHAT_NODE, Queue, Source, Task, TaskStatus};
use magi::run::{Liveness, RunState, RunStatus};
use magi::talk::{self, Talks};

struct Fixture {
    tmp: tempfile::TempDir,
    talks: Talks,
    talk: String,
    task: Task,
    run: RunState,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let talks = Talks::at(tmp.path().join("talks"));
        let cfg = Config {
            agents: vec![AgentSpec {
                id: "mock".into(),
                kind: AgentKind::Command,
                model: None,
                command: vec!["true".into()],
                extra_args: vec![],
                env: BTreeMap::new(),
                prompt_delivery: None,
                max_turn_secs: None,
            }],
            ..Config::default()
        };
        let t = talk::begin(&talks, &cfg, tmp.path().to_owned(), Some("mock")).unwrap();
        let mut run = RunState::new(
            tmp.path().to_owned(),
            "main".into(),
            "base".into(),
            "old".into(),
            cfg,
        );
        run.begin_pass();
        run.status = RunStatus::Blocked;
        run.driver_exited = true;
        run.save_under(tmp.path()).unwrap();
        let mut task = Task::new(
            "old".into(),
            "old".into(),
            tmp.path().to_owned(),
            Source::Agent {
                run: t.id.clone(),
                node: CHAT_NODE.into(),
            },
        );
        task.start(run.id.clone());
        task.handed_off("stopped");
        Queue::at(tmp.path().join("queue")).put(&mut task).unwrap();
        Self {
            tmp,
            talks,
            talk: t.id,
            task,
            run,
        }
    }

    fn queue(&self) -> Queue {
        Queue::at(self.tmp.path().join("queue"))
    }
    fn saved(&self) -> Task {
        self.queue().get(&self.task.id).unwrap()
    }
    fn cli(&self, args: &[&str], node: &str) -> Output {
        common::magi_cli(self.tmp.path())
            .args(["task"])
            .args(args)
            .env("MAGI_CONFIG_DIR", "")
            .env("MAGI_RUN", &self.talk)
            .env("MAGI_NODE", node)
            .output()
            .unwrap()
    }
    fn control(&self, args: &[&str]) -> Output {
        let mut all = vec!["control"];
        all.extend_from_slice(args);
        self.cli(&all, CHAT_NODE)
    }
}

fn passed(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn refused(out: Output) -> String {
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stderr).unwrap()
}

#[tokio::test]
async fn interrupted_resume_publication_recovers_only_unpublished_requests() {
    use magi::{chat_control::Operation, task_decision::Status, task_inquiry::Outbox};
    let _home = common::home_lock().await;
    // Recreate the durable boundaries left by process loss, then use fresh
    // store handles and real CLI commands to reconcile, retry or cancel.
    for boundary in ["accepted", "started", "uncertain"] {
        for cancel in [false, true] {
            let f = Fixture::new();
            let queue = f.queue();
            let mut task = f.saved();
            task.chat = Some(f.talk.clone());
            queue.put(&mut task).unwrap();
            let outbox = Outbox::at(f.tmp.path());
            let inquiry = outbox
                .file(
                    &task,
                    Some(f.run.id.clone()),
                    "resume".into(),
                    "Resume?",
                    boundary,
                )
                .unwrap();
            outbox.deliver(&queue, &f.talks, &inquiry.id).unwrap();
            let request = serde_json::to_string(&Operation::Resume {
                run: f.run.id.clone(),
            })
            .unwrap();
            outbox
                .update_decision(&inquiry.id, |d| {
                    d.accept(&request)?;
                    if boundary != "accepted" {
                        d.start()?;
                    }
                    if boundary == "uncertain" {
                        d.fail("Interrupted without a receipt")?;
                    }
                    Ok(true)
                })
                .unwrap();
            let before = std::fs::read(queue.path_of(&task.id)).unwrap();
            let claim = queue.claim(&task.id).unwrap();
            magi::chat_control::reconcile_decisions(&queue, f.tmp.path()).unwrap();
            assert!(
                !outbox
                    .get(&inquiry.id)
                    .unwrap()
                    .decision
                    .unwrap()
                    .retry_safe,
                "contention cannot establish absence of publication"
            );
            drop(claim);
            // Both the startup sweep and the explicit supported command can
            // recover this gap, without modifying the queue or starting work.
            if cancel {
                passed(f.control(&["reconcile-decision", &inquiry.id]));
            } else {
                magi::chat_control::reconcile_decisions(
                    &Queue::at(f.tmp.path().join("queue")),
                    f.tmp.path(),
                )
                .unwrap();
            }
            let recovered = Outbox::at(f.tmp.path())
                .get(&inquiry.id)
                .unwrap()
                .decision
                .unwrap();
            assert_eq!(recovered.status, Status::RetryNeeded);
            assert!(recovered.retry_safe);
            assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
            assert!(
                RunState::load_under(&f.run.id, f.tmp.path())
                    .unwrap()
                    .decision_starts
                    .is_empty()
            );
            if cancel {
                passed(f.control(&[
                    "settle-decision",
                    &inquiry.id,
                    "--reason",
                    "Owner cancels the unpublished request",
                ]));
                assert_eq!(
                    outbox.get(&inquiry.id).unwrap().decision.unwrap().status,
                    Status::Resolved
                );
                assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
            } else {
                passed(f.control(&["--decision", &inquiry.id, "resume", &f.run.id]));
                let published = queue.get(&task.id).unwrap();
                assert!(published.action_applied(&inquiry.id));
                assert_eq!(published.resume_override.unwrap().question_id, inquiry.id);
                // Process loss after publication but before the projection's
                // queued write must never enable another publication/cancel.
                outbox.update_decision(&inquiry.id, |d| d.start()).unwrap();
                magi::chat_control::reconcile_decisions(&queue, f.tmp.path()).unwrap();
                let pending = outbox.get(&inquiry.id).unwrap().decision.unwrap();
                assert_eq!(pending.status, Status::InProgress);
                assert!(!pending.retry_safe);
                refused(f.control(&["--decision", &inquiry.id, "resume", &f.run.id]));
                refused(f.control(&[
                    "settle-decision",
                    &inquiry.id,
                    "--reason",
                    "Cannot cancel published work",
                ]));
                // The pin can be consumed before a pass receipt is committed.
                let mut dispatched = queue.get(&task.id).unwrap();
                dispatched.start(f.run.id.clone());
                queue.put(&mut dispatched).unwrap();
                magi::chat_control::reconcile_decisions(&queue, f.tmp.path()).unwrap();
                let pending = outbox.get(&inquiry.id).unwrap().decision.unwrap();
                assert_eq!(pending.status, Status::InProgress);
                assert!(!pending.retry_safe);
            }
        }
    }
}

#[test]
fn repeated_unpublished_resume_can_retry_after_inline_reconciliation() {
    use magi::{chat_control::Operation, task_decision::Status, task_inquiry::Outbox};

    let f = Fixture::new();
    let mut task = f.saved();
    task.chat = Some(f.talk.clone());
    f.queue().put(&mut task).unwrap();
    let outbox = Outbox::at(f.tmp.path());
    let inquiry = outbox
        .file(
            &task,
            Some(f.run.id.clone()),
            "resume".into(),
            "Resume?",
            "",
        )
        .unwrap();
    outbox.deliver(&f.queue(), &f.talks, &inquiry.id).unwrap();
    let request = serde_json::to_string(&Operation::Resume {
        run: f.run.id.clone(),
    })
    .unwrap();
    // Acceptance survived restart, but no queue publication happened.
    outbox
        .update_decision(&inquiry.id, |d| d.accept(&request))
        .unwrap();
    passed(f.control(&["--decision", &inquiry.id, "resume", &f.run.id]));
    let task = f.saved();
    assert!(task.action_applied(&inquiry.id));
    assert_eq!(task.resume_override.unwrap().question_id, inquiry.id);
    let decision = outbox.get(&inquiry.id).unwrap().decision.unwrap();
    assert_eq!(decision.status, Status::Queued);
    assert!(!decision.retry_safe);
    assert!(
        decision
            .history
            .iter()
            .any(|event| event.status == Status::RetryNeeded)
    );
}

#[test]
fn repeated_queued_resume_reconciles_receipts_before_republication() {
    use magi::{task_decision::Status, task_inquiry::Outbox};

    for boundary in ["published", "dispatched", "started", "superseded"] {
        let f = Fixture::new();
        let queue = f.queue();
        let mut task = f.saved();
        task.chat = Some(f.talk.clone());
        queue.put(&mut task).unwrap();
        let outbox = Outbox::at(f.tmp.path());
        let inquiry = outbox
            .file(
                &task,
                Some(f.run.id.clone()),
                "resume".into(),
                "Resume?",
                boundary,
            )
            .unwrap();
        outbox.deliver(&queue, &f.talks, &inquiry.id).unwrap();
        passed(f.control(&["--decision", &inquiry.id, "resume", &f.run.id]));
        assert_eq!(
            outbox.get(&inquiry.id).unwrap().decision.unwrap().status,
            Status::Queued
        );
        let mut task = f.saved();
        let mut run = RunState::load_under(&f.run.id, f.tmp.path()).unwrap();
        if matches!(boundary, "dispatched" | "started") {
            task.start(run.id.clone());
            queue.put(&mut task).unwrap();
            if boundary == "started" {
                run.next_pass_inquiries.push(inquiry.id.clone());
                run.begin_pass();
                run.status = RunStatus::Blocked;
                run.driver_exited = true;
                run.save_under(f.tmp.path()).unwrap();
            }
            // The first execution can return to a resumable state before a
            // reconciliation sweep sees its receipt.
            task.handed_off("blocked again");
        } else if boundary == "superseded" {
            // Replacing a published pin records explicit supersession.
            task.resume_override = None;
            task.handed_off("pin no longer present");
        }
        queue.put(&mut task).unwrap();
        let queue_before = std::fs::read(queue.path_of(&task.id)).unwrap();
        let passes_before = run.passes.len();
        // A contended queue claim must not permit another publication either.
        let claim = queue.claim(&task.id).unwrap();
        refused(f.control(&["--decision", &inquiry.id, "resume", &f.run.id]));
        drop(claim);
        let result = f.control(&["--decision", &inquiry.id, "resume", &f.run.id]);
        if matches!(boundary, "started" | "superseded") {
            assert!(passed(result).contains("already resolved"));
            assert_eq!(
                Outbox::at(f.tmp.path())
                    .get(&inquiry.id)
                    .unwrap()
                    .decision
                    .unwrap()
                    .status,
                Status::Resolved
            );
        } else {
            assert!(refused(result).contains("will not be replayed"));
            assert_eq!(
                outbox.get(&inquiry.id).unwrap().decision.unwrap().status,
                Status::Queued
            );
        }
        assert_eq!(
            std::fs::read(queue.path_of(&task.id)).unwrap(),
            queue_before
        );
        assert_eq!(
            RunState::load_under(&f.run.id, f.tmp.path())
                .unwrap()
                .passes
                .len(),
            passes_before
        );
    }
}

#[cfg(unix)]
#[test]
fn routed_merge_preserves_admission_retries_and_reconciles_refused_receipts() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let mut task = f.saved();
    task.chat = Some(f.talk.clone());
    f.queue().put(&mut task).unwrap();
    let mut run = f.run.clone();
    run.pr = Some(magi::run::PrRecord {
        url: "https://github.com/example/project/pull/1".into(),
        number: 1,
        state: "open".into(),
        checks: "green".into(),
        round: 0,
        rounds: 1,
        red_at_merge: vec![],
    });
    run.save_under(f.tmp.path()).unwrap();
    let outbox = magi::task_inquiry::Outbox::at(f.tmp.path());
    let inquiry = outbox
        .file(&task, Some(run.id), "merge".into(), "Merge PR", "")
        .unwrap();
    outbox.deliver(&f.queue(), &f.talks, &inquiry.id).unwrap();
    let bin = f.tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        r#"#!/bin/sh
if [ "$1 $2" = 'pr merge' ]; then
  echo merge >> sent
  if [ -f refuse ]; then exit 1; fi
  touch merged
elif [ -f merged ]; then
  cat merged.json
else
  cat open.json
fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let head = "a".repeat(40);
    let invoke = |request: &str, sha: &str| {
        common::magi_cli(f.tmp.path())
            .args([
                "task",
                "control",
                "--decision",
                &inquiry.id,
                "merge-pr",
                "example/project",
                "1",
                "--head",
                sha,
                "--request",
                request,
            ])
            .env("MAGI_CONFIG_DIR", "")
            .env("MAGI_RUN", &f.talk)
            .env("MAGI_NODE", CHAT_NODE)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    };
    let awaiting = outbox.get(&inquiry.id).unwrap().decision.unwrap();
    assert!(refused(invoke("merge-once", "abc")).contains("full SHA"));
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap(), awaiting);
    assert!(refused(invoke("bad/id", &head)).contains("invalid request ID"));
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap(), awaiting);
    assert!(!f.tmp.path().join("chat-controls").exists());

    // Admission failures leave the decision entirely unbound. Keep real
    // flock guards in this process while each independent CLI attempts entry.
    for lock_path in [
        f.tmp
            .path()
            .join("chat-merge-locks/github.com_example_project_pull_1.lock"),
        f.tmp
            .path()
            .join("chat-controls")
            .join(&f.talk)
            .join("merge-once.lock"),
    ] {
        use std::os::fd::AsRawFd;
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        refused(invoke("merge-once", &head));
        assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap(), awaiting);
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    }
    let receipt_path = f
        .tmp
        .path()
        .join("chat-controls")
        .join(&f.talk)
        .join("other-request.json");
    let foreign_receipt = serde_json::to_vec(&serde_json::json!({
        "action": {"repo": "example/project", "number": 1, "head": "b".repeat(40), "request": "other-request"},
        "status": "started", "detail": "another operation is uncertain"
    })).unwrap();
    std::fs::write(&receipt_path, &foreign_receipt).unwrap();
    assert!(refused(invoke("other-request", &head)).contains("different operation"));
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap(), awaiting);
    assert_eq!(std::fs::read(&receipt_path).unwrap(), foreign_receipt);

    // A forge read fails before the service writes its started receipt.
    refused(invoke("merge-once", &head));
    let failed = outbox.get(&inquiry.id).unwrap().decision.unwrap();
    assert_eq!(failed.status, magi::task_decision::Status::RetryNeeded);
    assert!(failed.retry_safe);
    assert!(
        !f.tmp
            .path()
            .join("chat-controls")
            .join(&f.talk)
            .join("merge-once.json")
            .exists()
    );
    assert!(!f.tmp.path().join("sent").exists());
    for (file, state) in [("open.json", "OPEN"), ("merged.json", "MERGED")] {
        std::fs::write(
            f.tmp.path().join(file),
            serde_json::to_vec(&serde_json::json!({
                "url": "https://github.com/example/project/pull/1", "state": state,
                "isDraft": false, "mergeable": "MERGEABLE", "headRefOid": head,
                "title": "Example change"
            }))
            .unwrap(),
        )
        .unwrap();
    }
    std::fs::write(f.tmp.path().join("refuse"), "").unwrap();
    passed(invoke("merge-once", &head));
    let refused_decision = outbox.get(&inquiry.id).unwrap().decision.unwrap();
    assert_eq!(
        refused_decision.status,
        magi::task_decision::Status::RetryNeeded
    );
    assert!(!refused_decision.retry_safe);
    // A separate owner operation merged this head. The next invocation may
    // only read its outcome, even though our saved receipt said refused.
    std::fs::write(f.tmp.path().join("merged"), "").unwrap();
    passed(invoke("merge-once", &head));
    passed(invoke("merge-once", &head));
    assert_eq!(
        outbox.get(&inquiry.id).unwrap().decision.unwrap().status,
        magi::task_decision::Status::Resolved
    );
    assert_eq!(
        std::fs::read_to_string(f.tmp.path().join("sent")).unwrap(),
        "merge\n"
    );
}

#[cfg(unix)]
#[test]
fn ordinary_chat_turn_uses_inherited_controls_without_store_grants() {
    common::isolated(
        "ordinary_chat_turn_uses_inherited_controls_without_store_grants",
        || chat_turn_controls(false, false, false),
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires the installed CLI sandbox and permission to launch it"]
fn sandboxed_chat_turn_controls_work_and_refuses_unrelated_writes() {
    common::isolated(
        "sandboxed_chat_turn_controls_work_and_refuses_unrelated_writes",
        || chat_turn_controls(true, false, false),
    );
}

#[cfg(unix)]
#[test]
fn ordinary_chat_survives_unavailable_control_storage() {
    common::isolated("ordinary_chat_survives_unavailable_control_storage", || {
        chat_turn_controls(false, true, false)
    });
}

#[cfg(unix)]
#[test]
fn ordinary_chat_survives_control_channel_setup_failure() {
    common::isolated(
        "ordinary_chat_survives_control_channel_setup_failure",
        || chat_turn_controls(false, true, true),
    );
}

#[cfg(unix)]
async fn chat_turn_controls(sandbox: bool, broken_store: bool, missing_channel: bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let _home = common::home_lock().await;
    let f = Fixture::new();
    magi::run::set_home(if missing_channel {
        f.tmp.path().join("unavailable-home")
    } else {
        f.tmp.path().to_owned()
    });
    let repo = f.tmp.path().join("repository");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("protected"), "repository evidence").unwrap();
    let other = f.tmp.path().join("runs/unrelated");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("protected"), "other run").unwrap();
    let artifacts = f.talks.artifacts_of(&f.talk);
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(artifacts.join("protected"), "earlier attachment").unwrap();
    let questions = magi::ask::Questions::at(f.tmp.path().join("questions"));
    let mut question = magi::ask::Question::new(
        f.run.id.clone(),
        "implement".into(),
        "worker".into(),
        "Waiting?".into(),
        String::new(),
        vec!["continue".into()],
    );
    questions.put(&mut question).unwrap();
    let foreign_talk = talk::begin(&f.talks, &f.run.config, repo.clone(), Some("mock")).unwrap();
    let mut foreign = Task::new(
        "foreign".into(),
        "foreign".into(),
        repo.clone(),
        Source::Agent {
            run: foreign_talk.id.clone(),
            node: CHAT_NODE.into(),
        },
    );
    f.queue().put(&mut foreign).unwrap();
    if broken_store {
        std::fs::write(f.tmp.path().join("choice-actions"), "unwritable directory").unwrap();
    }
    let mut t = f.talks.get(&f.talk).unwrap();
    t.repo = repo.clone();
    f.talks.put(&mut t).unwrap();
    let words = "please clean up the stopped run";
    let mut cfg = f.run.config.clone();
    assert!(!cfg.talk.allow_write);
    let bin = f.tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let real_cli = magi::config::find_program("codex");
    if sandbox {
        assert!(real_cli.is_some(), "installed CLI sandbox required");
    }
    let script = bin.join("codex");
    // Exercise talk::turn and agent::invoke, including the actual cwd and
    // generated flags. The host regression validates the launch contract;
    // the explicit sandbox regression also proves filesystem enforcement.
    std::fs::write(&script, r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
sys.stdin.read()
args = sys.argv[1:]
cwd = args[args.index('-C') + 1]
assert pathlib.Path(cwd).resolve() == pathlib.Path.cwd().resolve()
assert pathlib.Path(cwd).resolve() == pathlib.Path(os.environ['REPO']).resolve()
assert args[args.index('--sandbox') + 1] == 'read-only'
assert not any('bypass' in a for a in args)
assert os.environ['MAGI_ALLOW_WRITE'] == '0'
settings = [args[i+1] for i, a in enumerate(args) if a == '-c']
assert 'approval_policy="never"' in settings
assert not any('sandbox_workspace_write' in s for s in settings)
fds = tuple(int(os.environ[n]) for n in ['MAGI_CHAT_CONTROL_FD', 'MAGI_CHAT_CONTROL_RESPONSE_FD', 'MAGI_CHAT_CONTROL_LOCK_FD']) if 'MAGI_CHAT_CONTROL_FD' in os.environ else ()
assert bool(fds) == (os.environ['TEST_NO_CHANNEL'] == '0')
payload = r'''
import os, pathlib, subprocess
binary = os.environ['MAGI_TEST_BINARY']
task, run = os.environ['TEST_TASK'], os.environ['TEST_RUN']
fds = tuple(int(os.environ[n]) for n in ['MAGI_CHAT_CONTROL_FD', 'MAGI_CHAT_CONTROL_RESPONSE_FD', 'MAGI_CHAT_CONTROL_LOCK_FD'])
def control(*args):
    p = subprocess.run([binary, 'task', *args], capture_output=True, text=True, pass_fds=fds)
    assert p.returncode == 0, (args, p.stdout, p.stderr)
    return p.stdout
# The server binds the capability to this talk even if the child spoofs env.
# Another chat's task is the owner's work too: holding it goes through.
other = subprocess.run([binary, 'task', 'control', 'hold', os.environ['FOREIGN_TASK']],
                       capture_output=True, text=True, pass_fds=fds, env={**os.environ, 'MAGI_RUN':os.environ['FOREIGN_TALK']})
assert other.returncode == 0, other.stderr
# Both planning spellings cross the bridge and preserve the unanswered-decision guard.
for prefix in [[], ['control']]:
    planned = subprocess.run([binary, 'task', *prefix, 'reconcile-retention', task,
                              '--authority-run', run], capture_output=True, text=True, pass_fds=fds)
    assert planned.returncode != 0
    assert 'unanswered owner decision' in planned.stderr, planned.stderr
# Kill requesters only after a complete write and before any response read.
# The stopped child holds the process lock; waitpid confirms the exact point.
import signal
orphan = r"""
import fcntl, json, os, signal, struct
request, response, lock = (int(os.environ[n]) for n in ['MAGI_CHAT_CONTROL_FD', 'MAGI_CHAT_CONTROL_RESPONSE_FD', 'MAGI_CHAT_CONTROL_LOCK_FD'])
fcntl.lockf(lock, fcntl.LOCK_EX)
operation = {'operation':'Hold', 'task':os.environ['TEST_TASK'], 'reason':'orphan'}
if os.environ['ORPHAN_ID'] == '2':
    operation['task'] = 'no-such-task'
body = json.dumps({'id':int(os.environ['ORPHAN_ID']), 'operation':operation}).encode()
frame = struct.pack('>I', len(body)) + body
while frame:
    frame = frame[os.write(request, frame):]
os.kill(os.getpid(), signal.SIGSTOP)
raise AssertionError('requester must be killed before reading')
"""
for orphan_id in ['1', '2']:
    child = subprocess.Popen([os.environ.get('PYTHON', 'python3'), '-c', orphan], pass_fds=fds,
                             env={**os.environ, 'ORPHAN_ID':orphan_id})
    pid, status = os.waitpid(child.pid, os.WUNTRACED)
    assert pid == child.pid and os.WIFSTOPPED(status), status
    child.kill()
    assert child.wait() == -signal.SIGKILL
assert 'held' in control('control', 'hold', task, 'after orphan')
import json
saved = json.loads((pathlib.Path(os.environ['MAGI_HOME'])/'queue'/(task+'.json')).read_text())
assert saved['hold_reason'] == 'after orphan', saved
assert 'released' in control('control', 'release', task)
# Preparation may fall back only with local write permission. Exercise failure
# at each duplicate, including cleanup of the preceding successful copies.
if os.environ['TEST_SANDBOX'] == '0':
    import json
    for name in ['MAGI_CHAT_CONTROL_LOCK_FD', 'MAGI_CHAT_CONTROL_FD', 'MAGI_CHAT_CONTROL_RESPONSE_FD']:
        for allowed in ['0', '1']:
            p = subprocess.run([binary, 'task', 'control', 'hold', task, 'local fallback'],
                               capture_output=True, text=True, pass_fds=fds,
                               env={**os.environ, name:'999999', 'MAGI_CHAT_CONTROL_SOCKET':'/nonexistent/magi-control', 'MAGI_ALLOW_WRITE':allowed})
            assert (p.returncode == 0) == (allowed == '1'), (p.stdout, p.stderr)
    # The local fallback still authorizes the standing chat it speaks for.
    denied = subprocess.run([binary, 'task', 'control', 'hold', task],
                            capture_output=True, text=True, pass_fds=fds,
                            env={**os.environ, 'MAGI_RUN':'no-such-talk', 'MAGI_CHAT_CONTROL_FD':'999999', 'MAGI_ALLOW_WRITE':'1'})
    assert denied.returncode != 0, denied.stdout
    # A reused descriptor must fail validation before any request is sent.
    read, write = os.pipe()
    os.close(write)
    p = subprocess.run([binary, 'task', 'control', 'hold', task, 'must not execute locally'],
                       capture_output=True, text=True, pass_fds=(*fds, read),
                       env={**os.environ, 'MAGI_CHAT_CONTROL_RESPONSE_FD':str(read), 'MAGI_CHAT_CONTROL_SOCKET':'/nonexistent/magi-control', 'MAGI_ALLOW_WRITE':'0'})
    os.close(read)
    assert p.returncode != 0 and 'identity mismatch' in p.stderr, p.stderr
    # A rejected descriptor does not disturb subsequent valid requests.
    control('control', 'hold', task, 'after disconnect')
    saved = json.loads((pathlib.Path(os.environ['MAGI_HOME'])/'queue'/(task+'.json')).read_text())
    assert saved['hold_reason'] == 'after disconnect', saved
# Serialize concurrent processes and a frame larger than PIPE_BUF.
jobs = [subprocess.Popen([binary, 'task', 'control', 'hold', task, str(i)],
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, pass_fds=fds) for i in range(3)]
for job in jobs:
    out, err = job.communicate()
    assert job.returncode == 0, (out, err)
control('control', 'hold', task, 'wait')
control('control', 'release', task)
control('control', 'retry', task, 'revised instruction'+' '*8192)
control('control', 'resume', run)
control('control', 'rerun-checks', run, '--request', 'sandbox-check', '--head', 'current', '--workflow', '42')
control('control', 'abandon', run)
if os.environ['TEST_SANDBOX'] == '1':
    home = pathlib.Path(os.environ['MAGI_HOME'])
    for p in [pathlib.Path(os.environ['REPO'])/'protected', home/'runs/unrelated/protected',
              home/'talks'/(os.environ['MAGI_RUN']+'.json'), home/'unrelated',
              home/'talks'/(os.environ['MAGI_RUN']+'.artifacts')/'protected',
              home/'queue'/(task+'.json'), home/'questions'/(os.environ['TEST_QUESTION']+'.json'),
              home/'choice-actions/reruns.json', home/'runs'/run/'run.json']:
        try:
            p.write_text('forbidden')
        except PermissionError:
            pass
        else:
            raise AssertionError('unrelated write permitted: '+str(p))
'''
command = [sys.executable, '-c', payload]
if os.environ['TEST_SANDBOX'] == '1':
    command = [os.environ['REAL_CLI'], 'sandbox', '-c', 'sandbox_mode="read-only"',
               *[a for setting in settings for a in ['-c', setting]], *command]
p = subprocess.run(command, capture_output=True, text=True, pass_fds=fds) if os.environ['TEST_PASSIVE'] == '0' else subprocess.CompletedProcess(command, 0, '', '')
assert p.returncode == 0, (p.stdout, p.stderr)
print(json.dumps({'type':'thread.started', 'thread_id':'controls-session'}))
print(json.dumps({'type':'item.completed', 'item':{'type':'agent_message','text':'Controls completed.'}}))
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, r#"#!/bin/sh
if [ "$1" = pr ]; then
  printf '%s\n' '{"url":"https://github.com/example/project/pull/1","number":1,"state":"OPEN","headRefOid":"current","statusCheckRollup":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://github.com/example/project/actions/runs/42/job/1"}]}'
fi
"#).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut run = f.run.clone();
    run.repo = repo.clone();
    run.pr = Some(magi::run::PrRecord {
        url: "https://github.com/example/project/pull/1".into(),
        number: 1,
        state: "open".into(),
        checks: "red".into(),
        round: 0,
        rounds: 1,
        red_at_merge: vec![],
    });
    run.save_under(f.tmp.path()).unwrap();
    let cli_path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    // SAFETY: common::isolated runs this test in a dedicated child process;
    // no agent or blocking task exists yet.
    unsafe { std::env::set_var("PATH", &cli_path) };
    cfg.agents[0].kind = AgentKind::Codex;
    cfg.agents[0].command.clear();
    cfg.agents[0].env = BTreeMap::from([
        ("PATH".into(), cli_path),
        ("MAGI_HOME".into(), f.tmp.path().to_str().unwrap().into()),
        ("MAGI_CONFIG_DIR".into(), String::new()),
        ("TEST_TASK".into(), f.task.id.clone()),
        ("FOREIGN_TASK".into(), foreign.id.clone()),
        ("FOREIGN_TALK".into(), foreign_talk.id.clone()),
        (
            "TEST_NO_CHANNEL".into(),
            if missing_channel { "1" } else { "0" }.into(),
        ),
        ("TEST_QUESTION".into(), question.id.clone()),
        (
            "TEST_PASSIVE".into(),
            if broken_store { "1" } else { "0" }.into(),
        ),
        ("TEST_RUN".into(), f.run.id.clone()),
        (
            "TEST_SANDBOX".into(),
            if sandbox { "1" } else { "0" }.into(),
        ),
        (
            "REAL_CLI".into(),
            real_cli.unwrap_or_default().to_string_lossy().into(),
        ),
        ("REPO".into(), repo.to_str().unwrap().into()),
    ]);
    cfg.agents[0]
        .env
        .extend(common::magi_cli_env(f.tmp.path(), "MAGI_TEST_BINARY"));
    let lease = f.talks.claim_turn(&f.talk).unwrap().unwrap();
    talk::say(&lease, &mut t, &f.talks, &cfg, words, vec![])
        .await
        .unwrap();
    if broken_store {
        assert_eq!(f.saved().instruction, "old");
        return;
    }
    cfg.agents[0].env.insert("TEST_PASSIVE".into(), "1".into());
    talk::say(&lease, &mut t, &f.talks, &cfg, "show status", vec![])
        .await
        .unwrap();
    assert_eq!(f.saved().instruction.trim_end(), "revised instruction");
    assert_eq!(f.saved().status, TaskStatus::Queued);
    assert_eq!(
        f.queue().get(&foreign.id).unwrap().status,
        TaskStatus::Held,
        "another chat's task is controlled on the owner's request"
    );
    assert_eq!(
        RunState::load_under(&f.run.id, f.tmp.path())
            .unwrap()
            .status,
        RunStatus::Failed
    );
    assert!(
        f.tmp
            .path()
            .join("chat-controls")
            .join(&f.talk)
            .join("sandbox-check.json")
            .exists()
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("protected")).unwrap(),
        "repository evidence"
    );
    assert_eq!(
        std::fs::read_to_string(other.join("protected")).unwrap(),
        "other run"
    );
    // The question was routed to the task chat at filing time and not yet
    // delivered, so settling its run keeps it as a pending message instead of
    // abandoning it (see `Questions::settle_run`).
    let left = questions.get(&question.id).unwrap();
    assert!(left.routed_task.is_some() && left.routed_chat.is_none());
    assert_eq!(left.status, magi::ask::QuestionStatus::Open);
    assert_eq!(
        std::fs::read_to_string(artifacts.join("protected")).unwrap(),
        "earlier attachment"
    );
}

#[tokio::test]
async fn resume_is_pinned_idempotent_and_does_not_start_a_driver() {
    let _home = common::home_lock().await;
    let f = Fixture::new();
    let before = std::fs::read(f.tmp.path().join("runs").join(&f.run.id).join("run.json")).unwrap();
    assert!(passed(f.control(&["resume", &f.run.id])).contains("execution lease"));
    let first = f.saved();
    passed(f.control(&["resume", &f.run.id]));
    let second = f.saved();
    assert_eq!(first.resumption, second.resumption);
    assert_eq!(second.status, TaskStatus::Queued);
    assert_eq!(
        second
            .resume_override
            .as_ref()
            .unwrap()
            .pinned_run
            .as_deref(),
        Some(f.run.id.as_str())
    );
    assert_eq!(first.runs, second.runs);
    assert_eq!(first.resumption_history, second.resumption_history);
    assert_eq!(first.resume_override, second.resume_override);
    assert!(
        second
            .resume_override
            .as_ref()
            .unwrap()
            .question_id
            .is_empty()
    );
    assert!(second.resumption.as_ref().unwrap().question_id.is_none());
    assert_eq!(
        before,
        std::fs::read(f.tmp.path().join("runs").join(&f.run.id).join("run.json")).unwrap()
    );
    passed(f.control(&["hold", &f.task.id, "manual decision"]));
    // An explicit resume is the owner's request: it releases their own hold.
    passed(f.control(&["resume", &f.run.id]));
    let resumed = f.saved();
    assert!(!resumed.operator_held());
    assert_eq!(resumed.status, TaskStatus::Queued);
    let _claim = f.queue().claim(&f.task.id).unwrap();
    refused(f.control(&["hold", &f.task.id, "busy"]));
}

#[tokio::test]
async fn abandon_needs_no_phrase_but_only_a_proven_dead_driver() {
    let _home = common::home_lock().await;
    let mut f = Fixture::new();
    let run_id = f.run.id.clone();
    let args = ["control", "abandon", &run_id];
    f.run.driver_exited = false;
    f.run.save_under(f.tmp.path()).unwrap();
    assert_eq!(f.run.liveness_under(f.tmp.path(), false), Liveness::Unknown);
    assert!(refused(f.cli(&args, CHAT_NODE)).contains("liveness is not proven gone"));
    f.run.driver_pid = Some(std::process::id());
    f.run.driver_started_at = magi::proc::process_started_at(std::process::id());
    f.run.save_under(f.tmp.path()).unwrap();
    assert_eq!(f.run.liveness_under(f.tmp.path(), false), Liveness::Live);
    assert!(refused(f.cli(&args, CHAT_NODE)).contains("liveness is not proven gone"));
    f.run.driver_pid = None;
    f.run.driver_started_at = None;
    f.run.driver_exited = true;
    f.run.save_under(f.tmp.path()).unwrap();
    assert_eq!(f.run.liveness_under(f.tmp.path(), false), Liveness::Dead);
    // No owner phrase or quote: the chat's request is enough once the driver
    // is proven gone.
    passed(f.cli(&args, CHAT_NODE));
    assert_eq!(
        RunState::load_under(&f.run.id, f.tmp.path())
            .unwrap()
            .status,
        RunStatus::Failed
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rerun_checks_binds_head_request_and_workflow_and_never_retries_uncertainty() {
    use std::os::unix::fs::PermissionsExt;
    let _home = common::home_lock().await;
    let mut f = Fixture::new();
    f.run.pr = Some(magi::run::PrRecord {
        url: "https://github.com/example/project/pull/1".into(),
        number: 1,
        state: "open".into(),
        checks: "red".into(),
        round: 0,
        rounds: 1,
        red_at_merge: vec![],
    });
    f.run.save_under(f.tmp.path()).unwrap();
    let bin = f.tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        r#"#!/bin/sh
case "$1 $2" in
  'pr view') cat "$MAGI_HOME/snapshot.json" ;;
  'run rerun')
    echo "$3" >> "$MAGI_HOME/sent"
    if [ -f "$MAGI_HOME/fail" ]; then exit 1; fi ;;
  *) echo "unexpected gh command: $*" >&2; exit 64 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        f.tmp.path().join("snapshot.json"),
        r#"{
        "url":"https://github.com/example/project/pull/1", "number":1,
        "state":"OPEN", "headRefOid":"current",
        "statusCheckRollup":[{"__typename":"CheckRun", "name":"ci",
        "status":"COMPLETED", "conclusion":"FAILURE",
        "detailsUrl":"https://github.com/example/project/actions/runs/42/job/1"}]
    }"#,
    )
    .unwrap();
    let invoke = |request: &str, head: &str, workflow: &str| {
        common::magi_cli(f.tmp.path())
            .args([
                "task",
                "control",
                "rerun-checks",
                &f.run.id,
                "--request",
                request,
                "--head",
                head,
                "--workflow",
                workflow,
            ])
            .env("MAGI_CONFIG_DIR", "")
            .env("MAGI_RUN", &f.talk)
            .env("MAGI_NODE", CHAT_NODE)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    };
    assert!(refused(invoke("changed", "old", "42")).contains("head or lifecycle changed"));
    assert!(
        refused(invoke("wrong-workflow", "current", "43")).contains("workflow is no longer failed")
    );
    assert!(!f.tmp.path().join("sent").exists());
    passed(invoke("once", "current", "42"));
    passed(invoke("once", "current", "42"));
    refused(invoke("once", "other", "42"));
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            f.tmp
                .path()
                .join("chat-controls")
                .join(&f.talk)
                .join("once.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["status"], "applied");
    assert_eq!(receipt["action"]["head"], "current");
    let ledger: Vec<String> = serde_json::from_slice(
        &std::fs::read(f.tmp.path().join("choice-actions/reruns.json")).unwrap(),
    )
    .unwrap();
    let entry: serde_json::Value = serde_json::from_str(&ledger[0]).unwrap();
    assert_eq!(entry[0], format!("chat:{}:once", f.talk));
    assert_eq!(
        std::fs::read_to_string(f.tmp.path().join("sent")).unwrap(),
        "42\n"
    );
    std::fs::write(f.tmp.path().join("fail"), "").unwrap();
    refused(invoke("uncertain", "current", "42"));
    std::fs::remove_file(f.tmp.path().join("fail")).unwrap();
    assert!(refused(invoke("uncertain", "current", "42")).contains("uncertain result"));
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            f.tmp
                .path()
                .join("chat-controls")
                .join(&f.talk)
                .join("uncertain.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["status"], "started");
    assert_eq!(
        std::fs::read_to_string(f.tmp.path().join("sent")).unwrap(),
        "42\n42\n"
    );

    // The same routed decision remains retryable when the forge read fails
    // before the started callback. Exercise CLI persistence across processes.
    let mut task = f.saved();
    task.chat = Some(f.talk.clone());
    f.queue().put(&mut task).unwrap();
    let outbox = magi::task_inquiry::Outbox::at(f.tmp.path());
    let inquiry = outbox
        .file(
            &task,
            Some(f.run.id.clone()),
            "checks".into(),
            "Retry checks",
            "",
        )
        .unwrap();
    outbox.deliver(&f.queue(), &f.talks, &inquiry.id).unwrap();
    let routed_for = |decision: &str, request: &str, head: &str, workflow: &str| {
        common::magi_cli(f.tmp.path())
            .args([
                "task",
                "control",
                "--decision",
                decision,
                "rerun-checks",
                &f.run.id,
                "--request",
                request,
                "--head",
                head,
                "--workflow",
                workflow,
            ])
            .env("MAGI_CONFIG_DIR", "")
            .env("MAGI_RUN", &f.talk)
            .env("MAGI_NODE", CHAT_NODE)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    };
    let routed = |request: &str| routed_for(&inquiry.id, request, "current", "42");
    let awaiting = outbox.get(&inquiry.id).unwrap().decision.unwrap();
    assert!(refused(routed("bad/id")).contains("safe filename"));
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap(), awaiting);
    // A missing PR is also a correctable pre-acceptance read failure.
    let mut without_pr = RunState::load_under(&f.run.id, f.tmp.path()).unwrap();
    let recorded_pr = without_pr.pr.take();
    without_pr.save_under(f.tmp.path()).unwrap();
    assert!(refused(routed("preflight")).contains("no pull request"));
    assert_eq!(outbox.get(&inquiry.id).unwrap().decision.unwrap(), awaiting);
    without_pr.pr = recorded_pr;
    without_pr.save_under(f.tmp.path()).unwrap();
    let snapshot = std::fs::read(f.tmp.path().join("snapshot.json")).unwrap();
    std::fs::remove_file(f.tmp.path().join("snapshot.json")).unwrap();
    refused(routed("preflight"));
    let decision = outbox.get(&inquiry.id).unwrap().decision.unwrap();
    assert_eq!(decision.status, magi::task_decision::Status::RetryNeeded);
    assert!(decision.retry_safe);
    assert!(
        !f.tmp
            .path()
            .join("chat-controls")
            .join(&f.talk)
            .join("preflight.json")
            .exists()
    );
    assert_eq!(
        std::fs::read_to_string(f.tmp.path().join("sent")).unwrap(),
        "42\n42\n"
    );
    std::fs::write(f.tmp.path().join("snapshot.json"), snapshot).unwrap();
    passed(routed("preflight"));
    passed(routed("preflight"));
    assert_eq!(
        outbox.get(&inquiry.id).unwrap().decision.unwrap().status,
        magi::task_decision::Status::Resolved
    );
    assert_eq!(
        std::fs::read_to_string(f.tmp.path().join("sent")).unwrap(),
        "42\n42\n42\n"
    );

    let file_decision = |source: &str| {
        let record = outbox
            .file(
                &task,
                Some(f.run.id.clone()),
                source.into(),
                "Retry checks",
                "",
            )
            .unwrap();
        outbox.deliver(&f.queue(), &f.talks, &record.id).unwrap();
        record.id
    };
    let controls = f.tmp.path().join("chat-controls").join(&f.talk);
    let busy = file_decision("rerun-contention");
    let lock = std::fs::File::create(controls.join("busy.lock")).unwrap();
    use std::os::fd::AsRawFd;
    // Hold the production receipt lock while a separate CLI process attempts
    // admission. No forge invocation or started receipt may result.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    for _ in 0..2 {
        refused(routed_for(&busy, "busy", "current", "42"));
        let decision = outbox.get(&busy).unwrap().decision.unwrap();
        assert_eq!(decision.status, magi::task_decision::Status::RetryNeeded);
        assert!(decision.retry_safe);
        assert!(!controls.join("busy.json").exists());
        assert_eq!(
            std::fs::read_to_string(f.tmp.path().join("sent")).unwrap(),
            "42\n42\n42\n"
        );
    }
    drop(lock);
    passed(routed_for(&busy, "busy", "current", "42"));
    assert_eq!(
        outbox.get(&busy).unwrap().decision.unwrap().status,
        magi::task_decision::Status::Resolved
    );
    let sent = std::fs::read(f.tmp.path().join("sent")).unwrap();
    for (index, request, head, workflow) in [
        (0, "once", "other", "42"),
        (1, "once", "current", "43"),
        (2, "uncertain", "other", "42"),
    ] {
        let collision = file_decision(&format!("rerun-collision-{index}"));
        let receipt = std::fs::read(controls.join(format!("{request}.json"))).unwrap();
        for _ in 0..2 {
            assert!(
                refused(routed_for(&collision, request, head, workflow))
                    .contains("different operation")
            );
            let decision = outbox.get(&collision).unwrap().decision.unwrap();
            assert_eq!(decision.status, magi::task_decision::Status::RetryNeeded);
            assert!(decision.retry_safe);
            magi::chat_control::reconcile_decisions(&f.queue(), f.tmp.path()).unwrap();
            assert!(outbox.get(&collision).unwrap().decision.unwrap().retry_safe);
        }
        passed(f.control(&[
            "settle-decision",
            &collision,
            "--reason",
            "Owner cancels the rejected request",
        ]));
        assert_eq!(
            outbox.get(&collision).unwrap().decision.unwrap().status,
            magi::task_decision::Status::Resolved
        );
        assert_eq!(
            std::fs::read(controls.join(format!("{request}.json"))).unwrap(),
            receipt
        );
        assert_eq!(std::fs::read(f.tmp.path().join("sent")).unwrap(), sent);
    }
    // The identical operation's started receipt remains uncertain, unlike a
    // collision with an unrelated operation. It cannot be replayed or settled.
    let uncertain = file_decision("rerun-identical-uncertainty");
    let receipt = std::fs::read(controls.join("uncertain.json")).unwrap();
    assert!(
        refused(routed_for(&uncertain, "uncertain", "current", "42")).contains("uncertain result")
    );
    let decision = outbox.get(&uncertain).unwrap().decision.unwrap();
    assert_eq!(decision.status, magi::task_decision::Status::RetryNeeded);
    assert!(!decision.retry_safe);
    magi::chat_control::reconcile_decisions(&f.queue(), f.tmp.path()).unwrap();
    refused(routed_for(&uncertain, "uncertain", "current", "42"));
    refused(f.control(&[
        "settle-decision",
        &uncertain,
        "--reason",
        "Cannot erase an uncertain effect",
    ]));
    assert_eq!(
        std::fs::read(controls.join("uncertain.json")).unwrap(),
        receipt
    );
    assert_eq!(std::fs::read(f.tmp.path().join("sent")).unwrap(), sent);
}

#[tokio::test]
async fn typed_question_resume_still_uses_shared_queue_transition_once() {
    let _home = common::home_lock().await;
    let f = Fixture::new();
    let questions = magi::ask::Questions::at(f.tmp.path().join("questions"));
    let mut q = magi::ask::Question::new(
        f.run.id.clone(),
        "maintenance".into(),
        "owner".into(),
        "Continue this run?".into(),
        String::new(),
        vec!["continue".into()],
    );
    q.actions.insert(
        "continue".into(),
        magi::ask::ChoiceAction::ResumeRun {
            run: f.run.id.clone(),
        },
    );
    q.answer(magi::ask::Answer::Choice("continue".into()))
        .unwrap();
    questions.put(&mut q).unwrap();
    magi::daemon::apply_choice_actions(&f.queue(), &questions, f.tmp.path()).await;
    let mut after = f.saved();
    assert_eq!(after.status, TaskStatus::Queued);
    assert_eq!(after.resume_override.as_ref().unwrap().question_id, q.id);
    assert!(after.action_applied(&q.id));
    after.hold_manual(Some("later decision".into()));
    f.queue().put(&mut after).unwrap();
    magi::daemon::apply_choice_actions(&f.queue(), &questions, f.tmp.path()).await;
    assert!(f.saved().operator_held());
}

#[cfg(any(unix, windows))]
#[test]
fn chat_controls_through_real_cli_child() {
    common::isolated("chat_controls_through_real_cli_child", || async {
        let f = Fixture::new();
        magi::run::set_home(f.tmp.path().to_owned());
        let artifacts = f.talks.artifacts_of(&f.talk);
        let mut spec = f.run.config.agents[0].clone();
        spec.env
            .extend(common::magi_cli_env(f.tmp.path(), "MAGI_TEST_BINARY"));
        // The launcher must override a stale per-agent store path, even for
        // read-only turns: CLI authorization reads the same talk as the bridge.
        spec.env.insert(
            "MAGI_HOME".into(),
            f.tmp
                .path()
                .join("wrong-home")
                .to_string_lossy()
                .into_owned(),
        );
        spec.command = vec![
            common::magi_cli(f.tmp.path())
                .get_program()
                .to_str()
                .unwrap()
                .into(),
            "task".into(),
            "control".into(),
            "hold".into(),
            f.task.id.clone(),
            "via launcher".into(),
        ];
        let mut seat = magi::agent::SeatState::new("chat", "mock", 1);
        let output = magi::agent::invoke(
            &spec,
            &mut seat,
            &magi::agent::Invocation {
                coordination: None,
                fetched_base: None,
                cwd: f.tmp.path(),
                prompt: "",
                timeout: std::time::Duration::from_secs(30),
                allow_write: false,
                sessions: false,
                artifacts: &artifacts,
                stem: "pipe",
                run: &f.talk,
                node: CHAT_NODE,
                cache_dir: None,
                attachments: &[],
                writable: &[],
            },
        )
        .await
        .unwrap();
        assert!(output.usable(), "{output:?}");
        assert_eq!(f.saved().status, TaskStatus::Held);
        assert_eq!(f.saved().hold_reason.as_deref(), Some("via launcher"));
    });
}

/// A read-only chat seat files a follow-up and answers through the launcher.
/// The CLI reads a decoy home with read-only queue and question stores; filing,
/// attachment copying and the obsolete approval's dismissal must reach the
/// launcher's stores through the inherited bridge.
#[cfg(unix)]
#[test]
fn chat_commands_over_the_bridge_settle_native_inquiries_and_dismiss_approvals() {
    common::isolated(
        "chat_commands_over_the_bridge_settle_native_inquiries_and_dismiss_approvals",
        || async {
            use std::os::unix::fs::PermissionsExt as _;
            let f = Fixture::new();
            let home = f.tmp.path();
            magi::run::set_home(home.to_owned());
            let questions = magi::ask::Questions::at(home.join("questions"));
            // Task inquiries use native chat commands even with the retired opt-out.
            let repo = home.join("repository");
            std::fs::create_dir(&repo).unwrap();
            assert!(
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .arg(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
            let mut repo_cfg = Config::default();
            repo_cfg.graph.question_routing = false;
            std::fs::write(repo.join("magi.toml"), toml::to_string(&repo_cfg).unwrap()).unwrap();
            let mut state = RunState::new(
                repo.clone(),
                "main".into(),
                "head".into(),
                "Original task".into(),
                f.run.config.clone(),
            );
            let mut approval = magi::ask::Question::new(
                state.id.clone(),
                magi::land::APPROVAL_NODE.into(),
                "land".into(),
                "Merge?".into(),
                String::new(),
                vec![
                    "merge".into(),
                    "hold".into(),
                    magi::merge_decision::DISMISS.into(),
                ],
            );
            state.status = RunStatus::Landing;
            state.parked = true;
            state.driver_exited = true;
            state.land_approval = Some(magi::run::LandApproval {
                question: approval.id.clone(),
                head: "head".into(),
            });
            state.pr = Some(magi::run::PrRecord {
                url: "https://example.test/pull/1".into(),
                number: 1,
                state: "open".into(),
                checks: "green".into(),
                round: 1,
                rounds: 2,
                red_at_merge: vec![],
            });
            state.save_under(home).unwrap();
            approval.merge_target = Some(magi::merge_decision::Target {
                pr: "https://example.test/pull/1".into(),
                head: "head".into(),
                language: "en".into(),
            });
            let mut owner = Task::new(
                "Original task".into(),
                "Original instruction".into(),
                repo.clone(),
                Source::Human,
            );
            owner.runs.push(state.id.clone());
            owner.chat = Some(f.talk.clone());
            owner.status = TaskStatus::Parked;
            f.queue().put(&mut owner).unwrap();
            questions.put(&mut approval).unwrap();
            let mut free = magi::ask::Question::new(
                state.id.clone(),
                "implement".into(),
                "impl-A".into(),
                "Which file?".into(),
                String::new(),
                Vec::new(),
            );
            questions.put(&mut free).unwrap();
            assert_eq!(approval.routed_task.as_deref(), Some(owner.id.as_str()));
            assert_eq!(free.routed_task.as_deref(), Some(owner.id.as_str()));
            magi::task_chat::sweep(&f.queue(), &f.talks, &questions, home);

            // The decoy home holds copies of what the CLI reads; its question
            // store is read-only, so a direct answer could not even lock it.
            let decoy = home.join("decoy");
            let decoy_questions = magi::ask::Questions::at(decoy.join("questions"));
            for q in [&approval, &free] {
                decoy_questions
                    .put(&mut questions.get(&q.id).unwrap())
                    .unwrap();
            }
            std::fs::create_dir_all(decoy.join("talks")).unwrap();
            std::fs::copy(
                f.talks.path_of(&f.talk),
                Talks::at(decoy.join("talks")).path_of(&f.talk),
            )
            .unwrap();
            let decoy_before: std::collections::BTreeMap<_, _> = [&free, &approval]
                .into_iter()
                .map(|q| {
                    (
                        q.id.clone(),
                        std::fs::read(decoy_questions.path_of(&q.id)).unwrap(),
                    )
                })
                .collect();
            let readonly = |mode| {
                std::fs::set_permissions(
                    decoy.join("questions"),
                    std::fs::Permissions::from_mode(mode),
                )
                .unwrap();
            };
            std::fs::create_dir_all(decoy.join("queue")).unwrap();
            std::fs::set_permissions(decoy.join("queue"), std::fs::Permissions::from_mode(0o555))
                .unwrap();
            readonly(0o555);

            // Completing a task after a successful retry must also retire the
            // prior blocked run, a write the read-only seat cannot perform.
            let mut successful = RunState::new(
                home.to_owned(),
                "main".into(),
                "head".into(),
                "Retry".into(),
                f.run.config.clone(),
            );
            successful.status = RunStatus::Ready;
            successful.driver_exited = true;
            successful.save_under(home).unwrap();
            let mut completed = f.saved();
            completed.runs.push(successful.id.clone());
            f.queue().put(&mut completed).unwrap();
            let attachment = home.join("follow-up.txt");
            std::fs::write(&attachment, "Owner's context").unwrap();
            let artifacts = f.talks.artifacts_of(&f.talk);
            let mut spec = f.run.config.agents[0].clone();
            spec.env.extend(common::magi_cli_env(home, "ANSWER_BIN"));
            spec.env
                .insert("DECOY".into(), decoy.to_string_lossy().into_owned());
            spec.env.insert("FREE".into(), free.id.clone());
            spec.env
                .insert("COMPLETE_TASK".into(), completed.id.clone());
            spec.env.insert(
                "FILING_ATTACH".into(),
                attachment.to_string_lossy().into_owned(),
            );
            spec.env
                .insert("FILING_REPO".into(), repo.to_string_lossy().into_owned());
            spec.env.insert("APPROVAL".into(), approval.id.clone());
            spec.env
                .insert("DISMISS".into(), magi::merge_decision::DISMISS.into());
            spec.command = vec![
                "sh".into(),
                "-c".into(),
                "cat >/dev/null\n\
                 test \"$MAGI_ALLOW_WRITE\" = 0 || exit 31\n\
                 test -n \"$MAGI_CHAT_CONTROL_FD\" || exit 32\n\
                 MAGI_HOME=\"$DECOY\" \"$ANSWER_BIN\" task add --repo \"$FILING_REPO\" --solo --urgent --priority 7 --implementers 2 --attach \"$FILING_ATTACH\" --hold 'owner requested' --title 'Follow-up' -- 'Implement follow-up work' || exit 35\n\
                 MAGI_HOME=\"$DECOY\" \"$ANSWER_BIN\" task priority \"$COMPLETE_TASK\" 12 || exit 36\n\
                 MAGI_HOME=\"$DECOY\" \"$ANSWER_BIN\" task done \"$COMPLETE_TASK\" || exit 37\n\
                 MAGI_HOME=\"$DECOY\" \"$ANSWER_BIN\" task control settle-decision \"$FREE\" --reason 'Owner selected src/lib.rs; no further effect remains'  || exit 33\n\
                 MAGI_HOME=\"$DECOY\" \"$ANSWER_BIN\" task control decide \"$APPROVAL\" \"$DISMISS\" || exit 34\n"
                    .into(),
            ];
            let mut seat = magi::agent::SeatState::new("chat", "mock", 1);
            let output = magi::agent::invoke(
                &spec,
                &mut seat,
                &magi::agent::Invocation {
                    coordination: None,
                    fetched_base: None,
                    cwd: home,
                    prompt: "",
                    timeout: std::time::Duration::from_secs(30),
                    allow_write: false,
                    sessions: false,
                    artifacts: &artifacts,
                    stem: "answer",
                    run: &f.talk,
                    node: CHAT_NODE,
                    cache_dir: None,
                    attachments: &[],
                    writable: &[],
                },
            )
            .await
            .unwrap();
            readonly(0o755);
            std::fs::set_permissions(decoy.join("queue"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            assert!(output.usable(), "{output:?}");
            let filed = f
                .queue()
                .list()
                .into_iter()
                .find(|task| task.title == "Follow-up")
                .expect("launcher filed the task");
            assert_eq!(filed.instruction, "Implement follow-up work");
            assert_eq!(filed.filed_by_chat(), Some(f.talk.as_str()));
            assert_eq!(filed.status, TaskStatus::Held);
            assert_eq!(filed.priority, 7);
            assert!(filed.solo && filed.urgent);
            assert_eq!(filed.overrides.as_ref().unwrap().candidates, Some(2));
            assert_eq!(filed.attachments.len(), 1);
            assert!(Queue::at(decoy.join("queue")).list().is_empty());
            assert_eq!(f.saved().priority, 12);
            assert_eq!(f.saved().status, TaskStatus::Done);
            assert_eq!(
                RunState::load_under(&f.run.id, home).unwrap().status,
                RunStatus::Superseded
            );
            // The launcher's report comes back through the bridge.
            assert!(output.text.contains("settled decision"), "{output:?}");
            assert!(output.text.contains("recorded"), "{output:?}");

            let decision = magi::task_inquiry::Outbox::at(home)
                .get(&free.id)
                .unwrap()
                .decision
                .unwrap();
            assert_eq!(decision.status, magi::task_decision::Status::Resolved);
            assert!(
                decision
                    .history
                    .last()
                    .unwrap()
                    .evidence
                    .contains("src/lib.rs")
            );
            assert_eq!(
                questions.get(&approval.id).unwrap().resolution().as_deref(),
                Some(magi::merge_decision::DISMISS)
            );
            let run = RunState::load_under(&state.id, home).unwrap();
            assert_eq!(
                run.merge_decision.unwrap().choice,
                magi::merge_decision::DISMISS
            );
            assert_eq!(run.status, RunStatus::Superseded);
            assert_eq!(run.pr.unwrap().state, "open");
            assert_eq!(f.queue().get(&owner.id).unwrap().status, TaskStatus::Done);
            for q in [&free, &approval] {
                assert_eq!(
                    std::fs::read(decoy_questions.path_of(&q.id)).unwrap(),
                    decoy_before[&q.id]
                );
            }
        },
    );
}

#[cfg(windows)]
#[test]
#[ignore = "requires an installed, authenticated CLI and the Windows read-only sandbox"]
fn windows_sandboxed_chat_controls_without_direct_store_writes() {
    common::isolated(
        "windows_sandboxed_chat_controls_without_direct_store_writes",
        || async {
            let f = Fixture::new();
            magi::run::set_home(f.tmp.path().to_owned());
            let artifacts = f.talks.artifacts_of(&f.talk);
            let sentinel = f.tmp.path().join("queue/protected.txt");
            std::fs::write(&sentinel, "unchanged").unwrap();
            let mut spec = f.run.config.agents[0].clone();
            spec.env
                .extend(common::magi_cli_env(f.tmp.path(), "MAGI_TEST_BINARY"));
            spec.kind = AgentKind::Codex;
            spec.command.clear();
            // Exercise environments that automatically exclude credential names.
            spec.extra_args.extend([
                "-c".into(),
                "shell_environment_policy.ignore_default_excludes=false".into(),
                "-c".into(),
                "shell_environment_policy.inherit=all".into(),
            ]);
            spec.env.insert("MAGI_TEST_TASK".into(), f.task.id.clone());
            let prompt = r#"Run these two PowerShell operations using your shell tool, without asking for approval:
1. try { [IO.File]::WriteAllText((Join-Path $env:MAGI_HOME 'queue/protected.txt'), 'changed'); Write-Output 'STORE_WRITE_ALLOWED' } catch { Write-Output 'STORE_WRITE_DENIED' }
2. & $env:MAGI_TEST_BINARY task control hold $env:MAGI_TEST_TASK 'sandbox control'
Do both even if one fails. Include the exact STORE_WRITE result and control result in your final response."#;
            let mut seat = magi::agent::SeatState::new("chat", "mock", 1);
            let output = magi::agent::invoke(
                &spec,
                &mut seat,
                &magi::agent::Invocation {
                    coordination: None,
                    fetched_base: None,
                    cwd: f.tmp.path(),
                    prompt,
                    timeout: std::time::Duration::from_secs(120),
                    allow_write: false,
                    sessions: false,
                    artifacts: &artifacts,
                    stem: "sandbox-pipe",
                    run: &f.talk,
                    node: CHAT_NODE,
                    cache_dir: None,
                    attachments: &[],
                    writable: &[],
                },
            )
            .await
            .unwrap();
            assert!(output.usable(), "{output:?}");
            assert!(output.text.contains("STORE_WRITE_DENIED"), "{output:?}");
            assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
            assert_eq!(f.saved().status, TaskStatus::Held);
            assert_eq!(f.saved().hold_reason.as_deref(), Some("sandbox control"));
        },
    );
}

#[test]
fn reconciliation_cli_rejects_non_chat_seats_and_preserves_direct_route() {
    let f = Fixture::new();
    let args = [
        "reconcile-retention",
        &f.task.id,
        "--authority-run",
        &f.run.id,
    ];
    for node in ["implement-1", "review-1", "task", ""] {
        let error = refused(f.cli(&args, node));
        assert!(error.contains("standing chat"), "{error}");
    }
    let error = refused(
        common::magi_cli(f.tmp.path())
            .args(["task"])
            .args(args)
            .env_remove("MAGI_RUN")
            .env_remove("MAGI_NODE")
            .output()
            .unwrap(),
    );
    let expected = if cfg!(unix) {
        "authority must be a retained implementation"
    } else {
        "retention reconciliation requires Unix leases"
    };
    assert!(error.contains(expected), "{error}");
}

#[cfg(unix)]
#[test]
fn chat_transport_rejects_reused_fds_and_never_replays_after_ack() {
    common::isolated(
        "chat_transport_rejects_reused_fds_and_never_replays_after_ack",
        || async {
            use std::io::{Read as _, Write as _};
            let f = Fixture::new();
            let before = std::fs::read(f.queue().path_of(&f.task.id)).unwrap();
            let sentinel = f.tmp.path().join("unrelated-file");
            std::fs::write(&sentinel, "must stay intact").unwrap();
            let script = f.tmp.path().join("reuse.py");
            std::fs::write(
                &script,
                r#"import os, subprocess, sys
sys.stdin.read()
fd = int(os.environ['MAGI_CHAT_CONTROL_FD'])
os.close(fd)
file = os.open(os.environ['SENTINEL'], os.O_WRONLY)
os.dup2(file, fd)
p = subprocess.run([os.environ['TEST_BINARY'], 'task', 'control', 'hold', os.environ['TEST_TASK']],
    pass_fds=tuple(int(os.environ[n]) for n in ['MAGI_CHAT_CONTROL_FD', 'MAGI_CHAT_CONTROL_RESPONSE_FD', 'MAGI_CHAT_CONTROL_LOCK_FD']), capture_output=True, text=True,
    env={**os.environ, 'MAGI_CHAT_CONTROL_SOCKET':'/nonexistent/control'})
assert p.returncode != 0 and 'identity mismatch' in p.stderr, p.stderr
print('reused descriptor rejected')
"#,
            )
            .unwrap();
            magi::run::set_home(f.tmp.path().to_owned());
            let mut spec = f.run.config.agents[0].clone();
            spec.command = vec!["python3".into(), script.to_str().unwrap().into()];
            spec.env
                .extend(common::magi_cli_env(f.tmp.path(), "TEST_BINARY"));
            spec.env.insert("TEST_TASK".into(), f.task.id.clone());
            spec.env
                .insert("SENTINEL".into(), sentinel.to_str().unwrap().into());
            let mut seat = magi::agent::SeatState::new("chat", "mock", 1);
            let artifacts = f.talks.artifacts_of(&f.talk);
            let output = magi::agent::invoke(
                &spec,
                &mut seat,
                &magi::agent::Invocation {
                    coordination: None,
                    fetched_base: None,
                    cwd: f.tmp.path(),
                    prompt: "",
                    timeout: std::time::Duration::from_secs(30),
                    allow_write: false,
                    sessions: false,
                    artifacts: &artifacts,
                    stem: "reuse",
                    run: &f.talk,
                    node: CHAT_NODE,
                    cache_dir: None,
                    attachments: &[],
                    writable: &[],
                },
            )
            .await
            .unwrap();
            assert!(output.usable(), "{output:?}");
            assert_eq!(
                std::fs::read_to_string(sentinel).unwrap(),
                "must stay intact"
            );
            assert_eq!(
                std::fs::read(f.queue().path_of(&f.task.id)).unwrap(),
                before
            );

            // A server that authenticates, reads the request, then loses its response
            // is fundamentally different from transport preparation failure. Even a
            // writable chat must not execute this hold locally or resend it.
            let endpoint = f.tmp.path().join("lost-response");
            let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut cap = [0; 64];
                stream.read_exact(&mut cap).unwrap();
                stream.write_all(b"A").unwrap();
                let mut len = [0; 4];
                stream.read_exact(&mut len).unwrap();
                let mut body = vec![0; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut body).unwrap();
            });
            let output = common::magi_cli(f.tmp.path())
                .env("MAGI_RUN", &f.talk)
                .env("MAGI_NODE", CHAT_NODE)
                .env("MAGI_ALLOW_WRITE", "1")
                .env("MAGI_CHAT_CONTROL_SOCKET", &endpoint)
                .env("MAGI_CHAT_CONTROL_CAP", "0".repeat(64))
                .env_remove("MAGI_CHAT_CONTROL_IDENTITIES")
                .args(["task", "control", "hold", &f.task.id, "must not replay"])
                .output()
                .unwrap();
            server.join().unwrap();
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("outcome is unknown"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read(f.queue().path_of(&f.task.id)).unwrap(),
                before
            );
        },
    );
}
