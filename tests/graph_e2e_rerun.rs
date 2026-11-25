//! A failed `verify.e2e` is run once more on the same commit before it is
//! believed. A failure that does not reproduce is a recorded, filed flake that
//! still verifies the head; one that reproduces stands, with the second run's
//! output; a build/link failure keeps its own single retry; and
//! `[graph] rerun_failed_e2e = false` restores the first-result-decides
//! behaviour.
//!
//! Every scenario runs real shell commands against the mock agent's fixture.
//! The commands keep their state outside the worktree (`runs`, a counter file
//! under the fixture's temp tree) so counting leaves the tree clean.
mod common;

use common::{Judges, fixture};
use magi::config::Config;
use magi::graph::Runner;
use magi::queue::{Queue, Source};
use magi::run::{E2eStatus, RunState, RunStatus};

/// `path` as a POSIX shell word: forward slashes, quoted.
fn sh(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\\', "/"))
}

/// Fails with a test-shaped failure on every odd invocation and passes on
/// every even one, counting invocations in `counter`. The line number in the
/// panic message differs per call, as a real flaky assertion's would.
fn fails_then_passes(counter: &std::path::Path) -> String {
    let c = sh(counter);
    format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $((n % 2)) -eq 1 ]; then \
         echo 'test web::tests::layout ... FAILED'; \
         echo \"thread 'web::tests::layout' panicked at src/web.rs:$n:9:\"; exit 1; fi"
    )
}

fn count(counter: &std::path::Path) -> usize {
    std::fs::read_to_string(counter)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn verify_events(state: &RunState) -> Vec<&str> {
    state
        .events
        .iter()
        .filter(|e| e.node == "verify")
        .map(|e| e.message.as_str())
        .collect()
}

fn flake_tasks() -> Vec<magi::queue::Task> {
    Queue::open()
        .list()
        .into_iter()
        .filter(|t| matches!(&t.source, Source::Agent { node, .. } if node == magi::flake::NODE))
        .collect()
}

async fn run_once(fx: &common::Fixture, config: Config) -> Runner {
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".to_owned(),
        config,
        magi::run::Origin::operator(),
    )
    .await
    .expect("start");
    runner.execute().await.expect("execute");
    runner
}

common::e2e! {
async fn a_flake_verifies_the_head_and_files_one_task_across_two_runs() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    let counter = fx.tmp.path().join("e2e-count");
    fx.config.verify.e2e = vec![fails_then_passes(&counter)];

    let first = run_once(&fx, fx.config.clone()).await;
    let state = &first.state;
    let round = state.reviews.last().expect("a round");
    assert_eq!(round.e2e_status(), E2eStatus::Passed, "{:?}", verify_events(state));
    assert!(round.clean, "{:?}", verify_events(state));
    assert_eq!(state.status, RunStatus::Ready, "{:?}", verify_events(state));
    assert_eq!(count(&counter), 2, "one failure, one rerun");

    // Recorded on the run, visibly, and never as a plain green.
    assert_eq!(state.verify_flakes.len(), 1);
    let flake = &state.verify_flakes[0];
    assert_eq!(flake.round, Some(round.round));
    assert_eq!(Some(flake.head.as_str()), round.verified_head.as_deref());
    assert!(flake.first_failure.contains("web::tests::layout"));
    assert!(flake.filed && flake.filing_error.is_none());
    let events = verify_events(state).join("\n");
    assert!(events.contains("once more on the same commit"), "{events}");
    assert!(events.contains("a flake, not a verdict on the change"), "{events}");
    let report = magi::report::run(state);
    assert!(report.contains("e2e green on rerun only"), "{report}");
    assert!(report.contains("flaky verification"), "{report}");

    // Exactly one task, asking for the root cause.
    let tasks = flake_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(Some(tasks[0].id.as_str()), flake.task.as_deref());
    assert!(tasks[0].instruction.contains("Find the root cause"));
    assert!(tasks[0].instruction.contains("web::tests::layout"));
    assert!(matches!(&tasks[0].source, Source::Agent { run, node }
        if run == &state.id && node == magi::flake::NODE));
    let task_id = tasks[0].id.clone();

    // A second run hitting the same failure leaves the one task alone and
    // records the occurrence on the run.
    let second = run_once(&fx, fx.config.clone()).await;
    let state = &second.state;
    assert_eq!(count(&counter), 4);
    assert_eq!(state.status, RunStatus::Ready, "{:?}", verify_events(state));
    assert_eq!(state.verify_flakes.len(), 1);
    assert!(!state.verify_flakes[0].filed);
    assert_eq!(state.verify_flakes[0].task.as_deref(), Some(task_id.as_str()));
    assert_eq!(flake_tasks().len(), 1, "ten runs, one task");
}
}

