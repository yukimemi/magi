//! Consultation continuations use the same session without supplying owner evidence.
#![cfg(unix)]
mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use magi::ask::{Question, Questions};
use magi::config::{AgentKind, AgentSpec, Config};
use magi::talk::{self, Talk, Talks};

struct Fixture {
    _tmp: tempfile::TempDir,
    talks: Talks,
    questions: Questions,
    talk: Talk,
    question: Question,
    cfg: Config,
    trace: PathBuf,
    mode: PathBuf,
    execution_files: Vec<(PathBuf, Vec<u8>)>,
    _claim: magi::queue::Claim,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_task_repository(false).await
    }

    async fn with_task_repository(other: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let cli_path = format!(
            "{}:{}",
            tmp.path().display(),
            std::env::var("PATH").unwrap()
        );
        // Agent selection checks the parent's PATH before invocation uses the
        // per-agent environment. Both must find the fixture CLI on CI too.
        // SAFETY: e2e runs in a dedicated process on a current-thread runtime;
        // no other turns or blocking work have started before this first await.
        unsafe { std::env::set_var("PATH", &cli_path) };
        magi::run::set_home(tmp.path().to_owned());
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        magi::git::git(&repo, &["init"]).await.unwrap();
        std::fs::write(repo.join(".git/updater.contract"), "test-v1").unwrap();
        let trace = tmp.path().join("trace");
        std::fs::create_dir(&trace).unwrap();
        let mode = tmp.path().join("mode");
        std::fs::write(&mode, "investigate").unwrap();
        let cli = tmp.path().join("codex");
        std::fs::write(&cli, r#"#!/bin/sh
cat >/dev/null
cp "$MAGI_PROMPT_FILE" "$TRACE/$MAGI_TURN.prompt"
printf '%s\n' "$@" > "$TRACE/$MAGI_TURN.args"
printf '%s\n' "$MAGI_ALLOW_WRITE" > "$TRACE/$MAGI_TURN.write"
printf '%s\n' '{"type":"thread.started","thread_id":"standing-session"}'
case "$(cat "$MAGI_PROMPT_FILE")" in
  *'Current turn scope: consultation investigation'*)
    printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Evidence inspected; owner decision still needed."}}';;
  *)
    case "$(cat "$MODE")" in
      investigate) printf '%s\n' "$REQUEST_EVENT";;
      closed) cp "$CLOSED_RECORD" "$TALK_RECORD"; printf '%s\n' "$REQUEST_EVENT";;
      failed) printf '%s\n' "$REQUEST_EVENT"; exit 1;;
      quoted) printf '%s\n' "$QUOTED_EVENT";;
      *) printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Control reply."}}';;
    esac;;
