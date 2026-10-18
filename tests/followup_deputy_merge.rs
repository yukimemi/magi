//! End to end: the follow-up tasks the merge approval's deputy filed are
//! released only once the forge confirms the merge, through the real
//! `land::correct_manual_merge` entry point, and a second pass after the run
//! is reloaded from disk changes nothing.
//!
//! `gh` is a shell script on `PATH`; the pull request's state is read from a
//! file the test flips from `OPEN` to `MERGED`. Anything it does not know
//! exits 1, so a stray call can never reach a real forge.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};

use common::{Fixture, Judges, e2e, fixture, home_lock};
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{ReviewRecord, ReviewRound, RunState};
use magi::verdict::{Finding, ReviewVote, Severity};
use magi::{followup, land};

const PR: &str = "https://github.com/example/repo/pull/7";

const FAKE_GH: &str = r#"#!/bin/sh
{
  printf '%s\n' '--call--'
  for a in "$@"; do printf '%s\n' "$a"; done
} >> "$FAKE_GH_LOG"
case "$1 $2" in
  "repo view") echo '{"url":"https://github.com/example/repo"}' ;;
  "pr view")
    state=$(cat "$FAKE_GH_STATE")
    printf '{"url":"%s","number":7,"state":"%s","title":"t","reviews":[],"comments":[],"mergeStateStatus":"CLEAN","headRefOid":"abc","baseRefName":"main"}\n' \
      "https://github.com/example/repo/pull/7" "$state" ;;
  "pr comment") ;;
  *) echo "fake gh: unsupported: $*" >&2; exit 1 ;;
esac
"#;

/// Put the fake `gh` first on `PATH`; returns its call log and state file.
fn install_fake_gh(dir: &Path) -> (PathBuf, PathBuf) {
    let bin = dir.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(&script, FAKE_GH).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.join("gh.log");
    std::fs::write(&log, "").unwrap();
    let state = dir.join("gh.state");
    std::fs::write(&state, "OPEN").unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&path));
    // SAFETY: this runs only inside the isolated child process `e2e!` spawns,
    // before the body starts any other thread that reads the environment.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("FAKE_GH_LOG", &log);
        std::env::set_var("FAKE_GH_STATE", &state);
    }
    (log, state)
}

/// How many `gh pr comment` calls were made.
fn comments(log: &Path) -> usize {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines
        .windows(3)
        .filter(|w| w[0] == "--call--" && w[1] == "pr" && w[2] == "comment")
        .count()
}

fn finding() -> Finding {
    Finding {
        id: "R3-1-1".to_owned(),
        severity: Severity::Major,
        file: Some("src/waiter.rs".to_owned()),
        line: Some(211),
        title: "a defect nobody fixed".to_owned(),
        detail: "detail".to_owned(),
    }
}

fn round() -> ReviewRound {
    ReviewRound {
        round: 3,
        head: "deadbee".to_owned(),
        verified_head: None,
        verified_at: None,
        reviews: vec![ReviewRecord {
            attempts: 0,
            reviewer: 1,
            agent: "alpha".to_owned(),
            summary: String::new(),
            findings: vec![finding()],
            vote: Some(ReviewVote::Reject),
            failed: None,
            duration_ms: 0,
        }],
        e2e: Vec::new(),
        verify_retried: false,
        e2e_deferred: false,
        e2e_defer_reason: None,
        fix: None,
        blocking: 1,
        answered: 1,
        expected: 1,
        clean: false,
        progressed: false,
        vote_split: false,
        reconsideration: Vec::new(),
        verdict: None,
    }
}

fn deputy_task(run: &str, repo: &Path, title: &str, text: &str) -> Task {
    let mut t = Task::new(
        title.to_owned(),
        text.to_owned(),
        repo.to_path_buf(),
        Source::Agent {
            run: run.to_owned(),
            node: magi::deputy::NODE.to_owned(),
        },
    );
    t.hold_manual(Some(format!("after {PR}")));
    t
}

