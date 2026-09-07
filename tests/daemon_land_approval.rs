//! A run parked on a land-merge approval must not hold the daemon's slot.
//!
//! Reproduces the incident this change exists for at the daemon level, not
//! just inside `land`: a task whose run is sitting in `land`'s approval wait
//! must not stop a different, genuinely runnable task from being attempted
//! and finished, and once the operator answers, the daemon must pick the
//! landing task back up on its own — no `magi task release` required.

mod common;

use std::time::Duration;

use std::collections::BTreeMap;

use common::{Judges, fixture, home_lock};
use magi::ask::{self, Answer};
use magi::config::MergeMode;
use magi::daemon::{self, Opts};
use magi::land::APPROVAL_NODE;
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{Candidate, CommandOutcome, MergeOutcome, ReviewRound, RunState, RunStatus, Tally};

/// Write `config` to a `magi.toml` under `dir` and hand back the path, so
/// `Opts::config` can point the daemon at it explicitly rather than at
/// whatever layer stack `fx.repo` would otherwise resolve.
fn write_config(dir: &std::path::Path, config: &magi::config::Config) -> std::path::PathBuf {
    let path = dir.join("magi.toml");
    std::fs::write(&path, toml::to_string(config).expect("serialize config")).unwrap();
    path
}

/// A run that already competed, reviewed and gated clean, pushed a pull
/// request, and is now sitting parked in `land`'s merge-approval wait -
/// exactly the state a real run reaches right before it would ask the
/// operator to confirm the merge. No real worktree is built: the daemon's
/// scheduling decision, which is what these tests check, only reads
/// `state.status`, `state.parked` and the approval question, never the
/// worktree.
fn parked_run(repo: &std::path::Path, config: &magi::config::Config) -> RunState {
    let mut state = RunState::new(
        repo.to_path_buf(),
        "main".to_owned(),
        "deadbeef".to_owned(),
        "land the winner".to_owned(),
        config.clone(),
    );
    state.candidates = vec![Candidate {
        index: 0,
        label: 'A',
        agent: "alpha".to_owned(),
        branch: "does-not-exist".to_owned(),
        worktree: repo.to_path_buf(),
        summary: String::new(),
        stat: String::new(),
        files: 0,
        // Non-zero: `implement`'s own resume check (`c.commits == 0 && ...`)
        // is what marks a candidate already done, and a fake `0` here would
        // make it try to implement this candidate for real.
        commits: 1,
        empty: false,
        failed: None,
        duration_ms: 0,
        folded: false,
    }];
    state.tally = Some(Tally {
        first_choice: BTreeMap::from([('A', 1)]),
        borda: BTreeMap::new(),
        winner: 'A',
        rankings: 1,
        unanimous_initial: true,
        deliberated: false,
        changed_votes: 0,
        unanimous_final: true,
        tie_break: None,
        judges: 0,
        present: 0,
        quorum: 0,
        met_quorum: true,
        uncontested: Some("only candidate A produced a change".to_owned()),
    });
    state.reviews = vec![ReviewRound {
        round: 1,
        head: "deadbeef".to_owned(),
        reviews: Vec::new(),
        e2e: Vec::new(),
        fix: None,
        blocking: 0,
        answered: 0,
        expected: 0,
        clean: true,
        verify_retried: false,
        progressed: false,
    }];
    state.gate = vec![CommandOutcome {
        command: "test".to_owned(),
        code: Some(0),
        output_tail: String::new(),
        duration_ms: 0,
    }];
    state.merge = Some(MergeOutcome {
        mode: MergeMode::Pr,
        ok: true,
        detail: "https://example.invalid/x/y/pull/1".to_owned(),
    });
    state.status = RunStatus::Landing;
    state.parked = true;
    state.save().expect("save parked run");
    state
}

fn approval_question(run: &str) -> ask::Question {
    ask::Question::new(
        run.to_owned(),
        APPROVAL_NODE.to_owned(),
        "land".to_owned(),
        "merge?".to_owned(),
        String::new(),
        vec!["merge".to_owned(), "hold".to_owned()],
    )
}

