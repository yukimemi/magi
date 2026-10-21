//! 実 CLI と隔離したストア・偽の forge によるチャット制御の回帰検証。
#[allow(unused_macros, unused_imports)]
mod common;

use std::collections::BTreeMap;
use std::process::{Command, Output};

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
        let tmp = tempfile::tempdir().unwrap();
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
    fn cli(&self, args: &[&str], node: &str, scope: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_magi"))
            .args(["task"])
            .args(args)
            .env("MAGI_HOME", self.tmp.path())
            .env("MAGI_CONFIG_DIR", "")
            .env("MAGI_RUN", &self.talk)
            .env("MAGI_NODE", node)
            .env("MAGI_TURN_SCOPE", scope)
            .output()
            .unwrap()
    }
    fn control(&self, args: &[&str]) -> Output {
        let mut all = vec!["control"];
        all.extend_from_slice(args);
        self.cli(&all, CHAT_NODE, "repository")
    }
    fn owner(&self, words: &str) {
        let mut t = self.talks.get(&self.talk).unwrap();
        talk::record(&mut t, &self.talks, words, vec![]).unwrap();
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
fn sandboxed_chat_turn_controls_own_work_and_refuses_unrelated_writes() {
    common::isolated(
        "sandboxed_chat_turn_controls_own_work_and_refuses_unrelated_writes",
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
    let words = format!("abandon-run: {}", f.run.id);
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
assert not any('writable_roots' in s or 'network_access=true' in s for s in settings)
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
# The server binds the capability to this talk even if the child spoofs env.
other = subprocess.run([binary, 'task', 'control', 'hold', os.environ['FOREIGN_TASK']],
                       capture_output=True, text=True, pass_fds=fds, env={**os.environ, 'MAGI_RUN':os.environ['FOREIGN_TALK']})
assert other.returncode != 0
assert 'not filed by this conversation' in other.stderr, other.stderr
bad = subprocess.run([binary, 'task', '--quote', 'invented', 'control', 'abandon', run],
                     capture_output=True, text=True, pass_fds=fds)
assert bad.returncode != 0
assert 'individual owner confirmation' in bad.stderr, bad.stderr
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
control('--quote', 'abandon-run: '+run, 'control', 'abandon', run)
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
        ("MAGI_TEST_BINARY".into(), env!("CARGO_BIN_EXE_magi").into()),
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
    let lease = f.talks.claim_turn(&f.talk).unwrap().unwrap();
    talk::say(&lease, &mut t, &f.talks, &cfg, &words, vec![])
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
    assert_eq!(
        questions.get(&question.id).unwrap().status,
        magi::ask::QuestionStatus::Abandoned
    );
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
    refused(f.control(&["resume", &f.run.id]));
    passed(f.control(&["release", &f.task.id]));
    passed(f.control(&["resume", &f.run.id]));
    let _claim = f.queue().claim(&f.task.id).unwrap();
    refused(f.control(&["hold", &f.task.id, "busy"]));
}

#[tokio::test]
async fn abandon_requires_current_individual_consent_and_proven_dead_liveness() {
    let _home = common::home_lock().await;
    let mut f = Fixture::new();
    refused(f.control(&["abandon", &f.run.id]));
    let words = format!("abandon-run: {}", f.run.id);
    f.owner(&words);
    let run_id = f.run.id.clone();
    let args = ["--quote", &words, "control", "abandon", &run_id];
    f.run.driver_exited = false;
    f.run.save_under(f.tmp.path()).unwrap();
    assert_eq!(f.run.liveness_under(f.tmp.path(), false), Liveness::Unknown);
    assert!(refused(f.cli(&args, CHAT_NODE, "repository")).contains("liveness is not proven gone"));
    f.run.driver_pid = Some(std::process::id());
    f.run.driver_started_at = magi::proc::process_started_at(std::process::id());
    f.run.save_under(f.tmp.path()).unwrap();
    assert_eq!(f.run.liveness_under(f.tmp.path(), false), Liveness::Live);
    assert!(refused(f.cli(&args, CHAT_NODE, "repository")).contains("liveness is not proven gone"));
    f.run.driver_pid = None;
    f.run.driver_started_at = None;
    f.run.driver_exited = true;
    f.run.save_under(f.tmp.path()).unwrap();
    assert_eq!(f.run.liveness_under(f.tmp.path(), false), Liveness::Dead);
    let mut t = f.talks.get(&f.talk).unwrap();
    talk::queue(&mut t, &f.talks, "wait", vec![]).unwrap();
    refused(f.cli(&args, CHAT_NODE, "repository"));
    talk::clear_pending(&mut t, &f.talks).unwrap();
    passed(f.cli(&args, CHAT_NODE, "repository"));
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
if [ "$1" = pr ]; then
  cat "$MAGI_HOME/snapshot.json"
else
  echo "$3" >> "$MAGI_HOME/sent"
  if [ -f "$MAGI_HOME/fail" ]; then exit 1; fi
fi
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
        Command::new(env!("CARGO_BIN_EXE_magi"))
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
            .env("MAGI_HOME", f.tmp.path())
            .env("MAGI_CONFIG_DIR", "")
            .env("MAGI_RUN", &f.talk)
            .env("MAGI_NODE", CHAT_NODE)
            .env("MAGI_TURN_SCOPE", "repository")
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
