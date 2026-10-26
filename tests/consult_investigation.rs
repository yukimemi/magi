//! A consultation uses the standing chat's session and does not queue behind
//! repository work. A pending consultation grants no extra access: even a
//! read-only chat keeps its read-only sandbox and answers over the inherited
//! control bridge. Owner requests run through the CLI with no quote evidence.
#![cfg(unix)]
mod common;

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
        std::fs::write(&mode, "reply").unwrap();
        let cli = tmp.path().join("codex");
        std::fs::write(&cli, r#"#!/bin/sh
cat >/dev/null
# Title generation reuses the agent on its own seat at turn 0; it must not
# overwrite the conversation's first-turn trace.
[ "$MAGI_SEAT" = "talk-title" ] && exit 0
cp "$MAGI_PROMPT_FILE" "$TRACE/$MAGI_TURN.prompt"
printf '%s\n' "$@" > "$TRACE/$MAGI_TURN.args"
printf '%s\n' "$MAGI_ALLOW_WRITE" > "$TRACE/$MAGI_TURN.write"
printf '%s\n' '{"type":"thread.started","thread_id":"standing-session"}'
case "$(cat "$MODE")" in
  answer) "$ANSWER_BIN" answer "$QUESTION" --reply hold > "$TRACE/$MAGI_TURN.answer" 2>&1 || exit 9;;
  closed) cp "$CLOSED_RECORD" "$TALK_RECORD";;
esac
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Turn reply."}}'
"#).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = Config {
            coordination: Some(toml::from_str("contract_file='updater.contract'\ncontract_token='test-v1'\nlock_file='updater.lock'").unwrap()),
            ..Config::default()
        };
        // The standing conversation's own access applies to a consultation too.
        cfg.talk.allow_write = true;
        let mut env = common::magi_cli_env(tmp.path(), "ANSWER_BIN");
        env.insert("PATH".into(), cli_path);
        env.insert("TRACE".into(), trace.to_str().unwrap().into());
        env.insert("MODE".into(), mode.to_str().unwrap().into());
        cfg.agents = vec![AgentSpec {
            id: "fixture".into(),
            kind: AgentKind::Codex,
            model: None,
            command: Vec::new(),
            extra_args: Vec::new(),
            prompt_delivery: None,
            max_turn_secs: None,
            env,
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
            "Task awaiting the owner".into(),
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
        cfg.agents[0]
            .env
            .insert("QUESTION".into(), question.id.clone());
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
    }

    fn seat(&self) -> magi::agent::SeatState {
        serde_json::from_value(serde_json::to_value(&self.talk).unwrap()["seat"].clone()).unwrap()
    }

    fn trace(&self, turn: usize, kind: &str) -> String {
        std::fs::read_to_string(self.trace.join(format!("{turn}.{kind}"))).unwrap()
    }

    /// The question is still the owner's to decide, and no execution state moved.
    fn unchanged(&self) {
        assert_eq!(
            self.questions.get(&self.question.id).unwrap(),
            self.question
        );
        assert!(self.question.status.open());
        for (path, original) in &self.execution_files {
            assert_eq!(&std::fs::read(path).unwrap(), original);
        }
    }

    fn lock(path: &std::path::Path) -> std::fs::File {
        use std::os::fd::AsRawFd as _;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join(".git/updater.lock"))
            .unwrap();
        // SAFETY: the descriptor stays open until the returned file drops.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        lock
    }
}

common::e2e! {
async fn consultation_is_an_ordinary_turn_with_the_standing_session_and_access() {
    let mut f = Fixture::new().await;
    f.delivery().await;
    assert_eq!(f.seat().turns, 1);
    assert_eq!(f.seat().captured_session.as_deref(), Some("standing-session"));
    let prompt = f.trace(0, "prompt");
    assert!(prompt.contains(&f.question.id), "{prompt}");
    assert!(!prompt.contains("Current turn scope"));
    assert!(!prompt.contains("investigate_question"));
    // A chat turn is never scheduled behind repository work.
    assert!(!prompt.contains("launcher holds the exclusive repository lock"));
    // The repository's `[talk] allow_write` applies unchanged.
    assert!(f.trace(0, "args").contains("dangerously-bypass"));
    assert_eq!(f.trace(0, "write").trim(), "1");
    f.say("What decision is still needed?").await;
    f.unchanged();
    assert_eq!(f.seat().turns, 2);
    assert!(f.trace(1, "args").contains("resume\nstanding-session"));
    assert_eq!(f.trace(1, "write").trim(), "1");
    assert_eq!(f.talk.turns.last().unwrap().body, "Turn reply.");
}
}

common::e2e! {
async fn owner_request_in_a_consultation_is_answered_by_the_turn_without_a_quote() {
    let mut f = Fixture::new().await;
    f.delivery().await;
    std::fs::write(&f.mode, "answer").unwrap();
    f.say("それは保留にしておいて").await;
    let answer = f.trace(1, "answer");
    let answered = f.questions.get(&f.question.id).unwrap();
    assert!(!answered.status.open(), "{answer}");
    assert_eq!(answered.resolution().as_deref(), Some(magi::land::HOLD));
    assert_eq!(answered.consult, f.question.consult);
    assert_eq!(answered.waiter, f.question.waiter);
    // Recording the answer moves no execution state by itself.
    for (path, original) in &f.execution_files {
        assert_eq!(&std::fs::read(path).unwrap(), original);
    }
}
}

