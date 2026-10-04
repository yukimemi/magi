//! A seat whose agent fails for an ordinary reason (not only a rate limit) is
//! handed to the next roster agent: each agent once per seat, a quota or a
//! timeout always, any other failure only until two agents in a row fail it
//! the same way.

mod common;

use common::fixture_with_handover_failures;
use magi::graph::Runner;
use magi::run::RunState;

/// How many times a seat was asked at `node`, from the mock's attribution log.
fn asked(state: &RunState, node: &str, seat: &str) -> usize {
    let log = magi::agent::artifacts_dir(&state.dir()).join("attribution.log");
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.ends_with(&format!(" {node} {seat}")))
        .count()
}

fn implement_events(state: &RunState) -> Vec<&str> {
    state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect()
}

common::e2e! {
async fn a_failed_seat_is_handed_to_the_next_agent_and_recovers() {
    let _home = common::home_lock().await;
    let fx = fixture_with_handover_failures(_home, &["alpha"], &[], &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.quota.is_empty(), "{:?}", state.quota);
    assert_eq!(state.handovers.len(), 1, "{:?}", state.handovers);
    let h = &state.handovers[0];
    assert_eq!((h.node.as_str(), h.seat.as_str()), ("implement", "impl-A"));
    assert_eq!((h.from.as_str(), h.to.as_str()), ("alpha", "beta"));
    assert!(h.reason.contains("exited with"), "{h:?}");
    assert!(
        implement_events(state)
            .iter()
            .any(|m| m.contains("handed over alpha -> beta")),
        "{:?}",
        implement_events(state)
    );

    let a = state.candidates.iter().find(|c| c.label == 'A').expect("A");
    assert!(!a.empty && a.failed.is_none(), "{a:?}");
    assert_eq!(a.agent, "beta", "{a:?}");

    // Persisted: a reloaded run still names the handover.
    let loaded = RunState::load(&state.id).expect("load");
    assert_eq!(loaded.handovers, state.handovers);
}
}

common::e2e! {
async fn a_timeout_hands_the_seat_over() {
    let _home = common::home_lock().await;
    let fx = fixture_with_handover_failures(_home, &[], &["alpha"], &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.handovers.len(), 1, "{:?}", state.handovers);
    assert_eq!(state.handovers[0].reason, "timed out");
    assert_eq!(state.handovers[0].to, "beta");
    let a = state.candidates.iter().find(|c| c.label == 'A').expect("A");
    assert!(!a.empty && a.failed.is_none(), "{a:?}");
    assert_eq!(a.agent, "beta");
}
}

common::e2e! {
async fn two_identical_failures_in_a_row_stop_the_chain() {
    let _home = common::home_lock().await;
    // alpha and beta fail the same way; gamma would answer, but is never asked.
    let fx = fixture_with_handover_failures(_home, &["alpha", "beta"], &[], &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    let err = runner.execute().await.expect_err("no candidate produced a change");
    assert!(err.to_string().contains("no candidate produced a change"), "{err}");
    let state = &runner.state;

    assert_eq!(state.handovers.len(), 1, "{:?}", state.handovers);
    assert_eq!(state.handovers[0].to, "beta");
    assert_eq!(asked(state, "implement", "impl-A"), 2);
    assert!(state.quota.is_empty(), "a plain failure is not a quota loss");
}
}

common::e2e! {
async fn a_roster_that_runs_out_ends_the_seat_with_each_agent_tried_once() {
    let _home = common::home_lock().await;
    // alpha fails, beta times out (breaking the run of identical failures),
    // gamma fails: three agents, three asks, then nobody is left.
    let fx = fixture_with_handover_failures(_home, &["alpha", "gamma"], &["beta"], &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect_err("no candidate produced a change");
    let state = &runner.state;

    let moves: Vec<(&str, &str)> = state
        .handovers
        .iter()
        .map(|h| (h.from.as_str(), h.to.as_str()))
        .collect();
    assert_eq!(moves, [("alpha", "beta"), ("beta", "gamma")]);
    assert_eq!(asked(state, "implement", "impl-A"), 3);
}
}

common::e2e! {
async fn a_judge_seat_is_handed_over_too() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, common::Judges::Unanimous, false);
    // Judge slots rotate by one: judge-1 is `beta`. Only `beta` fails its
    // ranking; `gamma` (next in the roster) answers it.
    for a in &mut fx.config.agents {
        if a.id == "beta" {
            a.env.insert("MOCK_FAILED_SEAT".to_owned(), "judge-1".to_owned());
        }
    }
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    let h = state
        .handovers
        .iter()
        .find(|h| h.node == "judge" && h.seat == "judge-1")
        .unwrap_or_else(|| panic!("{:?}", state.handovers));
    assert_eq!((h.from.as_str(), h.to.as_str()), ("beta", "gamma"));
    let j = &state.judgements[0];
    assert_eq!(j.agent, "gamma", "{j:?}");
    assert!(j.failed.is_none() && !j.ranking.is_empty(), "{j:?}");
    assert!(state.quota.is_empty());
}
}