common::e2e! {
async fn a_failure_that_reproduces_stands_with_the_second_runs_output() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, true);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.implementers = 1;
    fx.config.graph.e2e_every_round = true;
    let runs = fx.tmp.path().join("e2e-runs");
    // Red with a confirmed libtest terminal failure until the fixer's marker file exists.
    fx.config.verify.e2e = vec![format!(
        "echo run >> {}; test -f fixed.txt || {{ \
         printf 'running 1 test\\ntest web::tests::layout ... FAILED\\n\\nfailures:\\n    web::tests::layout\\n\\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\\n'; \
         exit 1; }}",
        sh(&runs)
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    let r1 = &state.reviews[0];
    assert_eq!(r1.e2e_status(), E2eStatus::Failed);
    assert!(
        !r1.e2e[0]
            .output_tail
            .contains(magi::verify_evidence::LEGACY_FAILED_TWICE_PREFIX),
        "new runs must record structured comparison instead of legacy FAILED_TWICE injection"
    );
    assert!(r1.e2e[0].output_tail.contains("web::tests::layout"));
    let cmp = r1.e2e[0]
        .rerun_comparison
        .as_ref()
        .expect("double failure records a structured rerun comparison");
    assert_eq!(
        cmp.verdict,
        magi::verify_evidence::ComparisonVerdict::Matching {
            failures: vec![magi::verify_evidence::TerminalFailure {
                suite: None,
                test: "web::tests::layout".to_owned(),
            }]
        }
    );
    let art1 = cmp.first.artifact.as_deref().expect("attempt 1 artifact");
    let art2 = cmp.second.artifact.as_deref().expect("attempt 2 artifact");
    assert_ne!(art1, art2);
    assert!(state.dir().join(art1).exists());
    assert!(state.dir().join(art2).exists());

    // Reload from disk: structured comparison and both artifacts survive resume.
    let reloaded = RunState::load(&state.id).expect("reload");
    assert_eq!(reloaded.reviews[0].e2e[0].rerun_comparison.as_ref(), Some(cmp));

    // Fixer context carries the structured matching verdict and both artifact references.
    let summary = r1
        .verification_summary(&r1.head)
        .expect("a failing round has a summary");
    let tail = summary.tail.expect("failed output");
    assert!(tail.contains("matching terminal failure(s)"), "{tail}");
    assert!(tail.contains("web::tests::layout"), "{tail}");
    assert!(tail.contains(art1) && tail.contains(art2), "{tail}");

    let events = verify_events(state).join("\n");
    assert!(events.contains("round 1: rerun `"), "{events}");
    assert!(events.contains("matching terminal failure(s)"), "{events}");
    assert!(events.contains("the failure reproduced"), "{events}");
    assert!(state.verify_flakes.is_empty(), "a reproduced failure is not a flake");
    assert!(flake_tasks().is_empty());
    // Round 1 ran twice (failure, rerun); round 2 passed on its first run.
    let ran = std::fs::read_to_string(&runs).unwrap_or_default().lines().count();
    assert_eq!(ran, 3, "{events}");
}
}

