//! A run parked on a land-merge approval must not hold the daemon's slot.
//!
//! Reproduces the incident this change exists for at the daemon level, not
//! just inside `land`: a task whose run is sitting in `land`'s approval wait
//! must not stop a different, genuinely runnable task from being attempted
//! and finished, and once the operator answers, the daemon must pick the
//! landing task back up on its own — no `magi task release` required.

mod common;

use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use common::{Judges, fixture, home_lock};
use magi::ask::{self, Answer};
use magi::config::MergeMode;
use magi::daemon::{self, Opts};
use magi::land::APPROVAL_NODE;
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{Candidate, CommandOutcome, MergeOutcome, ReviewRound, RunState, RunStatus, Tally};

const PR_URL: &str = "https://example.invalid/x/y/pull/1";
const GH_REJECTION: &str = "Unrecognized option: 'json'";

/// A native executable on both Unix and Windows, with no shell or network
/// dependency. Libtest rejects `gh pr view ... --json ...` before running any
/// tests. The fixture must not accidentally call an installed, authenticated
/// gh: an invalid URL alone does not bound its network/credential-helper waits.
#[derive(Debug)]
struct UnavailableGh {
    _dir: tempfile::TempDir,
    executable: std::path::PathBuf,
}

impl UnavailableGh {
    fn install() -> Self {
        let dir = tempfile::tempdir().expect("temporary gh directory");
        let executable = dir.path().join(if cfg!(windows) { "gh.exe" } else { "gh" });
        std::fs::copy(std::env::current_exe().unwrap(), &executable)
            .expect("copy native test binary");
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        // SAFETY: called only by the isolated test's setup, before its async
        // body starts any tasks or subprocesses. Each test owns its process.
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
        Self {
            _dir: dir,
            executable,
        }
    }

    async fn assert_selected(&self, repo: &std::path::Path) {
        // Exercise the same PATH lookup, cwd and option that land uses,
        // including Windows' executable search, rather than just invoking
        // the absolute path we copied to.
        let output = tokio::process::Command::new("gh")
            .args(["pr", "view", PR_URL, "--json", "url"])
            .current_dir(repo)
            .output()
            .await
            .expect("spawn fixture gh through PATH");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(GH_REJECTION),
            "fixture gh {} must reject --json: {output:?}",
            self.executable.display()
        );
    }
}

// Run setup inside common's isolated child, before the async test body. PATH
// must never change in the parent, where libtest can run tests concurrently.
macro_rules! approval_test {
    (async fn $name:ident($gh:ident) $body:block) => {
        #[test]
        fn $name() {
            common::isolated(stringify!($name), || {
                let $gh = UnavailableGh::install();
                async move $body
            });
        }
    };
}

/// Write `config` to a `magi.toml` under `dir` and hand back the path, so
/// `Opts::config` can point the daemon at it explicitly rather than at
/// whatever layer stack `fx.repo` would otherwise resolve.
fn write_config(dir: &std::path::Path, config: &magi::config::Config) -> std::path::PathBuf {
    let path = dir.join("magi.toml");
    std::fs::write(&path, toml::to_string(config).expect("serialize config")).unwrap();
    path
}

/// Keep the ordinary task small while still taking the real implement,
/// review, verification and completion path. The scheduling contract under
/// test is between that runnable task and the parked approval; a larger panel
/// does not add coverage, but on Windows it adds process and worktree churn
/// while this test is holding the process-wide test home lock.
fn daemon_config(fx: &common::Fixture) -> magi::config::Config {
    let mut config = fx.config.clone();
    config.graph.implementers = 1;
    config.graph.reviewers = 1;
    config.disk.min_free_bytes = 0;
    // `serve --once` runs the janitor both before and after its drain. Its
    // real housekeeping is covered elsewhere; scanning/pruning a brand-new
    // fixture's worktree bay and cache cannot affect this scheduling decision
    // and needlessly starts more Windows processes while the test home is
    // intentionally serialized.
    config.disk.auto_fold = false;
    config.disk.cache_limit_bytes = 0;
    config
}

