//! A seat that already answered once and then fails a *later* ask in the same
//! run (the final vote, a judge's recovery, a reviewer's reconsideration, a
//! deliberation turn) is handed to the next roster agent too, and that agent -
//! which has no session of its own - is sent a full-context prompt rather than
//! the short one the seat's own conversation would have understood.

mod common;

use common::{Fixture, Judges};
use magi::graph::Runner;
use magi::run::RunState;

/// The context flags the mock saw on each prompt to `seat` at `node`, oldest
/// first. An empty string is a short, resume-style prompt.
fn prompts(state: &RunState, node: &str, seat: &str) -> Vec<String> {
    let log = magi::agent::artifacts_dir(&state.dir()).join("prompts.log");
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let (n, s) = (it.next()?, it.next()?);
            (n == node && s == seat).then(|| it.next().unwrap_or("").to_owned())
        })
        .collect()
}

fn set_env(fx: &mut Fixture, agent: &str, pairs: &[(&str, &str)]) {
    for a in &mut fx.config.agents {
        if a.id == agent {
            for (k, v) in pairs {
                a.env.insert((*k).to_owned(), (*v).to_owned());
            }
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

fn handover<'a>(state: &'a RunState, node: &str, seat: &str) -> Vec<&'a magi::run::Handover> {
    state
        .handovers
        .iter()
        .filter(|h| h.node == node && h.seat == seat)
        .collect()
}

/// judge-1 is `beta` (judge seats rotate by one) and its successor is `gamma`.
/// `beta` fails only its final vote, after having ranked and deliberated.
async fn vote_handover(env: &[(&str, &str)]) -> (Runner, Fixture) {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, Judges::Split, false);
    let mut pairs = vec![("MOCK_FAIL_SEAT", "judge-1"), ("MOCK_ONLY_NODE", "vote")];
    pairs.extend_from_slice(env);
    set_env(&mut fx, "beta", &pairs);
    let runner = run(&fx).await;
    (runner, fx)
}

fn assert_full_vote_handover(state: &RunState) {
    let hs = handover(state, "vote", "judge-1");
    assert_eq!(hs.len(), 1, "{:?}", state.handovers);
    assert_eq!((hs[0].from.as_str(), hs[0].to.as_str()), ("beta", "gamma"));
    let seen = prompts(state, "vote", "judge-1");
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0], "", "the seat's own session gets the short prompt");
    for needed in ["task", "cands", "own", "argued"] {
        assert!(seen[1].contains(needed), "{needed} missing: {seen:?}");
    }
    let v = state.votes.iter().find(|v| v.judge == 1).expect("vote");
    assert_eq!((v.agent.as_str(), v.vote), ("gamma", Some('B')), "{v:?}");
}

common::e2e! {
async fn a_judge_that_failed_its_vote_is_handed_over_with_full_context() {
    let (runner, _fx) = vote_handover(&[]).await;
    assert_full_vote_handover(&runner.state);
}
}

common::e2e! {
async fn a_judge_that_times_out_voting_is_handed_over_with_full_context() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, Judges::Split, false);
    fx.config.graph.timeout_judge = common::HANDOVER_BUDGET;
    set_env(&mut fx, "beta", &[("MOCK_HANG_SEAT", "judge-1"), ("MOCK_ONLY_NODE", "vote")]);
    let runner = run(&fx).await;
    assert_full_vote_handover(&runner.state);
    assert_eq!(handover(&runner.state, "vote", "judge-1")[0].reason, "timed out");
}
}

common::e2e! {
async fn a_judge_rate_limited_on_its_vote_is_handed_over_not_lost() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, Judges::Split, false);
    set_env(&mut fx, "beta", &[("MOCK_QUOTA_SEAT", "judge-1"), ("MOCK_ONLY_NODE", "vote")]);
    let runner = run(&fx).await;
    assert_full_vote_handover(&runner.state);
    assert!(runner.state.quota.is_empty(), "{:?}", runner.state.quota);
}
}

common::e2e! {
async fn a_vote_chain_ends_when_the_roster_does_and_asks_each_agent_once() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, Judges::Split, false);
    fx.config.graph.retries = 0;
    for a in ["beta", "gamma"] {
        set_env(&mut fx, a, &[("MOCK_FAIL_SEAT", "judge-1"), ("MOCK_ONLY_NODE", "vote")]);
    }
    let runner = run(&fx).await;
    let state = &runner.state;

    // beta -> gamma, and gamma fails the same way: nobody is left.
    assert_eq!(handover(state, "vote", "judge-1").len(), 1, "{:?}", state.handovers);
    assert_eq!(prompts(state, "vote", "judge-1").len(), 2);
    let v = state.votes.iter().find(|v| v.judge == 1).expect("vote");
    assert_eq!(v.vote, None, "{v:?}");
    assert!(state.quota.is_empty());
}
}