common::e2e! {
async fn distinct_failures_a_then_b_stay_red_without_claiming_reproduction() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.implementers = 1;
    fx.config.graph.review_rounds = 1;
    let counter = fx.tmp.path().join("e2e-distinct-count");
    let c = sh(&counter);
    // Attempt 1 fails in libtest on a chat unit test; attempt 2 passes libtest and fails in
    // nextest on isolated_build_cache with intermediate TRY/FLAKY/cancellation noise.
    fx.config.verify.e2e = vec![format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $n -eq 1 ]; then \
           printf 'running 2 tests\\ntest ok_test ... ok\\ntest talk::tests::chat_timeout ... FAILED\\n\\nfailures:\\n\\n---- talk::tests::chat_timeout stdout ----\\ntimed out waiting for turn\\n\\nfailures:\\n    talk::tests::chat_timeout\\n\\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s\\n'; \
           exit 101; \
         else \
           printf 'running 2 tests\\ntest ok_test ... ok\\ntest talk::tests::chat_timeout ... ok\\n\\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s\\n'; \
           printf 'Starting 10 tests across 2 binaries\\n  TRY 1 FAIL [   0.300s] magi-cli::other test_flaky\\n   FLAKY 2/3 [   0.200s] magi-cli::other test_flaky\\nCancelling due to test failure: 2 tests still running\\n      CANCEL [   0.000s] magi-cli::web test_cancelled\\n------------\\n     Summary [   4.500s] 6/10 tests run: 5 passed (1 flaky), 1 failed, 0 skipped\\n        FAIL [   1.200s] magi-cli::isolated_build_cache test_cache\\nerror: test run failed\\n' >&2; \
           exit 100; \
         fi"
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.merge.is_none(), "publication stays blocked on red");
    assert_eq!(count(&counter), 2, "failed once, rerun once, then stopped");
    assert!(state.verify_flakes.is_empty());

    let r1 = &state.reviews[0];
    let cmp = r1.e2e[0]
        .rerun_comparison
        .as_ref()
        .expect("distinct comparison recorded");
    assert_eq!(
        cmp.verdict,
        magi::verify_evidence::ComparisonVerdict::Distinct {
            common: vec![],
            first_only: vec![magi::verify_evidence::TerminalFailure {
                suite: None,
                test: "talk::tests::chat_timeout".to_owned(),
            }],
            second_only: vec![magi::verify_evidence::TerminalFailure {
                suite: Some("magi-cli::isolated_build_cache".to_owned()),
                test: "test_cache".to_owned(),
            }],
        }
    );
    assert_eq!(cmp.second.intermediate.try_failure_count, 1);
    assert_eq!(cmp.second.intermediate.flaky_count, 1);
    assert!(cmp.second.intermediate.cancelled);
    assert_eq!(cmp.second.intermediate.not_run_count, Some(4));

    let art1 = cmp.first.artifact.as_deref().expect("attempt 1 artifact");
    let art2 = cmp.second.artifact.as_deref().expect("attempt 2 artifact");
    assert_ne!(art1, art2);
    assert!(state.dir().join(art1).exists());
    assert!(state.dir().join(art2).exists());

    // Event log, CLI report, web report, fixer prompt, and chat run_facts all report distinct
    // failures and never claim the failure reproduced.
    let events = verify_events(state).join("\n");
    assert!(events.contains("distinct terminal failure(s)"), "{events}");
    assert!(!events.contains("the failure reproduced"), "{events}");

    let cli_report = magi::report::run(state);
    assert!(cli_report.contains("distinct terminal failure(s)"), "{cli_report}");
    assert!(cli_report.contains("talk::tests::chat_timeout"), "{cli_report}");
    assert!(cli_report.contains("isolated_build_cache"), "{cli_report}");
    assert!(cli_report.contains(art1) && cli_report.contains(art2), "{cli_report}");

    let view = magi::report_view::build(state, magi::run::Liveness::Dead);
    let view_json = serde_json::to_string(&view).unwrap();
    assert!(view_json.contains("distinct terminal failure(s)"), "{view_json}");
    assert!(view_json.contains(art1) && view_json.contains(art2), "{view_json}");

    let fixer = r1
        .verification_summary(&r1.head)
        .expect("summary")
        .tail
        .expect("tail");
    assert!(fixer.contains("DIFFERENT terminal test failures"), "{fixer}");
    assert!(fixer.contains("talk::tests::chat_timeout"), "{fixer}");
    assert!(fixer.contains("isolated_build_cache"), "{fixer}");
    assert!(fixer.contains("FLAKY"), "{fixer}");

    let home_dir = magi::run::home();
    let facts = magi::chat_notify::run_facts(&home_dir, &state.id);
    let ver = facts.verification.as_deref().expect("chat verification fact");
    assert!(ver.contains("distinct terminal failure(s)"), "{ver}");
    assert!(ver.contains("talk::tests::chat_timeout"), "{ver}");
    assert!(ver.contains("isolated_build_cache"), "{ver}");
    assert!(!ver.contains("reproduced"), "{ver}");

    let reloaded = RunState::load(&state.id).expect("reload");
    assert_eq!(reloaded.reviews[0].e2e[0].rerun_comparison.as_ref(), Some(cmp));
}
}

