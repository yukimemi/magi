//! End-to-end: the design-deliberation stage (`graph::Runner::advise`) that
//! runs before `implement` — advisor seats sketch a design, a synthesis
//! seat blends them, and the result is carried into every implementer's
//! prompt.
mod common;

use std::path::Path;

use common::fixture_with_advise;
use magi::config::Roles;
use magi::graph::Runner;

fn implementer_prompt(run_dir: &Path, label: char) -> String {
    std::fs::read_to_string(
        run_dir
            .join("artifacts")
            .join(format!("impl-{label}.prompt.md")),
    )
    .expect("implementer prompt artifact exists")
}

#[tokio::test]
async fn advisor_proposals_are_gathered_and_synthesized_into_the_implementer_briefing() {
    let home = common::home_lock().await;
    let mut fx = fixture_with_advise(home, 2);
    fx.config.graph.candidates = 1;

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.advise_attempted);
    let advice = state
        .advice
        .as_ref()
        .expect("advise ran and recorded something");
    assert_eq!(advice.records.len(), 2, "one record per advisor seat");
    assert_eq!(
        advice.proposals().len(),
        2,
        "both mock advisor seats answer with a usable proposal"
    );
    let synthesis = advice
        .synthesis
        .as_deref()
        .expect("both proposals usable, so the synthesis seat should have run");
    assert!(synthesis.contains("advisor-1"), "{synthesis}");

    // The whole point: the implementer actually sees it.
    let prompt = implementer_prompt(&state.dir(), 'A');
    assert!(prompt.contains("# Design deliberation"), "{prompt}");
    assert!(prompt.contains(synthesis), "{prompt}");
}

#[tokio::test]
async fn the_on_off_switch_leaves_no_trace_when_off() {
    let home = common::home_lock().await;
    // The shared `fixture` already turns `advise` off; this asserts that
    // default explicitly rather than relying on it silently.
    let mut fx = common::fixture(home, common::Judges::Unanimous, false);
    assert!(!fx.config.graph.advise);
    fx.config.graph.candidates = 1;

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(!state.advise_attempted);
    assert!(state.advice.is_none());
    let prompt = implementer_prompt(&state.dir(), 'A');
    assert!(!prompt.contains("# Design deliberation"), "{prompt}");
}

#[tokio::test]
async fn the_proposal_count_config_controls_how_many_advisor_seats_are_asked() {
    let home = common::home_lock().await;
    let mut fx = fixture_with_advise(home, 1);
    fx.config.graph.candidates = 1;

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let advice = state.advice.as_ref().expect("advise ran");
    assert_eq!(
        advice.records.len(),
        1,
        "[graph] advisors = 1 must ask exactly one seat"
    );
}

#[tokio::test]
async fn a_failed_advisor_seat_still_leaves_a_record_and_the_others_still_synthesize() {
    let home = common::home_lock().await;
    let mut fx = fixture_with_advise(home, 2);
    fx.config.graph.candidates = 1;
    for a in &mut fx.config.agents {
        a.env
            .insert("MOCK_ADVISOR_FAIL_SEAT".to_owned(), "advisor-1".to_owned());
        a.env
            .insert("MOCK_SYNTH_NAMES".to_owned(), "advisor-2".to_owned());
    }

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let advice = state.advice.as_ref().expect("advise ran");
    assert_eq!(advice.records.len(), 2);
    let failed = advice
        .records
        .iter()
        .find(|r| r.seat == "advisor-1")
        .expect("advisor-1's record still exists");
    assert!(failed.proposal.is_none());
    assert!(failed.error.is_some());
    assert_eq!(
        failed.reflection,
        magi::advise::Reflection::Absent,
        "a seat with no proposal has nothing to reflect"
    );

    let ok = advice
        .records
        .iter()
        .find(|r| r.seat == "advisor-2")
        .expect("advisor-2's record still exists");
    assert!(ok.proposal.is_some());
    assert_eq!(
        ok.reflection,
        magi::advise::Reflection::Strong,
        "the synthesis names advisor-2 outright"
    );

    assert!(
        advice.synthesis.is_some(),
        "one usable proposal is enough to synthesize"
    );
}