struct Scene {
    id: String,
    unrelated: String,
    matching: String,
    bystander: String,
    log: PathBuf,
    state_file: PathBuf,
}

/// A run that ended with a Major finding open, two deputy tasks held for the
/// pull request (one about that finding, one not) and an unrelated held task.
fn scene(fx: &Fixture, file_followups: bool, release: bool) -> Scene {
    let (log, state_file) = install_fake_gh(fx.tmp.path());
    let mut config = fx.config.clone();
    config.graph.file_followups = file_followups;
    config.graph.release_deputy_followups = release;
    let mut s = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        "abc1234".to_owned(),
        "do it".to_owned(),
        config,
    );
    s.followup_generation = Some(0);
    s.reviews.push(round());
    s.save().unwrap();

    let q = Queue::open();
    let mut unrelated = deputy_task(&s.id, &fx.repo, "Unrelated work", "do the thing");
    let mut matching = deputy_task(
        &s.id,
        &fx.repo,
        "Fix the waiter",
        "look at src/waiter.rs:212",
    );
    let mut bystander = Task::new(
        "Held by hand".to_owned(),
        "x".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    bystander.hold_manual(Some(format!("after {PR}")));
    for t in [&mut unrelated, &mut matching, &mut bystander] {
        q.put(t).unwrap();
    }
    Scene {
        id: s.id.clone(),
        unrelated: unrelated.id,
        matching: matching.id,
        bystander: bystander.id,
        log,
        state_file,
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Both deputy tasks: with no automatic follow-up to duplicate, each is released.
fn both(sc: &Scene) -> Vec<String> {
    sorted(vec![sc.unrelated.clone(), sc.matching.clone()])
}

async fn correct(id: &str) -> anyhow::Result<()> {
    let mut state = RunState::load(id)?;
    land::correct_manual_merge(&mut state, PR).await.map(|_| ())
}

e2e! {
async fn a_deputy_task_is_released_only_after_the_forge_confirms_the_merge() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let sc = scene(&fx, false, true);
    let q = Queue::open();

    // Not merged yet: refused, and nothing in the queue or the run moves.
    let err = correct(&sc.id).await.expect_err("an open pull request is refused");
    assert!(format!("{err:#}").contains("not merged"), "{err:#}");
    for id in [&sc.unrelated, &sc.matching, &sc.bystander] {
        assert_eq!(q.get(id).unwrap().status, TaskStatus::Held);
    }
    assert!(RunState::load(&sc.id).unwrap().deputy_followups.is_empty());
    assert_eq!(comments(&sc.log), 0);

    // The forge says merged: the unrelated task is released and stamped.
    std::fs::write(&sc.state_file, "MERGED").unwrap();
    correct(&sc.id).await.expect("merged pull request is recorded");
    let released = q.get(&sc.unrelated).unwrap();
    assert_eq!(released.status, TaskStatus::Queued);
    let stamp = released.followup.clone().expect("stamped");
    assert_eq!((stamp.run.as_str(), stamp.generation), (sc.id.as_str(), 1));
    assert_eq!(q.get(&sc.matching).unwrap().status, TaskStatus::Queued);
    assert_eq!(q.get(&sc.bystander).unwrap().status, TaskStatus::Held);
    assert!(q.get(&sc.bystander).unwrap().followup.is_none());

    // What was saved is what a restarted process reads back.
    let reloaded = RunState::load(&sc.id).unwrap();
    assert_eq!(sorted(reloaded.deputy_followups), both(&sc));
    assert!(reloaded.followup_commented);
    assert_eq!(comments(&sc.log), 1);

    let before: Vec<Task> = {
        let mut v = q.list();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    };

    // A second pass, again from the disk record, is a no-op.
    correct(&sc.id).await.expect("second pass");
    let after: Vec<Task> = {
        let mut v = q.list();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    };
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    assert_eq!(
        sorted(RunState::load(&sc.id).unwrap().deputy_followups),
        both(&sc)
    );
    assert_eq!(comments(&sc.log), 1, "the comment is posted once");
}
}

e2e! {
async fn turning_the_release_off_keeps_deputy_tasks_held_after_the_merge() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let sc = scene(&fx, false, false);
    let q = Queue::open();
    std::fs::write(&sc.state_file, "MERGED").unwrap();
    correct(&sc.id).await.expect("merged pull request is recorded");
    for id in [&sc.unrelated, &sc.matching, &sc.bystander] {
        let t = q.get(id).unwrap();
        assert_eq!(t.status, TaskStatus::Held, "{id}");
        assert!(t.followup.is_none(), "{id}");
    }
    assert!(RunState::load(&sc.id).unwrap().deputy_followups.is_empty());
}
}