common::e2e! {
async fn ambiguous_double_failure_reports_unknown_and_keeps_both_attempts() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.implementers = 1;
    fx.config.graph.review_rounds = 1;
    let counter = fx.tmp.path().join("e2e-ambiguous-count");
    let c = sh(&counter);
    // Attempt 1 ends mid-run without a final summary (simulating a hung unit test);
    // attempt 2 finishes with a confirmed nextest failure.
    fx.config.verify.e2e = vec![format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $n -eq 1 ]; then \
           printf 'running 2 tests\\ntest ok_test ... ok\\ntest talk::tests::chat_timeout has been running for over 60 seconds\\n'; \
           exit 1; \
         else \
           printf -- '------------\\n     Summary [   2.000s] 3 tests run: 2 passed, 1 failed, 0 skipped\\n        FAIL [   1.000s] magi-cli::isolated_build_cache test_cache\\n' >&2; \
           exit 100; \
         fi"
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(state.merge.is_none());
    assert_eq!(count(&counter), 2);

    let r1 = &state.reviews[0];
    let cmp = r1.e2e[0]
        .rerun_comparison
        .as_ref()
        .expect("unknown comparison recorded");
    assert!(
        matches!(
            cmp.verdict,
            magi::verify_evidence::ComparisonVerdict::Unknown { .. }
        ),
        "{:?}",
        cmp.verdict
    );
    assert_eq!(
        cmp.first.intermediate.long_running,
        vec!["talk::tests::chat_timeout"]
    );
    assert!(
        matches!(
            cmp.second.extraction,
            magi::verify_evidence::FailureExtraction::Confirmed { .. }
        ),
        "{:?}",
        cmp.second.extraction
    );

    let events = verify_events(state).join("\n");
    assert!(events.contains("comparison unknown"), "{events}");
    assert!(!events.contains("the failure reproduced"), "{events}");
    assert!(events.contains("isolated_build_cache"), "{events}");

    let reloaded = RunState::load(&state.id).expect("reload");
    assert_eq!(reloaded.reviews[0].e2e[0].rerun_comparison.as_ref(), Some(cmp));

    // Resuming on the unchanged stopped head reuses saved facts without rerunning e2e.
    let mut resumed = Runner::resume(&state.id).expect("resume");
    resumed.execute().await.expect("resume execute");
    assert_eq!(resumed.state.status, RunStatus::Blocked);
    assert_eq!(count(&counter), 2, "unchanged stop must not rerun e2e");
    assert_eq!(
        resumed.state.reviews[0].e2e[0].rerun_comparison.as_ref(),
        Some(cmp)
    );
}
}

common::e2e! {
async fn with_the_switch_off_the_first_result_decides_as_before() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, true);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.implementers = 1;
    fx.config.graph.e2e_every_round = true;
    fx.config.graph.rerun_failed_e2e = false;
    let runs = fx.tmp.path().join("e2e-runs");
    fx.config.verify.e2e = vec![format!(
        "echo run >> {}; test -f fixed.txt || {{ echo 'test web::tests::layout ... FAILED'; exit 1; }}",
        sh(&runs)
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    assert_eq!(state.reviews[0].e2e_status(), E2eStatus::Failed);
    assert!(!state.reviews[0].e2e[0].output_tail.contains("failed twice"));
    // One execution per round, exactly the events there were before the rerun.
    let ran = std::fs::read_to_string(&runs).unwrap_or_default().lines().count();
    assert_eq!(ran, state.reviews.len());
    let events = verify_events(state);
    assert_eq!(events.len(), state.reviews.len(), "{events:?}");
    assert!(events[0].starts_with("round 1: `") && events[0].contains("-> FAIL (Some(1))"));
    assert!(events[1].starts_with("round 2: `") && events[1].contains("-> pass"));
    assert!(state.verify_flakes.is_empty() && flake_tasks().is_empty());
}
}

common::e2e! {
async fn a_build_failure_keeps_its_one_retry_and_is_never_rerun_as_a_test() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    let runs = fx.tmp.path().join("e2e-runs");
    fx.config.verify.e2e = vec![format!(
        "echo run >> {}; echo 'error: could not compile `fixture`'; exit 101",
        sh(&runs)
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    let events = verify_events(state).join("\n");
    assert!(events.contains("retrying once before concluding"), "{events}");
    assert!(!events.contains("once more on the same commit"), "{events}");
    assert!(state.reviews.iter().any(|r| r.verify_retried));
    let ran = std::fs::read_to_string(&runs).unwrap_or_default().lines().count();
    let rounds_that_ran = state.reviews.iter().filter(|r| !r.e2e.is_empty()).count();
    assert_eq!(ran, 2 * rounds_that_ran, "attempt + build retry, nothing more: {events}");
    assert!(state.verify_flakes.is_empty() && flake_tasks().is_empty());
}
}

common::e2e! {
async fn a_verification_never_runs_more_than_three_times() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    let counter = fx.tmp.path().join("e2e-count");
    let c = sh(&counter);
    // 1: cannot build; 2: a test failure; 3 and later: green.
    fx.config.verify.e2e = vec![format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $n -eq 1 ]; then echo 'error: could not compile `fixture`'; exit 101; fi; \
         if [ $n -eq 2 ]; then echo 'test web::tests::layout ... FAILED'; exit 1; fi"
    )];

    let runner = run_once(&fx, fx.config.clone()).await;
    let state = &runner.state;
    let round = state.reviews.last().expect("a round");
    assert_eq!(round.e2e_status(), E2eStatus::Passed, "{:?}", verify_events(state));
    assert!(round.verify_retried);
    assert_eq!(count(&counter), 3, "build retry, then one rerun, then stop");
    assert_eq!(state.verify_flakes.len(), 1);
    assert_eq!(flake_tasks().len(), 1);
}
}
