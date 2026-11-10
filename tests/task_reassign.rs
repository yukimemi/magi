mod common;

use common::{Judges, fixture, home_lock};
use magi::{
    config::Config,
    queue::{Queue, Source, Task, TaskStatus},
    run::{RunState, RunStatus},
};
use std::path::{Path, PathBuf};

fn git(repo: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn publish_config(repo: &Path, config: &Config) {
    std::fs::write(repo.join("magi.toml"), toml::to_string(config).unwrap()).unwrap();
    git(repo, &["add", "magi.toml"]);
    git(repo, &["commit", "-m", "configure fixture"]);
    git(repo, &["push", "origin", "main"]);
    git(repo, &["fetch", "origin"]);
}

fn destination(f: &common::Fixture) -> PathBuf {
    let repo = f.tmp.path().join("destination");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    let remote = f.tmp.path().join("destination.git");
    git(
        f.tmp.path(),
        &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
    );
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    std::fs::write(repo.join("README.md"), "destination\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init destination"]);
    let mut config = f.config.clone();
    config.graph.implementers = 1;
    config.graph.reviewers = 1;
    config.graph.judges = 1;
    config.graph.advise = false;
    config.graph.land = false;
    config.coordination = Some(magi::config::Coordination {
        contract_file: "scope-contract".into(),
        contract_token: "destination".into(),
        lock_file: "scope-lock".into(),
    });
    std::fs::write(repo.join(".git/scope-contract"), "destination\n").unwrap();
    publish_config(&repo, &config);
    repo.canonicalize().unwrap()
}

fn stopped(f: &common::Fixture, queue: &Queue) -> (Task, RunState) {
    let mut run = RunState::new(
        f.repo.clone(),
        "main".into(),
        "base".into(),
        "add a note".into(),
        f.config.clone(),
    );
    run.status = RunStatus::Blocked;
    run.driver_exited = true;
    run.reserve().unwrap();
    let mut task = Task::new(
        "note".into(),
        "add a note".into(),
        f.repo.clone(),
        Source::Human,
    );
    task.chat = Some("original-conversation".into());
    task.start(run.id.clone());
    task.hold_manual(Some("investigation complete".into()));
    task.answers.push(magi::queue::AnsweredQuestion {
        question: "where?".into(),
        answer: "another repository".into(),
    });
    queue.put(&mut task).unwrap();
    (task, run)
}