e2e! {
async fn a_deputy_task_is_superseded_by_the_automatic_followup_when_release_is_off() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let sc = scene(&fx, true, false);
    let q = Queue::open();
    std::fs::write(&sc.state_file, "MERGED").unwrap();
    correct(&sc.id).await.expect("merged pull request is recorded");

    let automatic: Vec<_> = q
        .list()
        .into_iter()
        .filter(|t| t.followup.as_ref().is_some_and(|f| f.run == sc.id))
        .collect();
    assert_eq!(automatic.len(), 1, "one automatic follow-up was filed");
    let auto = &automatic[0];
    assert_ne!(auto.id, sc.matching);
    assert_eq!(auto.status, TaskStatus::Queued);
    let matching = q.get(&sc.matching).unwrap();
    assert_eq!(matching.status, TaskStatus::Held);
    assert!(
        matching.hold_reason.as_deref().is_some_and(|r| r.starts_with("[superseded] ")),
        "{:?}",
        matching.hold_reason
    );
    assert_eq!(q.get(&sc.unrelated).unwrap().status, TaskStatus::Held);

    // A second pass leaves the reason alone.
    let reason = matching.hold_reason.clone();
    correct(&sc.id).await.expect("second pass");
    assert_eq!(q.get(&sc.matching).unwrap().hold_reason, reason);
    assert_eq!(q.list().len(), 4, "the second pass files no duplicate");
}
}

e2e! {
async fn a_restart_recovers_deputy_releases_written_before_the_run_was_saved() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let sc = scene(&fx, true, true);
    std::fs::write(&sc.state_file, "MERGED").unwrap();

    // Confirm the merge, then stop between the queue write and the caller's
    // run save. Only the queue remembers which deputy tasks were released.
    {
        let mut state = RunState::load(&sc.id).unwrap();
        land::land(&mut state, PR).await.expect("confirmed merge");
        followup::settle_deputy_tasks(&mut state, PR, &Queue::open(), true);
        assert_eq!(sorted(state.deputy_followups), both(&sc));
    }

    let mut state = RunState::load(&sc.id).unwrap();
    assert!(state.deputy_followups.is_empty());
    let q = Queue::open();
    for id in [&sc.unrelated, &sc.matching] {
        let task = q.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::Queued);
        let stamp = task.followup.unwrap();
        assert_eq!(stamp.run, sc.id);
        assert_eq!(stamp.pr, PR);
        assert_eq!(stamp.generation, 1);
    }
    let before = serde_json::to_value(q.list()).unwrap();

    followup::after_merge(&mut state, PR).await;
    assert_eq!(sorted(state.deputy_followups.clone()), both(&sc));
    assert!(state.followups.is_empty(), "the deputy already covers the finding");
    assert!(state.followup_commented);
    state.save().unwrap();
    assert_eq!(serde_json::to_value(Queue::open().list()).unwrap(), before);
    assert_eq!(comments(&sc.log), 1);

    // Restart again through the normal entry point: recovery was persisted,
    // and neither queue entries nor the follow-up comment are duplicated.
    correct(&sc.id).await.expect("second restart");
    assert_eq!(sorted(RunState::load(&sc.id).unwrap().deputy_followups), both(&sc));
    assert_eq!(serde_json::to_value(Queue::open().list()).unwrap(), before);
    assert_eq!(comments(&sc.log), 1);
}
}