esac
"#).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = Config {
            coordination: Some(toml::from_str("contract_file='updater.contract'\ncontract_token='test-v1'\nlock_file='updater.lock'").unwrap()),
            ..Config::default()
        };
        // Investigation must remain read-only even when the standing chat can edit.
        cfg.talk.allow_write = true;
        cfg.agents = vec![AgentSpec {
            id: "fixture".into(),
            kind: AgentKind::Codex,
            model: None,
            command: Vec::new(),
            extra_args: Vec::new(),
            prompt_delivery: None,
            max_turn_secs: None,
            env: BTreeMap::from([
                ("PATH".into(), cli_path),
                ("TRACE".into(), trace.to_str().unwrap().into()),
                ("MODE".into(), mode.to_str().unwrap().into()),
            ]),
        }];
        let talks = Talks::at(tmp.path().join("talks"));
        let questions = Questions::at(tmp.path().join("questions"));
        let talk = talk::begin(&talks, &cfg, repo, Some("fixture")).unwrap();
        let task_repo = if other {
            let repo = tmp.path().join("other-repo");
            std::fs::create_dir(&repo).unwrap();
            magi::git::git(&repo, &["init"]).await.unwrap();
            std::fs::write(repo.join(".git/updater.contract"), "test-v1").unwrap();
            repo
        } else {
            talk.repo.clone()
        };
        let mut question = Question::new(
            "task-run".into(),
            magi::land::APPROVAL_NODE.into(),
            "land".into(),
            "Why did the task fail?".into(),
            "Inspect before deciding.".into(),
            vec![magi::land::APPROVE.into(), magi::land::HOLD.into()],
        );
        question.cwd = Some(task_repo.to_string_lossy().into());
        question.answer_timeout = 3600;
        question.merge_target = Some(magi::merge_decision::Target {
            pr: "https://example.invalid/pull/1".into(),
            head: "exact-reviewed-head".into(),
            language: "en".into(),
        });
        question.waiter = Some(magi::ask::Waiter {
            kind: magi::ask::WaiterKind::Daemon,
            since: jiff::Timestamp::now(),
        });
        let mut run = magi::run::RunState::new(
            task_repo.clone(),
            "main".into(),
            "base-head".into(),
            "Task awaiting evidence".into(),
            cfg.clone(),
        );
        run.parked = true;
        run.save_under(tmp.path()).unwrap();
        question.run = run.id.clone();
        let queue = magi::queue::Queue::at(tmp.path().join("queue"));
        let mut task = magi::queue::Task::new(
            "Original task".into(),
            "Original instruction".into(),
            task_repo,
            magi::queue::Source::Agent {
                run: talk.id.clone(),
                node: "chat".into(),
            },
        );
        task.status = magi::queue::TaskStatus::Running;
        task.runs.push(run.id.clone());
        queue.put(&mut task).unwrap();
        let claim = queue.claim(&task.id).unwrap();
        let mut execution_files: Vec<(PathBuf, Vec<u8>)> = [
            queue.path_of(&task.id),
            tmp.path().join("queue").join(format!("{}.lock", task.id)),
            tmp.path().join("runs").join(&run.id).join("run.json"),
        ]
        .into_iter()
        .map(|path| {
            let contents = std::fs::read(&path).unwrap();
            (path, contents)
        })
        .collect();
        questions.put(&mut question).unwrap();
        magi::consult::begin(&questions, &talks, &question, &talk).unwrap();
        let question = questions.get(&question.id).unwrap();
        questions.beat(&question.id, magi::ask::WaiterKind::Daemon);
        let waiter_lease = questions.lease_path(&question.id);
        execution_files.push((waiter_lease.clone(), std::fs::read(waiter_lease).unwrap()));
        let request = serde_json::json!({"investigate_question": question.id}).to_string();
        let event = |text: String| {
            serde_json::json!({"type":"item.completed", "item":{"type":"agent_message", "text":text}}).to_string()
        };
        cfg.agents[0]
            .env
            .insert("REQUEST_EVENT".into(), event(request.clone()));
        cfg.agents[0].env.insert(
            "QUOTED_EVENT".into(),
            event(format!("```json\n{request}\n```")),
        );
        Self {
            _tmp: tmp,
            talks,
            questions,
            talk,
            question,
            cfg,
            trace,
            mode,
            execution_files,
            _claim: claim,
        }
    }

    async fn delivery(&mut self) {
        self.talk = self.talks.get(&self.talk.id).unwrap();
        let lease = self.talks.claim_turn(&self.talk.id).unwrap().unwrap();
        let text = talk::drain(&mut self.talk, &self.talks).unwrap().unwrap();
        talk::respond(&lease, &mut self.talk, &self.talks, &self.cfg, &text)
            .await
            .unwrap();
        self.unchanged();
    }

    async fn say(&mut self, text: &str) {
        let lease = self.talks.claim_turn(&self.talk.id).unwrap().unwrap();
        talk::say(
            &lease,
            &mut self.talk,
            &self.talks,
            &self.cfg,
            text,
            Vec::new(),
        )
        .await
        .unwrap();
        self.unchanged();
    }

    fn seat(&self) -> magi::agent::SeatState {
        serde_json::from_value(serde_json::to_value(&self.talk).unwrap()["seat"].clone()).unwrap()
    }

    fn unchanged(&self) {
        assert_eq!(
            self.questions.get(&self.question.id).unwrap(),
            self.question
        );
        for (path, original) in &self.execution_files {
            assert_eq!(&std::fs::read(path).unwrap(), original);
        }
    }
}