common::e2e! {
async fn reassign_preserves_history_and_executes_with_destination_config_and_scope() {
    let f = fixture(home_lock().await, Judges::Unanimous, false);
    publish_config(&f.repo, &f.config);
    let dest = destination(&f);
    let home = magi::run::home();
    let queue = Queue::open();
    let (original, old) = stopped(&f, &queue);
    git(&f.repo, &["branch", "retained-evidence"]);
    let old_json = std::fs::read(home.join("runs").join(&old.id).join("run.json")).unwrap();
    let stale_snapshot = original.clone();
    let moved = magi::task_reassign::reassign(&queue, &home, &f.repo, &original.id, &dest, "test").await.unwrap();
    assert_eq!(moved.id, original.id);
    assert_eq!(moved.instruction, original.instruction);
    assert_eq!(moved.chat, original.chat);
    assert_eq!(moved.answers, original.answers);
    assert_eq!(moved.runs, original.runs);
    assert_eq!(moved.status, TaskStatus::Held);
    assert!(moved.execution_runs().is_empty());
    assert!(moved.earlier_attempts().is_empty());
    let mut stale_snapshot = stale_snapshot;
    assert!(queue.put(&mut stale_snapshot).unwrap_err().to_string().contains("assignment changed"));
    assert!(magi::chat_control::resume(&queue, &home, &old.id).is_err());
    assert!(magi::graph::Runner::resume(&old.id).is_err());

    let mut released = magi::chat_control::release(&queue, &original.id).unwrap();
    let _claim = queue.claim(&released.id).unwrap();
    magi::daemon::run_claimed(&magi::daemon::Opts { repo: f.repo.clone(), ..Default::default() }, &queue, &mut released).await;
    assert_eq!(released.runs.len(), 2, "{released:?}");
    let new = RunState::load(released.runs.last().unwrap()).unwrap();
    assert_eq!(new.repo, dest);
    assert_eq!(new.status, RunStatus::Ready, "{new:?}");
    assert_eq!(new.base_commit, magi::git::rev_parse(&dest, "origin/main").await.unwrap());
    assert_eq!(new.config.graph.reviewers, 1);
    assert_eq!(new.config.coordination.as_ref().unwrap().contract_token, "destination");
    assert_eq!(new.candidates.len(), 1);
    assert_eq!(magi::git::toplevel(&new.candidates[0].worktree).await.unwrap(), new.candidates[0].worktree.canonicalize().unwrap());
    let common = magi::git::git_raw(&new.candidates[0].worktree, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await.unwrap();
    assert_eq!(PathBuf::from(common.stdout.trim()).canonicalize().unwrap(), dest.join(".git").canonicalize().unwrap());
    assert!(dest.join(".git/scope-lock").exists());
    assert!(!f.repo.join(".git/scope-lock").exists());
    assert_eq!(std::fs::read(home.join("runs").join(&old.id).join("run.json")).unwrap(), old_json);
    assert!(magi::git::branch_exists(&f.repo, "retained-evidence").await.unwrap());
}

}

common::e2e! {
async fn reassign_refuses_claims_live_unknown_and_active_seats_atomically() {
    let f = fixture(home_lock().await, Judges::Unanimous, false);
    let dest = destination(&f);
    let home = magi::run::home();
    let queue = Queue::open();
    let (task, mut run) = stopped(&f, &queue);
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    let claim = queue.claim(&task.id).unwrap();
    assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.is_err());
    drop(claim);
    run.driver_exited = false;
    run.driver_pid = None;
    run.driver_claimed = None;
    run.save().unwrap();
    let error = magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.unwrap_err();
    assert!(error.to_string().contains("live or unknown"), "{error:#}");
    run.driver_pid = Some(std::process::id());
    run.driver_started_at = magi::proc::process_started_at(std::process::id());
    run.save().unwrap();
    assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.is_err());
    run.driver_exited = true;
    run.active.insert("seat".into(), magi::run::ActiveSeat {
        node: "implement".into(), started_at: jiff::Timestamp::now(), timeout_secs: 600,
        attempt: 0, task: None, command: None, index: None, total: None,
    });
    run.save().unwrap();
    assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.unwrap_err().to_string().contains("active seats"));
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        run.active.clear();
        run.save().unwrap();
        let worktree = run.worktree_root().join("retained-seat");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        git(&f.repo, &["worktree", "add", "--detach", worktree.to_str().unwrap(), "HEAD"]);
        let dir = magi::git::git_raw(&worktree, &["rev-parse", "--absolute-git-dir"]).await.unwrap();
        let lease = std::fs::OpenOptions::new().create(true).truncate(false).write(true)
            .open(PathBuf::from(dir.stdout.trim()).join("magi-output.lock")).unwrap();
        // A surviving tool keeps the inherited output reservation after its CLI exits.
        // SAFETY: the live file owns a valid descriptor for the nonblocking lock call.
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) }, 0);
        assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.unwrap_err().to_string().contains("still reserved"));
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        drop(lease);
        assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.is_ok());
    }
}

}

common::e2e! {
async fn reassign_cli_uses_filing_selectors_and_stale_questions_cannot_resume() {
    let f = fixture(home_lock().await, Judges::Unanimous, false);
    let dest = destination(&f);
    let home = magi::run::home();
    let queue = Queue::open();
    let (task, old) = stopped(&f, &queue);
    let mut config = f.config.clone();
    let root = f.tmp.path().join("repos");
    for owner in ["one", "two"] {
        let parent = root.join("host").join(owner);
        std::fs::create_dir_all(&parent).unwrap();
        git(&parent, &["clone", dest.to_str().unwrap(), "same"]);
    }
    config.repos.roots = vec![root];
    publish_config(&f.repo, &config);
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    for selector in ["missing-repository", "same"] {
        let output = common::magi_cli(&home).current_dir(&f.repo)
            .args(["task", "reassign", &task.id, "--repo", selector]).output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(if selector == "same" { "matches 2" } else { "does not exist" }), "{error}");
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
    }
    let questions = magi::ask::Questions::at(home.join("questions"));
    let mut q = magi::ask::Question::new(old.id.clone(), "implement".into(), "seat".into(), "continue?".into(), "evidence".into(), vec!["resume".into()]);
    q.actions.insert("resume".into(), magi::ask::ChoiceAction::Resume { run: old.id.clone() });
    questions.put(&mut q).unwrap();
    let mut conductor = magi::ask::Question::new(task.id.clone(), magi::conduct::NODE.into(), "seat".into(), "retry?".into(), "previous scope".into(), vec!["retry".into()]);
    conductor.actions.insert("retry".into(), magi::ask::ChoiceAction::Requeue);
    questions.put(&mut conductor).unwrap();
    let output = common::magi_cli(&home).current_dir(&f.repo)
        .args(["task", "reassign", &task.id, "--repo", "one/same"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    magi::ask::record_answer(&questions, &q.id, "resume", None).unwrap();
    magi::ask::record_answer(&questions, &conductor.id, "retry", None).unwrap();
    magi::daemon::apply_choice_actions(&queue, &questions, &home).await;
    let moved = queue.get(&task.id).unwrap();
    assert_eq!(moved.status, TaskStatus::Held);
    assert!(moved.resume_override.is_none());
    magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &f.repo, "test").await.unwrap();
    assert!(!queue.get(&task.id).unwrap().execution_run(&old.id));
    let output = common::magi_cli(&home).current_dir(&f.repo).args(["run", "--resume", &old.id]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("previous repository assignment"));
}
}

