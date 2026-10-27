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
async fn every_fixer_reaching_a_usage_limit_defers_without_spending_a_round() {
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
    assert!(state.gate_fixes.is_empty());
    assert_eq!(state.resource_block.as_ref().unwrap().kind, "agent usage limit");
    // One quota loss for the one exhausted call, like a single fixer.
    assert_eq!(state.quota.len(), 1, "{:?}", state.quota);
    assert_eq!(state.handovers.iter().filter(|h| h.node == "gate-fix").count(), 1);
    let mut task = magi::queue::Task::new("Usage outage".into(), "create note.txt".into(), fx.repo.clone(), magi::queue::Source::Human);
    task.start(state.id.clone());
    let queue = magi::queue::Queue::open();
    queue.put(&mut task).unwrap();
    magi::daemon::finish_attempt(2, &queue, &mut task, state, &[], Ok(()));
    assert_eq!(task.attempts, 0);
    let id = state.id.clone();
    drop(runner);
    let mut saved = magi::run::RunState::load(&id).unwrap();
    for agent in &mut saved.config.agents { agent.env.remove("MOCK_QUOTA_SEAT"); }
    saved.save().unwrap();
    let mut resumed = Runner::resume(&id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.status, RunStatus::Ready);
    assert_eq!(resumed.state.gate_fixes.len(), 1);

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

    assert!(state.gate_fixes.is_empty());
    assert_eq!(state.quota[0].agent, "beta");
    assert_eq!(state.quota.len(), 1);
    assert!(state.handovers.iter().all(|h| h.node != "gate-fix"));
}
}

common::e2e! {
async fn a_fallback_that_is_the_winners_author_gets_a_fresh_fix_seat() {
    let home = common::home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.disk.min_free_bytes = 0;
    fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
    fx.config.roles.fixer = chain(&["beta", "alpha"]);
    quota_on_fix(&mut fx, &["beta"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let winner = state.winner().expect("winner").clone();
    assert_eq!(winner.agent, "alpha", "the fixture's winner is the first agent");
    assert_eq!(state.gate_fixes[0].agent, "alpha");
    // Not the implementation conversation: the shared fix seat took it.
    assert_eq!(state.seats["fix"].agent, "alpha");
    assert_ne!(state.seats["fix"].claude_session, state.seats[&format!("impl-{}", winner.label)].claude_session);
}
}

common::e2e! {
async fn authentication_outages_defer_gate_and_review_fixes_without_spending_attempts() {
    for node in ["gate-fix", "fix"] {
        let home = common::home_lock().await;
        let mut fx = fixture(home, Judges::Unanimous, node == "fix");
        fx.config.disk.min_free_bytes = 0;
        fx.config.graph.gate_fix_rounds = 1;
        fx.config.roles.fixer = chain(&["beta", "gamma"]);
        if node == "gate-fix" {
            fx.config.verify.gate = vec![GATE_NEEDS_FIX.to_owned()];
        }
        for agent in &mut fx.config.agents {
            if ["beta", "gamma"].contains(&agent.id.as_str()) {
                agent.env.insert("MOCK_AUTH_SEAT".into(), "fix".into());
                agent.env.insert("MOCK_ONLY_NODE".into(), node.into());
            }
        }
        let mut runner = Runner::start(&fx.repo, "create note.txt".into(), fx.config.clone(), magi::run::Origin::operator()).await.unwrap();
        let mut task = magi::queue::Task::new("Authentication outage".into(), "create note.txt".into(), fx.repo.clone(), magi::queue::Source::Human);
        task.start(runner.state.id.clone());
        let result = runner.execute().await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(runner.state.status, RunStatus::Blocked);
        assert_eq!(runner.state.resource_block.as_ref().unwrap().node, node);
        assert!(runner.state.gate_fixes.is_empty());
        assert!(runner.state.reviews.iter().all(|round| round.fix.is_none()));
        assert_eq!(runner.state.quota.len(), 1);
        assert_eq!(runner.state.quota[0].cause, magi::agent::QuotaCause::Authentication);
        let queue = magi::queue::Queue::open();
        queue.put(&mut task).unwrap();
        magi::daemon::finish_attempt(2, &queue, &mut task, &runner.state, &[], result);
        assert_eq!(task.attempts, 0);
        assert_eq!(task.status, magi::queue::TaskStatus::Held);
        assert!(!task.operator_held());
        assert!(task.hold_reason.as_deref().unwrap().contains("re-authenticate"));
        let id = runner.state.id.clone();
        drop(runner);
        let mut saved = magi::run::RunState::load(&id).unwrap();
        for agent in &mut saved.config.agents { agent.env.remove("MOCK_AUTH_SEAT"); }
        saved.save().unwrap();
        let mut resumed = Runner::resume(&id).unwrap();
        resumed.execute().await.unwrap();
        assert_eq!(resumed.state.status, RunStatus::Ready, "{}", magi::report::run(&resumed.state));
        if node == "gate-fix" { assert_eq!(resumed.state.gate_fixes.len(), 1); }
    }
}
}