/// Preserve a bounded hang guard, but make a failure identify whether the
/// daemon was still advancing, which task/run owned its slot, and what the
/// persisted run states said when the guard fired. The daemon status path is
/// inside this fixture's locked, temporary home.
async fn serve_once(
    opts: Opts,
    queue: &Queue,
    task_ids: &[&str],
    scenario: &str,
    gh: &UnavailableGh,
) {
    const HANG_GUARD: Duration = Duration::from_secs(60);

    let started = Instant::now();
    match tokio::time::timeout(HANG_GUARD, daemon::serve(opts)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("{scenario}: daemon loop failed: {error:#}"),
        Err(_) => {
            let tasks: Vec<_> = task_ids
                .iter()
                .map(|id| {
                    queue.get(id).map_or_else(
                        |error| format!("{id}: {error:#}"),
                        |task| format!("{task:?}"),
                    )
                })
                .collect();
            let runs: Vec<_> = task_ids
                .iter()
                .filter_map(|id| queue.get(id).ok())
                .flat_map(|task| task.runs)
                .map(|id| match RunState::load(&id) {
                    Ok(run) => format!(
                        "{id}: status={:?}, parked={}, active={:?}, pr={:?}, merge={:?}, \
                         tail_events={:?}, questions={:?}",
                        run.status,
                        run.parked,
                        run.active,
                        run.pr,
                        run.merge,
                        run.events.iter().rev().take(8).collect::<Vec<_>>(),
                        ask::Questions::open()
                            .list()
                            .into_iter()
                            .filter(|q| q.run == id)
                            .map(|q| format!(
                                "{}: node={}, status={:?}, resolution={:?}",
                                q.id,
                                q.node,
                                q.status,
                                q.resolution()
                            ))
                            .collect::<Vec<_>>()
                    ),
                    Err(error) => format!("{id}: {error:#}"),
                })
                .collect();
            let status_path = daemon::status_path();
            let status = std::fs::read_to_string(&status_path)
                .unwrap_or_else(|error| format!("unreadable {}: {error}", status_path.display()));
            panic!(
                "{scenario}: daemon exceeded the {}s hang guard after {:?}; \
                 status at {}: {status}; tasks: {tasks:#?}; runs: {runs:#?}; \
                 fixture gh: {gh:?}; expected rejection: {GH_REJECTION}",
                HANG_GUARD.as_secs(),
                started.elapsed(),
                status_path.display(),
            );
        }
    }
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
        verified_noop: None,
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
        verified_head: None,
        verified_at: None,
        reviews: Vec::new(),
        e2e: Vec::new(),
        fix: None,
        blocking: 0,
        answered: 0,
        expected: 0,
        clean: true,
        verify_retried: false,
        e2e_deferred: false,
        e2e_defer_reason: None,
        progressed: false,
        vote_split: false,
        reconsideration: Vec::new(),
        verdict: None,
    }];
    state.gate = vec![CommandOutcome {
        command: "test".to_owned(),
        code: Some(0),
        output_tail: String::new(),
        duration_ms: 0,
        resource_blocked: false,
    }];
    state.merge = Some(MergeOutcome {
        mode: MergeMode::Pr,
        ok: true,
        detail: PR_URL.to_owned(),
        empty: false,
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

approval_test! {
async fn a_task_parked_on_land_approval_does_not_block_another_runnable_task(gh) {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    gh.assert_selected(&fx.repo).await;

    let config = daemon_config(&fx);
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
    store
        .put(&mut question)
        .expect("file the approval question");

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
        // `once: true` drains to idle and back out, and the idle branch runs
        // the janitor unconditionally. Left at the default `None` this would
        // have it reclaim worktrees under the operator's real
        // `crate::run::default_worktree_root()` on every run of this test,
        // exactly like `fx.config.graph.worktree_root` already keeps a run's
        // own worktrees off that same real directory.
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    serve_once(
        opts,
        &queue,
        &[&task_a.id, &task_b.id],
        "the drain must not hang on the parked task",
        &gh,
    )
    .await;

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
    assert_eq!(
        after_b.runs,
        vec![run_b.id.clone()],
        "no second run was started"
    );

    let run_after = RunState::load(&run_b.id).expect("the parked run's history survives");
    assert_eq!(
        run_after.status,
        RunStatus::Landing,
        "still parked, exactly where it was left"
    );
    assert!(run_after.parked);
}
}

approval_test! {
async fn once_the_approval_answers_the_daemon_resumes_the_run_on_its_own(gh) {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    gh.assert_selected(&fx.repo).await;

    let config = daemon_config(&fx);
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
    store
        .put(&mut question)
        .expect("file the answered question");

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
        // `once: true` drains to idle and back out, and the idle branch runs
        // the janitor unconditionally. Left at the default `None` this would
        // have it reclaim worktrees under the operator's real
        // `crate::run::default_worktree_root()` on every run of this test,
        // exactly like `fx.config.graph.worktree_root` already keeps a run's
        // own worktrees off that same real directory.
        worktrees_root: Some(fx.tmp.path().join("wt")),
        ..Opts::default()
    };
    serve_once(
        opts,
        &queue,
        &[&task.id],
        "the answered approval must resume without hanging",
        &gh,
    )
    .await;

    // The run was resumed through `Runner::resume`, not recompeted: the same
    // run id is still the task's only run, and `land` was re-entered - proven
    // by the status moving on from `Landing` rather than staying parked
    // forever with an answer nobody acted on. The fixture gh rejects the
    // observation deterministically, so the outcome must be `Blocked` with
    // that rejection recorded, proving magi *tried*, unattended.
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
    assert_eq!(
        run_after.status,
        RunStatus::Blocked,
        "an answered approval must not leave the run parked forever: {:?} \
         (task last_error: {:?})",
        run_after.status,
        after.last_error
    );
    assert!(!run_after.parked, "the approval wait was cleared");
    assert!(run_after.pr.is_none(), "no PR observation succeeded");
    assert!(
        run_after.events.iter().any(|e| e.node == "land"
            && e.message.contains(&format!("gave up: gh pr view {PR_URL}:"))
            && e.message.contains(GH_REJECTION)),
        "the resumed land must record the fixture gh's rejection: {:?}",
        run_after.events
    );
}
}