common::e2e! {
async fn reassign_preserves_idle_transitions_and_save_failure_is_atomic() {
    let f = fixture(home_lock().await, Judges::Unanimous, false);
    let dest = destination(&f);
    let home = magi::run::home();
    let queue = Queue::open();
    for (index, status) in [TaskStatus::Queued, TaskStatus::Held, TaskStatus::Failed, TaskStatus::Parked].into_iter().enumerate() {
        let mut task = Task::new(format!("independent {index}"), format!("independent instruction {index}"), f.repo.clone(), Source::Human);
        task.status = status;
        queue.put(&mut task).unwrap();
        let moved = magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.unwrap();
        assert_eq!(moved.status, if status == TaskStatus::Queued { TaskStatus::Queued } else { TaskStatus::Held });
        let mut done = moved;
        done.succeed();
        queue.put(&mut done).unwrap();
    }
    let (task, mut run) = stopped(&f, &queue);
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    run.pr_publication_pending = true;
    run.save().unwrap();
    assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.unwrap_err().to_string().contains("receipt"));
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
    run.pr_publication_pending = false;
    run.pr = Some(magi::run::PrRecord {
        url: "https://example.invalid/pull/1".into(), number: 1, state: "open".into(), checks: "unknown".into(), round: 0, rounds: 1, red_at_merge: vec![],
    });
    run.save().unwrap();
    let old_run = std::fs::read(home.join("runs").join(&run.id).join("run.json")).unwrap();
    assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.unwrap_err().to_string().contains("example.invalid/pull/1"));
    assert_eq!(std::fs::read(home.join("runs").join(&run.id).join("run.json")).unwrap(), old_run);
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
    run.pr = None;
    run.save().unwrap();
    let tmp = queue.path_of(&task.id).with_extension("json.tmp");
    std::fs::create_dir(&tmp).unwrap();
    assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.is_err());
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
    std::fs::remove_dir(tmp).unwrap();
    assert!(magi::task_reassign::reassign(&queue, &home, &f.repo, &task.id, &dest, "test").await.is_ok());
}
}

common::e2e! {
async fn chat_reassign_requires_control_channel_without_store_fallback() {
    let f = fixture(home_lock().await, Judges::Unanimous, false);
    let home = magi::run::home();
    let queue = Queue::open();
    let (task, _) = stopped(&f, &queue);
    let talks = magi::talk::Talks::at(home.join("talks"));
    let mut talk: magi::talk::Talk = serde_json::from_value(serde_json::json!({
        "schema":1,"id":"conversation","repo":f.repo,"agent":"worker","status":"open","turns":[],
        "created_at":jiff::Timestamp::now(),"updated_at":jiff::Timestamp::now(),
        "seat":magi::agent::SeatState::new("talk","worker",1)
    })).unwrap();
    talks.put(&mut talk).unwrap();
    let before = std::fs::read(queue.path_of(&task.id)).unwrap();
    let output = common::magi_cli(&home).current_dir(&f.repo)
        .env("MAGI_RUN", &talk.id).env("MAGI_NODE", "chat").env("MAGI_ALLOW_WRITE", "1")
        .env_remove("MAGI_CHAT_CONTROL_FD").env_remove("MAGI_CHAT_CONTROL_PIPE")
        .args(["task", "control", "reassign", &task.id, "--repo", "missing"])
        .output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("authenticated chat control channel"), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
}
}