common::e2e! {
async fn consultation_turn_does_not_wait_behind_repository_coordination() {
    let mut f = Fixture::new().await;
    // The task awaiting this answer may hold the repository's lock.
    let lock = Fixture::lock(&f.talk.repo);
    tokio::time::timeout(std::time::Duration::from_secs(10), f.delivery())
        .await
        .expect("a consultation never waits for repository coordination");
    assert_eq!(f.seat().turns, 1);
    assert!(!f.trace(0, "prompt").contains("launcher holds the exclusive repository lock"));
    assert_eq!(f.talk.turns.last().unwrap().body, "Turn reply.");
    drop(lock);
}
}

common::e2e! {
async fn foreign_repository_consultation_runs_in_the_standing_conversation() {
    let mut f = Fixture::with_task_repository(true).await;
    let target = PathBuf::from(f.question.cwd.as_ref().unwrap());
    let lock = Fixture::lock(&target);
    tokio::time::timeout(std::time::Duration::from_secs(10), f.delivery())
        .await
        .expect("a foreign repository's coordination does not hold the chat");
    assert_eq!(f.seat().turns, 1);
    // The turn runs in the standing checkout; the task's repository and its
    // coordination policy are neither entered nor borrowed.
    let args = f.trace(0, "args");
    assert!(args.contains(&*f.talk.repo.to_string_lossy()), "{args}");
    assert!(!args.contains(&*target.to_string_lossy()), "{args}");
    drop(lock);
    f.say("Is it still waiting?").await;
    f.unchanged();
    assert_eq!(f.seat().turns, 2);
}
}

common::e2e! {
async fn closing_during_a_consultation_turn_keeps_the_conversation_closed() {
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
    // The reply to the last message before the close cannot reopen it, and
    // the question stays open for the owner (checked by `delivery`).
    assert_eq!(f.talks.get(&f.talk.id).unwrap().status, talk::TalkStatus::Closed);
}
}

common::e2e! {
async fn ending_one_consultation_keeps_the_other_open_and_the_turns_ordinary() {
    let mut f = Fixture::new().await;
    f.delivery().await;
    let mut other = magi::ask::Question::new(
        f.question.run.clone(), "implement".into(), "implement".into(),
        "Another decision".into(), "".into(), vec!["A".into(), "B".into()],
    );
    f.questions.put(&mut other).unwrap();
    magi::consult::begin(&f.questions, &f.talks, &other, &f.talk).unwrap();
    f.talk = f.talks.get(&f.talk.id).unwrap();
    talk::drain(&mut f.talk, &f.talks).unwrap().unwrap();
    let words = "Reimplement from scratch instead";
    talk::record(&mut f.talk, &f.talks, words, Vec::new()).unwrap();
    f.questions.update(&f.question.id, |q| magi::consult::record_other(q, words)).unwrap();
    f.question = f.questions.get(&f.question.id).unwrap();
    assert!(f.question.status.open());
    assert!(f.question.answer.is_none());
    assert!(magi::consult::pending_consults(&f.questions, &f.talk.id));
    f.say("One question is still pending.").await;
    f.unchanged();
    f.questions.update(&other.id, |q| magi::consult::record_other(q, words)).unwrap();
    assert!(!magi::consult::pending_consults(&f.questions, &f.talk.id));
    f.say("Continue our ordinary conversation.").await;
    f.unchanged();
    // With or without a pending consultation, every turn had the same access.
    for turn in 0..f.seat().turns {
        assert_eq!(f.trace(turn, "write").trim(), "1", "turn {turn}");
        assert!(!f.trace(turn, "prompt").contains("Current turn scope"));
    }
}
}

async fn read_only_consultation_answers(free_text: bool) {
    let mut f = Fixture::new().await;
    f.cfg.talk.allow_write = false;
    let (q, ()) = f
        .questions
        .update(&f.question.id, |q| {
            q.node = "ordinary-question".into();
            q.merge_target = None;
            q.choices = if free_text {
                vec![]
            } else {
                vec!["hold".into()]
            };
            Ok(())
        })
        .unwrap();
    f.question = q;
    assert!(!magi::task_chat::awaits_decision(&f.question));
    f.delivery().await;
    f.say("回答前に相談を続けます。").await;
    // A pending consultation changes nothing about the turn's access.
    for turn in [0, 1] {
        let args = f.trace(turn, "args");
        assert!(args.contains("read-only"), "{args}");
        assert!(!args.contains("workspace-write"), "{args}");
        assert!(!args.contains("dangerously-bypass"), "{args}");
        assert!(
            !args.contains(f._tmp.path().join("questions").to_str().unwrap()),
            "{args}"
        );
        assert_eq!(f.trace(turn, "write").trim(), "0");
    }
    std::fs::write(&f.mode, "answer").unwrap();
    f.say("hold と回答してください。").await;
    let saved = f.questions.get(&f.question.id).unwrap();
    assert!(f.trace(2, "args").contains("read-only"));
    assert_eq!(f.trace(2, "write").trim(), "0");
    assert_eq!(saved.status, magi::ask::QuestionStatus::Answered);
    let expected = if free_text {
        magi::ask::Answer::Text("hold".into())
    } else {
        magi::ask::Answer::Choice("hold".into())
    };
    assert_eq!(saved.answer, Some(expected));
    assert!(!magi::consult::pending_consults(&f.questions, &f.talk.id));
    std::fs::write(&f.mode, "reply").unwrap();
    f.say("通常の会話を続けます。").await;
    assert!(f.trace(3, "args").contains("read-only"));
    assert_eq!(f.trace(3, "write").trim(), "0");
}

common::e2e! {
async fn read_only_consultation_can_answer_an_ordinary_question() {
    read_only_consultation_answers(false).await;
}
}

common::e2e! {
async fn read_only_consultation_can_answer_a_free_text_question() {
    read_only_consultation_answers(true).await;
}
}
