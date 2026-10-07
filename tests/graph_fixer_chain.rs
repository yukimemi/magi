//! `[roles] fixer` as a fallback chain, exercised through the gate-fix round
//! (the shortest path through the shared chain; the review loop, `land` and
//! the rebase round use the same `fixer::attempts`).
mod common;

use common::{Judges, fixture};
use magi::config::AgentChoice;
use magi::graph::Runner;
use magi::run::RunStatus;

const GATE_NEEDS_FIX: &str = "test -f gatefix.txt || { echo 'gatefix.txt is missing'; exit 1; }";

fn chain(ids: &[&str]) -> Option<AgentChoice> {
    Some(AgentChoice::Chain(
        ids.iter().map(|s| (*s).to_owned()).collect(),
    ))
}

/// Make `agents` rate limited on the shared `fix` seat.
fn quota_on_fix(fx: &mut common::Fixture, agents: &[&str]) {
    for a in &mut fx.config.agents {
        if agents.contains(&a.id.as_str()) {
            a.env.insert("MOCK_QUOTA_SEAT".to_owned(), "fix".to_owned());
        }
    }
}

fn artifact(runner: &Runner, name: &str) -> bool {
    runner.state.dir().join("artifacts").join(name).exists()
}

common::e2e! {
async fn a_quota_on_the_first_fixer_hands_the_round_to_the_next_with_a_fresh_seat() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    fx.config.roles.fixer = chain(&["beta", "gamma"]);
    quota_on_fix(&mut fx, &["beta"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.status, RunStatus::Ready);
    assert_eq!(state.gate_fixes.len(), 1);
    assert_eq!(state.gate_fixes[0].agent, "gamma");
    assert!(state.gate_fixes[0].committed);
    // The first agent's quota is a handover, not a quota loss of the seat.
    assert!(state.quota.is_empty(), "{:?}", state.quota);
    let hand: Vec<_> = state.handovers.iter().filter(|h| h.node == "gate-fix").collect();
    assert_eq!(hand.len(), 1, "{:?}", state.handovers);
    assert_eq!((hand[0].from.as_str(), hand[0].to.as_str()), ("beta", "gamma"));
    // A fresh seat: gamma owns the seat now, with its own session id.
    let seat = &state.seats["fix"];
    assert_eq!(seat.agent, "gamma");
    // Each id once, each attempt under its own artifact name.
    assert!(artifact(&runner, "gate-fix-1.prompt.md"));
    assert!(artifact(&runner, "gate-fix-1-gamma.prompt.md"));
    assert!(!artifact(&runner, "gate-fix-1-beta.prompt.md"));
}
}

common::e2e! {
async fn every_fixer_failing_ends_like_a_single_failed_fixer() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.gate_fix_rounds = 1;
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    fx.config.roles.fixer = chain(&["beta", "gamma"]);
    quota_on_fix(&mut fx, &["beta", "gamma"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.status, RunStatus::Blocked);
    assert_eq!(state.gate_fixes.len(), 1);
    assert_eq!(
        state.gate_fixes[0].error.as_deref(),
        Some("rate limited (quota); fixer could not run")
    );
    // One quota loss for the one exhausted call, like a single fixer.
    assert_eq!(state.quota.len(), 1, "{:?}", state.quota);
    assert_eq!(state.handovers.iter().filter(|h| h.node == "gate-fix").count(), 1);
}
}

common::e2e! {
async fn a_fallback_that_answered_keeps_the_seat_for_the_next_round() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.graph.gate_fix_rounds = 2;
    fx.config.verify.gate = vec!["echo still red; exit 1".to_owned()];
    fx.config.roles.fixer = chain(&["beta", "gamma"]);
    quota_on_fix(&mut fx, &["beta"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let agents: Vec<&str> = state.gate_fixes.iter().map(|g| g.agent.as_str()).collect();
    assert_eq!(agents, ["gamma", "gamma"]);
    // Round 2 started at gamma: beta was not asked (and billed) again.
    assert_eq!(state.handovers.iter().filter(|h| h.node == "gate-fix").count(), 1);
    assert!(!artifact(&runner, "gate-fix-2-beta.prompt.md"));
    assert!(!artifact(&runner, "gate-fix-2-gamma.prompt.md"));
}
}

common::e2e! {
async fn a_single_string_fixer_behaves_as_before() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    fx.config.roles.fixer = Some("beta".into());
    quota_on_fix(&mut fx, &["beta"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.gate_fixes[0].agent, "beta");
    assert_eq!(state.quota.len(), 1);
    assert!(state.handovers.iter().all(|h| h.node != "gate-fix"));
}
}
