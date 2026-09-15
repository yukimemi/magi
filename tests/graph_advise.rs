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