common::e2e! {
async fn a_deliberation_turn_is_handed_over_instead_of_skipped() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, Judges::Split, false);
    set_env(&mut fx, "beta", &[("MOCK_FAIL_SEAT", "judge-1"), ("MOCK_ONLY_NODE", "deliberate")]);
    let runner = run(&fx).await;
    let state = &runner.state;

    let hs = handover(state, "deliberate", "judge-1");
    assert_eq!(hs.len(), 1, "{:?}", state.handovers);
    assert_eq!((hs[0].from.as_str(), hs[0].to.as_str()), ("beta", "gamma"));
    let seen = prompts(state, "deliberate", "judge-1");
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(!seen[0].contains("resent"), "{seen:?}");
    assert!(seen[1].contains("resent"), "{seen:?}");
    let turn = state.deliberation[0]
        .turns
        .iter()
        .find(|t| t.judge == 1)
        .unwrap_or_else(|| panic!("judge 1 was skipped: {:?}", state.events));
    assert_eq!(turn.agent, "gamma");
    assert!(
        state.events.iter().all(|e| !e.message.contains("judge 1 skipped")),
        "{:?}",
        state.events
    );
}
}

common::e2e! {
async fn a_reviewer_that_failed_its_reconsideration_is_handed_over_with_the_patch_and_panel() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, Judges::Unanimous, false);
    // review-1 is `alpha`, review-2 `beta`; review-2 dissents, so the panel
    // splits and reconsiders. alpha fails only that reconsideration.
    for a in ["alpha", "beta", "gamma"] {
        set_env(&mut fx, a, &[("MOCK_SPLIT_REVIEW_SEAT", "review-2")]);
    }
    set_env(
        &mut fx,
        "alpha",
        &[
            ("MOCK_FAIL_SEAT", "review-1"),
            ("MOCK_ONLY_NODE", "review"),
            ("MOCK_ONLY_HAS", "Your revote"),
        ],
    );
    let runner = run(&fx).await;
    let state = &runner.state;

    let hs = handover(state, "review", "review-1");
    assert_eq!(hs.len(), 1, "{:?}", state.handovers);
    assert_eq!((hs[0].from.as_str(), hs[0].to.as_str()), ("alpha", "beta"));
    let recon: Vec<String> = prompts(state, "review", "review-1")
        .into_iter()
        .filter(|f| f.contains("panel"))
        .collect();
    assert_eq!(recon.len(), 2, "{recon:?}");
    assert!(!recon[0].contains("patch"), "{recon:?}");
    assert!(recon[1].contains("patch"), "{recon:?}");
    let rec = state.reviews[0]
        .reconsideration
        .iter()
        .find(|r| r.reviewer == 1)
        .expect("revote");
    assert_eq!((rec.agent.as_str(), rec.failed.as_deref()), ("beta", None), "{rec:?}");
    assert!(rec.vote.is_some());
}
}

common::e2e! {
async fn a_judge_recovering_from_a_stall_is_handed_over_for_its_ranking_and_its_vote() {
    let _home = common::home_lock().await;
    // judge-1 (beta) and judge-3 (alpha) are rate limited out: below quorum.
    let fx = common::fixture_with_quota(_home, &["judge-1", "judge-3"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    assert_eq!(runner.state.status, magi::run::RunStatus::Stalled);
    let id = runner.state.id.clone();
    drop(runner);

    // The quota clears, but judge-3's first agent (alpha) now fails its
    // ranking and the next (beta) fails its vote. Judge-3 is put back on alpha
    // as if the earlier handover had never happened.
    {
        let mut state = RunState::load(&id).expect("load");
        for a in &mut state.config.agents {
            a.env.remove("MOCK_QUOTA_SEAT");
            match a.id.as_str() {
                "alpha" => {
                    a.env.insert("MOCK_FAIL_SEAT".to_owned(), "judge-3".to_owned());
                    a.env.insert("MOCK_ONLY_NODE".to_owned(), "judge".to_owned());
                }
                "beta" => {
                    a.env.insert("MOCK_FAIL_SEAT".to_owned(), "judge-3".to_owned());
                    a.env.insert("MOCK_ONLY_NODE".to_owned(), "vote".to_owned());
                }
                _ => {}
            }
        }
        state.seats.remove("judge-3");
        state.handovers.clear();
        state.save().expect("save");
    }
    let mut again = Runner::resume(&id).expect("resume");
    again.execute().await.expect("re-execute");
    let state = &again.state;

    let ranking = handover(state, "judge", "judge-3");
    assert_eq!(ranking.len(), 1, "{:?}", state.handovers);
    assert_eq!((ranking[0].from.as_str(), ranking[0].to.as_str()), ("alpha", "beta"));
    let vote = handover(state, "vote", "judge-3");
    assert_eq!(vote.len(), 1, "{:?}", state.handovers);
    assert_eq!((vote[0].from.as_str(), vote[0].to.as_str()), ("beta", "gamma"));

    let seen = prompts(state, "vote", "judge-3");
    let last = seen.last().expect("a vote prompt");
    assert!(
        ["task", "cands", "own"].iter().all(|k| last.contains(k)),
        "{seen:?}"
    );
    assert_eq!(state.judgements[2].agent, "beta");
    assert!(state.judgements[2].failed.is_none());
    let v = state.votes.iter().find(|v| v.judge == 3).expect("vote");
    assert_eq!(v.agent, "gamma", "{v:?}");
    assert!(v.vote.is_some(), "{v:?}");
    assert!(state.tally.as_ref().expect("tally").met_quorum);
}
}