common::e2e! {
async fn consultation_investigates_on_delivery_and_natural_followup_then_returns_to_control() {
    let mut f = Fixture::new().await;
    f.delivery().await;
    assert_eq!(f.talk.turns.len(), 2, "internal request is not an operator send");
    assert_eq!(f.seat().turns, 2);
    assert_eq!(f.seat().captured_session.as_deref(), Some("standing-session"));
    assert!(f.talk.turns.last().unwrap().body.contains("Evidence inspected"));
    let args = std::fs::read_to_string(f.trace.join("1.args")).unwrap();
    assert!(args.contains("resume\nstanding-session"));
    assert!(!args.contains("dangerously-bypass"));
    assert!(!args.contains("writable_roots"));
    assert_eq!(std::fs::read_to_string(f.trace.join("1.write")).unwrap().trim(), "0");
    assert!(std::fs::read_to_string(f.trace.join("1.prompt")).unwrap().contains("launcher holds the exclusive repository lock"));
    f.say("Please investigate why this failed before I choose an answer.").await;
    assert_eq!(f.talk.turns.len(), 4);
    assert_eq!(f.seat().turns, 4);
    // Investigation's internal request never becomes authentic owner authority.
    let out = common::magi_cli(f._tmp.path())
        .args(["answer", &f.question.id, "--reply", magi::land::APPROVE, "--quote", "Please investigate why this failed before I choose an answer."])
        .env("MAGI_CONFIG_DIR", "")
        .env("MAGI_RUN", &f.talk.id).env("MAGI_NODE", "chat").output().unwrap();
    assert!(!out.status.success());
    f.unchanged();
    std::fs::write(&f.mode, "control").unwrap();
    f.say("What decision is still needed?").await;
    assert_eq!(f.seat().turns, 5);
    let prompt = std::fs::read_to_string(f.trace.join("4.prompt")).unwrap();
    assert!(prompt.contains("Current turn scope: consultation control"));
    assert!(!prompt.contains("launcher holds the exclusive repository lock"));
    let instruction = "hold".to_string();
    f.say(&instruction).await;
    let out = common::magi_cli(f._tmp.path())
        .args(["answer", &f.question.id, "--reply", "hold", "--quote", &instruction])
        .env("MAGI_CONFIG_DIR", "")
        .env("MAGI_RUN", &f.talk.id).env("MAGI_NODE", "chat").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let answered = f.questions.get(&f.question.id).unwrap();
    assert!(!answered.status.open());
    assert_eq!(answered.consult, f.question.consult);
    assert_eq!(answered.waiter, f.question.waiter);
    f.question = answered;
    f.say("Now continue our ordinary conversation.").await;
    let prompt = std::fs::read_to_string(f.trace.join("6.prompt")).unwrap();
    assert!(prompt.contains("Current turn scope: ordinary repository conversation"));
    assert!(!prompt.contains("Current turn scope: consultation control"));

}
}

common::e2e! {
async fn busy_consultation_investigation_stays_open_and_can_retry_in_same_conversation() {
    use std::os::fd::AsRawFd as _;
    let mut f = Fixture::new().await;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(f.talk.repo.join(".git/updater.lock")).unwrap();
    // SAFETY: descriptor remains open until explicitly unlocked below.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    tokio::time::timeout(std::time::Duration::from_secs(10), f.delivery()).await.unwrap();
    assert_eq!(f.seat().turns, 1);
    assert!(!f.trace.join("1.prompt").exists());
    assert!(f.talk.turns.last().unwrap().body.contains("Investigation pending"));
    assert!(f.talk.turns.last().unwrap().body.contains("owner unknown"));
    // SAFETY: this test owns the external descriptor, never another seat's lock.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    f.say("Investigate again now.").await;
    assert_eq!(f.seat().turns, 3);
    assert!(f.talk.turns.last().unwrap().body.contains("Evidence inspected"));
}
}

common::e2e! {
async fn quoted_and_failed_investigation_requests_do_not_start_repository_turns() {
    let mut f = Fixture::new().await;
    std::fs::write(&f.mode, "quoted").unwrap();
    f.delivery().await;
    assert_eq!(f.seat().turns, 1);
    std::fs::write(&f.mode, "failed").unwrap();
    assert!(talk::say(&f.talks.claim_turn(&f.talk.id).unwrap().unwrap(), &mut f.talk, &f.talks, &f.cfg, "Investigate", Vec::new()).await.is_err());
    assert_eq!(f.seat().turns, 2);
    assert!(!f.trace.join("2.prompt").exists());
    f.unchanged();
}
}

common::e2e! {
async fn closing_during_control_response_cancels_investigation_before_repository_spawn() {
    let mut f = Fixture::new().await;
    let mut closed = f.talks.get(&f.talk.id).unwrap();
    closed.status = talk::TalkStatus::Closed;
    let record = f._tmp.path().join("closed.json");
    std::fs::write(&record, serde_json::to_vec(&closed).unwrap()).unwrap();
    f.cfg.agents[0].env.insert("CLOSED_RECORD".into(), record.to_str().unwrap().into());
    f.cfg.agents[0].env.insert("TALK_RECORD".into(), f.talks.path_of(&f.talk.id).to_str().unwrap().into());
    std::fs::write(&f.mode, "closed").unwrap();
    f.delivery().await;
    assert_eq!(f.seat().turns, 1);
    assert!(!f.trace.join("1.prompt").exists());
    assert_eq!(f.talks.get(&f.talk.id).unwrap().status, talk::TalkStatus::Closed);
    assert!(f.talk.turns.last().unwrap().body.contains("investigation cancelled"));
}
}