#[tokio::test]
async fn an_unresolvable_advisor_roster_does_not_fail_the_run_and_names_the_run_data() {
    let home = common::home_lock().await;
    let mut fx = fixture_with_advise(home, 1);
    fx.config.graph.candidates = 1;
    fx.config.roles = Roles {
        advisors: vec!["nope".to_owned()],
        ..Roles::default()
    };

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner
        .execute()
        .await
        .expect("execute; a bad advisor roster must not abort the run");
    let state = &runner.state;

    assert!(state.advise_attempted);
    assert!(
        state.advice.is_none(),
        "nothing could even be asked, so there is no advice to record"
    );
    let run_json = state.dir().join("run.json").display().to_string();
    let named = state
        .events
        .iter()
        .any(|e| e.message.contains("nope") && e.message.contains(&run_json));
    assert!(
        named,
        "the resolution failure must name both the bad id and the run's own data path: {:?}",
        state.events.iter().map(|e| &e.message).collect::<Vec<_>>()
    );

    // The run must still reach a real verdict — this stage is enrichment,
    // not a precondition for the rest of the graph.
    assert!(state.status.done());
}

/// Reported: `advise`'s only reentry guard was `advise_attempted`, which a
/// run written by a binary that predates the field deserializes as `false`
/// (`#[serde(default)]`) regardless of how far the run actually got.
/// Resuming such a run — already past `implement`, worktrees `prep` will not
/// recreate because its own guard is "candidates non-empty" — walked
/// straight back into `advise` and sent every advisor seat at a worktree
/// that no longer existed, after implementation had already happened. The
/// fix reads candidate progress directly, the same predicate `implement`
/// itself uses to decide there is nothing left to do.
#[tokio::test]
async fn advise_does_not_reenter_once_implementation_has_already_progressed() {
    let home = common::home_lock().await;
    let mut fx = fixture_with_advise(home, 2);
    fx.config.graph.candidates = 1;

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;
    assert!(
        state.candidates[0].commits > 0,
        "the fixture's candidate must have actually implemented something"
    );
    let sketching = |events: &[magi::run::Event]| {
        events
            .iter()
            .filter(|e| e.message.contains("sketching a design in parallel"))
            .count()
    };
    assert_eq!(sketching(&state.events), 1, "advise ran exactly once so far");

    let id = state.id.clone();
    let run_json = state.dir().join("run.json");
    drop(runner);

    // Simulate a run written before `advice` / `advise_attempted` existed:
    // strip both keys back out of the persisted record. `#[serde(default)]`
    // is exactly what makes this deserialize as `false` / `None`, same as a
    // genuinely older file would.
    let raw = std::fs::read_to_string(&run_json).expect("read run.json");
    let mut value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    value
        .as_object_mut()
        .expect("run.json is an object")
        .remove("advice");
    value
        .as_object_mut()
        .expect("run.json is an object")
        .remove("advise_attempted");
    std::fs::write(&run_json, serde_json::to_string_pretty(&value).unwrap())
        .expect("write run.json");

    // Reenter exactly as `--resume` or the web UI's resume button would.
    let mut again = Runner::resume(&id).expect("resume");
    assert!(
        !again.state.advise_attempted,
        "the stripped field must read back as false, reproducing the bug's precondition"
    );
    again.execute().await.expect("re-execute");
    let state = &again.state;

    assert!(
        state.advice.is_none(),
        "implementation had already progressed, so advise must not run at all: {:?}",
        state.advice
    );
    assert_eq!(
        sketching(&state.events),
        1,
        "advise must not spawn a second advisor wave against a candidate that already implemented: {:?}",
        state.events
    );
    let skipped = state.events.iter().any(|e| {
        e.node == "advise" && e.message.contains("already shows implementation progress")
    });
    assert!(
        skipped,
        "the skip must be recorded, not silent: {:?}",
        state.events
    );
}