#[tokio::test]
async fn a_task_parked_on_land_approval_does_not_block_another_runnable_task() {
    let _home = home_lock().await;
    let fx = fixture(Judges::Unanimous, false);

    let mut config = fx.config.clone();
    config.graph.candidates = 1;
    // A single candidate makes `judge`/`deliberate`/`vote` skip their agent
    // calls entirely (see `Graph::candidates`'s doc), so this run only ever
    // needs the mock's implement and review branches.
    config.disk.min_free_bytes = 0;
    let config_path = write_config(fx.tmp.path(), &config);

    let queue = Queue::open();

    // Task A: an ordinary task with nothing standing in its way.
    let mut task_a = Task::new(
        "quick task".to_owned(),
        "create note.txt".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    queue.put(&mut task_a).expect("file task A");

    // Task B: its last run is parked on a land approval nobody has answered.
    let run_b = parked_run(&fx.repo, &config);
    let store = ask::Questions::open();
    let mut question = approval_question(&run_b.id);
    store.put(&mut question).expect("file the approval question");

    let mut task_b = Task::new(
        "landing task".to_owned(),
        "land the winner".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    // Matches what `settle`'s parked branch leaves behind: refunded, still
    // runnable, pointing at the run it belongs to.
    task_b.status = TaskStatus::Failed;
    task_b.runs.push(run_b.id.clone());
    queue.put(&mut task_b).expect("file task B");

    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config_path),
        once: true,
        poll: Duration::from_millis(20),
        ..Opts::default()
    };
    tokio::time::timeout(Duration::from_secs(60), daemon::serve(opts))
        .await
        .expect("the drain must not hang on the parked task")
        .expect("the loop itself must not error");

    let after_a = queue.get(&task_a.id).expect("task A still on disk");
    assert_eq!(
        after_a.status,
        TaskStatus::Done,
        "task A ran to completion while task B sat parked: {after_a:?}"
    );

    let after_b = queue.get(&task_b.id).expect("task B still on disk");
    assert_eq!(
        after_b.status,
        TaskStatus::Failed,
        "task B is untouched, not reclaimed as an orphan and not re-attempted \
         while the question is still open: {after_b:?}"
    );
    assert_eq!(after_b.runs, vec![run_b.id.clone()], "no second run was started");

    let run_after = RunState::load(&run_b.id).expect("the parked run's history survives");
    assert_eq!(
        run_after.status,
        RunStatus::Landing,
        "still parked, exactly where it was left"
    );
    assert!(run_after.parked);
}

#[tokio::test]
async fn once_the_approval_answers_the_daemon_resumes_the_run_on_its_own() {
    let _home = home_lock().await;
    let fx = fixture(Judges::Unanimous, false);

    let mut config = fx.config.clone();
    config.graph.candidates = 1;
    config.disk.min_free_bytes = 0;
    let config_path = write_config(fx.tmp.path(), &config);

    let queue = Queue::open();
    // The run's own saved config, not the daemon's `magi.toml`: a resumed
    // run reads `RunState::config`, never the layer stack a fresh
    // `Runner::start` would discover (`Runner::resume`'s whole point is that
    // a resumed run behaves like the original).
    let mut run_config = config.clone();
    run_config.merge.mode = MergeMode::Pr;
    let run = parked_run(&fx.repo, &run_config);
    let store = ask::Questions::open();
    let mut question = approval_question(&run.id);
    // The owner taps "merge" - the only interaction the operator's side of
    // this loop requires. Nothing here calls `magi task release`.
    question
        .answer(Answer::Choice("merge".to_owned()))
        .expect("record the approval");
    store.put(&mut question).expect("file the answered question");

    let mut task = Task::new(
        "landing task".to_owned(),
        "land the winner".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    task.status = TaskStatus::Failed;
    task.runs.push(run.id.clone());
    queue.put(&mut task).expect("file the task");

    let opts = Opts {
        repo: fx.repo.clone(),
        config: Some(config_path),
        once: true,
        poll: Duration::from_millis(20),
        ..Opts::default()
    };
    tokio::time::timeout(Duration::from_secs(60), daemon::serve(opts))
        .await
        .expect("the drain must not hang")
        .expect("the loop itself must not error");

    // The run was resumed through `Runner::resume`, not recompeted: the same
    // run id is still the task's only run, and `land` was re-entered - proven
    // by the status moving on from `Landing` rather than staying parked
    // forever with an answer nobody acted on. There is no real `gh` in this
    // sandbox, so the concrete outcome is `Blocked`, not `Merged`; what this
    // test cares about is that magi *tried*, unattended.
    // `Task::start` records a run id on every attempt, including a resume of
    // the same run - so a second entry here is expected. What must not
    // happen is a *different* id appearing, which is what a fresh
    // competition instead of a resume would leave behind.
    let after = queue.get(&task.id).expect("task still on disk");
    assert!(
        after.runs.iter().all(|r| r == &run.id),
        "the answered run was resumed, not recompeted from scratch: {:?} (last_error: {:?})",
        after.runs,
        after.last_error
    );

    let run_after = RunState::load(&run.id).expect("run readable");
    assert_ne!(
        run_after.status,
        RunStatus::Landing,
        "an answered approval must not leave the run parked forever: {:?} \
         (task last_error: {:?})",
        run_after.status,
        after.last_error
    );
    assert!(
        run_after
            .events
            .iter()
            .any(|e| e.node == "land"),
        "the resume actually re-entered `land`: {:?}",
        run_after.events
    );
}