common::e2e! {
async fn foreign_repository_consultation_stays_pending_even_when_standing_repository_is_free() {
    use std::os::fd::AsRawFd as _;
    let mut f = Fixture::with_task_repository(true).await;
    let target = PathBuf::from(f.question.cwd.as_ref().unwrap());
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(target.join(".git/updater.lock")).unwrap();
    // SAFETY: this test owns the descriptor through both consultation turns.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    tokio::time::timeout(std::time::Duration::from_secs(10), f.delivery()).await.unwrap();
    assert_eq!(f.seat().turns, 1);
    assert!(!f.trace.join("1.prompt").exists());
    let reply = &f.talk.turns.last().unwrap().body;
    assert!(reply.contains("Investigation pending"));
    assert!(reply.contains("repository differs"));
    assert!(reply.contains("magi task show"));
    // SAFETY: release only this test's descriptor, never another seat's lock.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    // An idle foreign repository still cannot borrow the standing policy.
    // Missing cwd must not make the persisted task/run association disappear.
    f.question.cwd = None;
    f.questions.put(&mut f.question).unwrap();
    f.say("Investigate again without changing or answering the question.").await;
    assert_eq!(f.seat().turns, 2);
    assert!(!f.trace.join("2.prompt").exists());
    assert!(f.talk.turns.last().unwrap().body.contains("repository differs"));
}
}

common::e2e! {
async fn unresolved_consultation_repository_does_not_fall_back_to_question_cwd() {
    let mut f = Fixture::new().await;
    let queue = magi::queue::Queue::at(f._tmp.path().join("queue"));
    let task = queue.list().into_iter().find(|t| t.runs.contains(&f.question.run)).unwrap();
    let paths = [
        queue.path_of(&task.id),
        f._tmp.path().join("runs").join(&f.question.run).join("run.json"),
    ];
    for path in &paths {
        std::fs::remove_file(path).unwrap();
    }
    f.execution_files.retain(|(path, _)| !paths.contains(path));
    // cwd still exists, but does not establish the task's repository policy.
    assert!(f.question.cwd.is_some());
    f.delivery().await;
    assert_eq!(f.seat().turns, 1);
    assert!(!f.trace.join("1.prompt").exists());
    let reply = &f.talk.turns.last().unwrap().body;
    assert!(reply.contains("Investigation pending"));
    assert!(reply.contains("cannot resolve consulted task repository"));
    assert!(reply.contains("restore that metadata"));
}
}

common::e2e! {
async fn ending_one_consultation_keeps_other_consults_then_restores_the_next_turn() {
    let mut f = Fixture::new().await;
    std::fs::write(&f.mode, "control").unwrap();
    f.delivery().await;
    let mut other = magi::ask::Question::new(
        f.question.run.clone(), "implement".into(), "implement".into(),
        "Another decision".into(), "".into(), vec!["A".into(), "B".into()],
    );
    f.questions.put(&mut other).unwrap();
    magi::consult::begin(&f.questions, &f.talks, &other, &f.talk).unwrap();
    // Drain the actual delivery and persist a later authentic owner send.
    f.talk = f.talks.get(&f.talk.id).unwrap();
    talk::drain(&mut f.talk, &f.talks).unwrap().unwrap();
    let words = "Reimplement from scratch instead";
    talk::record(&mut f.talk, &f.talks, words, Vec::new()).unwrap();
    let guard = f.talks.answer_guard().unwrap();
    f.questions.update(&f.question.id, |q| magi::consult::record_other(q, &f.talk, words, Some(words))).unwrap();
    drop(guard);
    f.question = f.questions.get(&f.question.id).unwrap();
    assert!(f.question.status.open());
    assert!(f.question.answer.is_none());
    assert!(magi::consult::pending_consults(&f.questions, &f.talk.id));
    f.say("One question is still pending.").await;
    let prompt = std::fs::read_to_string(f.trace.join("1.prompt")).unwrap();
    assert!(prompt.contains("Current turn scope: consultation control"));
    talk::record(&mut f.talk, &f.talks, words, Vec::new()).unwrap();
    let guard = f.talks.answer_guard().unwrap();
    f.questions.update(&other.id, |q| magi::consult::record_other(q, &f.talk, words, Some(words))).unwrap();
    drop(guard);
    assert!(!magi::consult::pending_consults(&f.questions, &f.talk.id));
    f.say("Continue our ordinary conversation.").await;
    let prompt = std::fs::read_to_string(f.trace.join("2.prompt")).unwrap();
    assert!(prompt.contains("Current turn scope: ordinary repository conversation"));
    assert!(!prompt.contains("Current turn scope: consultation control"));
}
}
