//! End-to-end: the graft phase ports the losers' judge-named strengths onto
//! the winner, is skipped when there is nothing to port, and is rolled back
//! (never failing the run) when the fixer fails or its diff is too large.
mod common;

use common::{Fixture, Judges, fixture};
use magi::graph::Runner;
use magi::run::{GraftOutcome, RunState, RunStatus};

fn with_env(fx: &mut Fixture, pairs: &[(&str, &str)]) {
    for a in &mut fx.config.agents {
        for (k, v) in pairs {
            a.env.insert((*k).to_owned(), (*v).to_owned());
        }
    }
}

async fn run(fx: &Fixture) -> Runner {
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".to_owned(),
        fx.config.clone(),
        magi::run::Origin::operator(),
    )
    .await
    .expect("start");
    runner.execute().await.expect("execute");
    runner
}

fn graft_events(state: &RunState) -> Vec<&str> {
    state
        .events
        .iter()
        .filter(|e| e.node == "graft")
        .map(|e| e.message.as_str())
        .collect()
}

fn winner_has(state: &RunState, file: &str) -> bool {
    state.winner().expect("winner").worktree.join(file).exists()
}

common::e2e! {
async fn a_graft_is_applied_and_reviewed_like_any_other_change() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    with_env(&mut fx, &[("MOCK_STRENGTHS", "1"), ("MOCK_GRAFT", "apply")]);
    let runner = run(&fx).await;
    let state = &runner.state;

    let g = state.graft.as_ref().expect("graft record");
    assert_eq!(g.outcome, GraftOutcome::Applied, "{:?}", graft_events(state));
    assert!(g.post.is_some() && g.post.as_deref() != Some(g.pre.as_str()));
    assert!(g.lines.unwrap() <= g.limit.unwrap());
    assert!(winner_has(state, "grafted.txt"));
    // The review that follows saw the grafted tree.
    assert!(!state.reviews.is_empty());
    assert_eq!(state.status, RunStatus::Ready, "{:?}", graft_events(state));

    // Blindness: the graft prompt speaks in labels, never in agent ids.
    let prompt = std::fs::read_to_string(state.dir().join("artifacts/graft.prompt.md"))
        .expect("graft prompt artifact");
    assert!(prompt.contains("# Graft the runner-up strengths"));
    assert!(prompt.contains("mock strength of B"));
    for agent in ["alpha", "beta", "gamma"] {
        assert!(!prompt.contains(agent), "graft prompt leaked `{agent}`");
    }
}
}

common::e2e! {
async fn a_solo_run_skips_the_graft_and_says_so() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.graph.implementers = 1;
    with_env(&mut fx, &[("MOCK_STRENGTHS", "1"), ("MOCK_GRAFT", "apply")]);
    let runner = run(&fx).await;
    let state = &runner.state;

    assert!(state.graft.is_none());
    assert!(!winner_has(state, "grafted.txt"));
    assert!(
        graft_events(state).iter().any(|m| m.starts_with("skipped: only one candidate")),
        "{:?}",
        graft_events(state)
    );
}
}

common::e2e! {
async fn judges_naming_no_strengths_leave_the_winner_untouched() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    with_env(&mut fx, &[("MOCK_GRAFT", "apply")]);
    let runner = run(&fx).await;
    let state = &runner.state;

    assert!(state.graft.is_none());
    assert!(!winner_has(state, "grafted.txt"));
    assert!(graft_events(state).iter().any(|m| m.contains("no strengths")));
}
}

common::e2e! {
async fn a_graft_over_the_size_limit_is_rolled_back() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    with_env(&mut fx, &[("MOCK_STRENGTHS", "1"), ("MOCK_GRAFT", "huge")]);
    let runner = run(&fx).await;
    let state = &runner.state;

    let g = state.graft.as_ref().expect("graft record");
    assert!(matches!(&g.outcome, GraftOutcome::RolledBack(why) if why.contains("limit")), "{g:?}");
    assert!(!winner_has(state, "grafted.txt"));
    assert_eq!(state.status, RunStatus::Ready, "the run goes on as if graft were off");
    assert!(!state.reviews.is_empty());
}
}

common::e2e! {
async fn a_failing_graft_is_rolled_back_and_the_run_finishes() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    with_env(&mut fx, &[("MOCK_STRENGTHS", "1"), ("MOCK_GRAFT", "fail")]);
    let runner = run(&fx).await;
    let state = &runner.state;

    let g = state.graft.as_ref().expect("graft record");
    assert!(matches!(g.outcome, GraftOutcome::RolledBack(_)), "{g:?}");
    assert!(!winner_has(state, "grafted.txt"));
    assert_eq!(state.status, RunStatus::Ready);
}
}

common::e2e! {
async fn a_graft_that_changes_nothing_is_recorded_as_rolled_back() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    with_env(&mut fx, &[("MOCK_STRENGTHS", "1"), ("MOCK_GRAFT", "noop")]);
    let runner = run(&fx).await;
    let g = runner.state.graft.as_ref().expect("graft record");
    assert!(matches!(&g.outcome, GraftOutcome::RolledBack(w) if w.contains("nothing")), "{g:?}");
}
}
