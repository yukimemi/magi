//! An implement seat that loses to quota mid-run has other agents in the
//! machine's roster that might still be able to do the work — the graph must
//! fall through to the next untried one instead of giving up on the seat the
//! moment its first agent runs dry. Solo runs are the motivating case:
//! `Config::resolve_roles` only ever hands a solo seat one implementer slot,
//! so without the fallback there was nothing else to try.

mod common;

use common::fixture_with_quota_on_agents;
use magi::graph::Runner;

#[tokio::test]
async fn a_solo_seat_falls_through_to_the_next_agent_and_recovers() {
    let _home = common::home_lock().await;
    // `alpha` (the only agent a solo run would otherwise ever pick) is
    // rate-limited on the implement seat; `beta` and `gamma` are not.
    let fx = fixture_with_quota_on_agents(_home, &["alpha"], &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // Recovered on the next roster entry: no lasting quota loss recorded —
    // `daemon.rs`'s own backoff reads `state.quota` to decide whether a task
    // attempt should go unspent, and a seat that ultimately produced a
    // candidate must not still look like a stalled panel.
    assert!(
        state.quota.is_empty(),
        "a recovered seat must not leave a QuotaLoss behind: {:?}",
        state.quota
    );

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events
            .iter()
            .any(|m| m.contains("rate limited (quota)") && m.contains("retrying with beta")),
        "{events:?}"
    );

    let a = state
        .candidates
        .iter()
        .find(|c| c.label == 'A')
        .expect("candidate A");
    assert!(!a.empty, "{a:?}");
    assert!(a.failed.is_none(), "{a:?}");
    assert!(a.commits > 0, "{a:?}");
    // The stats tables must credit whoever actually answered, not the agent
    // that was rate limited out.
    assert_eq!(a.agent, "beta", "{a:?}");
}

#[tokio::test]
async fn a_seat_that_exhausts_the_whole_roster_records_exactly_one_quota_loss() {
    let _home = common::home_lock().await;
    // Every agent in the roster is rate-limited on this seat: the fallback
    // chain runs out and today's behaviour applies unchanged.
    let fx = fixture_with_quota_on_agents(_home, &["alpha", "beta", "gamma"], &["impl-A"]);
    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    // An ordinary all-empty implement round: `after_implement` still fails
    // the run outright (see `after_implement_still_fails_an_ordinary_all_empty_run`),
    // exactly as it does today with no fallback in the picture at all — the
    // fallback chain running out changes nothing about that.
    let err = runner
        .execute()
        .await
        .expect_err("no candidate produced a change");
    assert!(
        err.to_string().contains("no candidate produced a change"),
        "{err}"
    );
    let state = &runner.state;

    // Exactly one loss, not one per fallback attempt: every intermediate
    // quota this loop absorbed is folded into an event, only the final,
    // unrecovered one is recorded.
    assert_eq!(state.quota.len(), 1, "{:?}", state.quota);
    assert_eq!(state.quota[0].seat, "impl-A");
    assert_eq!(state.quota[0].node, "implement");

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events.iter().any(|m| m.contains("retrying with beta")),
        "{events:?}"
    );
    assert!(
        events.iter().any(|m| m.contains("retrying with gamma")),
        "{events:?}"
    );

    // Left as the ordinary quota loss it always was, once nothing else in
    // the roster is left to try.
    let a = state
        .candidates
        .iter()
        .find(|c| c.label == 'A')
        .expect("candidate A");
    assert!(a.empty, "{a:?}");
    assert_eq!(a.commits, 0, "{a:?}");
    assert_eq!(
        a.failed.as_deref(),
        Some("rate limited (quota); produced no change")
    );
    // Nobody actually answered — every fallback also quota'd out — so the
    // candidate must keep the agent it was assigned at creation, not the
    // last one the fallback happened to try. Crediting `gamma` here would
    // erase `alpha`'s and `beta`'s own quota losses from any per-agent stats
    // keyed on `Candidate::agent`, and unfairly count the loss against
    // `gamma` alone.
    assert_eq!(a.agent, "alpha", "{a:?}");
}

#[tokio::test]
async fn a_later_candidate_slots_seat_falls_back_past_its_own_position_not_the_roster_front() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, common::Judges::Unanimous, false);
    fx.config.graph.candidates = 2;
    // `implementers` rotation gives candidate slot 0 -> alpha, slot 1 ->
    // beta (`Config::resolve_roles`, offset 0). Which *label* each slot lands
    // on is a seeded shuffle (`blind::assign_labels`), so the seat name for
    // slot 1 has to be computed from the fixture's own seed rather than
    // assumed to be "impl-B".
    let labels = magi::blind::assign_labels(2, fx.config.blind.seed.expect("fixture pins a seed"));
    let seat_for_slot_1 = format!("impl-{}", labels[1]);
    // Only slot 1's own agent (`beta`) is rate-limited; `alpha` and `gamma`
    // are not. The fallback must walk forward from beta's own position in
    // the roster (index 1) to gamma, never back to alpha, which is slot 0's
    // own agent already.
    for a in &mut fx.config.agents {
        if a.id == "beta" {
            a.env
                .insert("MOCK_QUOTA_SEAT".to_owned(), seat_for_slot_1.clone());
        }
    }

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert!(state.quota.is_empty(), "{:?}", state.quota);

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events.iter().any(|m| m.contains("retrying with gamma")),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|m| m.contains("retrying with alpha")),
        "must not fall back onto a different candidate slot's own agent: {events:?}"
    );

    let slot0 = state
        .candidates
        .iter()
        .find(|c| c.index == 0)
        .expect("candidate slot 0");
    assert_eq!(slot0.agent, "alpha", "{slot0:?}");
    assert!(!slot0.empty && slot0.failed.is_none(), "{slot0:?}");

    let slot1 = state
        .candidates
        .iter()
        .find(|c| c.index == 1)
        .expect("candidate slot 1");
    assert_eq!(slot1.agent, "gamma", "{slot1:?}");
    assert!(!slot1.empty && slot1.failed.is_none(), "{slot1:?}");
}

#[tokio::test]
async fn a_later_candidate_slots_fallback_chain_stops_at_the_rosters_tail_without_wrapping() {
    let _home = common::home_lock().await;
    let mut fx = common::fixture(_home, common::Judges::Unanimous, false);
    fx.config.graph.candidates = 2;
    let labels = magi::blind::assign_labels(2, fx.config.blind.seed.expect("fixture pins a seed"));
    let seat_for_slot_1 = format!("impl-{}", labels[1]);
    // Slot 1's own agent (`beta`) and the only roster entry after it
    // (`gamma`) both quota on this seat; `alpha` — earlier in the roster,
    // and slot 0's own agent — never does. Once gamma also quotas there is
    // nothing left forward of beta to fall through to, and the seat must
    // fail rather than wrap back onto alpha.
    for a in &mut fx.config.agents {
        if a.id == "beta" || a.id == "gamma" {
            a.env
                .insert("MOCK_QUOTA_SEAT".to_owned(), seat_for_slot_1.clone());
        }
    }

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone())
        .await
        .expect("start");
    // Slot 0 (alpha) still produces a viable candidate, so the run does not
    // bail outright the way an all-empty implement round would — judging is
    // simply skipped down to the one candidate that exists (`judge`'s own
    // `viable.len() == 1` short-circuit).
    runner.execute().await.expect("execute");
    let state = &runner.state;

    // Exactly one loss, recorded once the fallback chain ran out — never a
    // wrap back onto alpha, which would have recovered the seat instead.
    assert_eq!(state.quota.len(), 1, "{:?}", state.quota);
    assert_eq!(state.quota[0].seat, seat_for_slot_1);

    let events: Vec<&str> = state
        .events
        .iter()
        .filter(|e| e.node == "implement")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        events.iter().any(|m| m.contains("retrying with gamma")),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|m| m.contains("retrying with alpha")),
        "must not wrap back onto a different candidate slot's own agent: {events:?}"
    );

    let slot0 = state
        .candidates
        .iter()
        .find(|c| c.index == 0)
        .expect("candidate slot 0");
    assert_eq!(slot0.agent, "alpha", "{slot0:?}");
    assert!(!slot0.empty && slot0.failed.is_none(), "{slot0:?}");

    let slot1 = state
        .candidates
        .iter()
        .find(|c| c.index == 1)
        .expect("candidate slot 1");
    assert!(slot1.empty, "{slot1:?}");
    assert_eq!(
        slot1.failed.as_deref(),
        Some("rate limited (quota); produced no change")
    );
    // Nobody actually answered this seat — both fallback attempts also
    // quota'd out — so it must keep its originally assigned agent, not the
    // last one the (exhausted) fallback happened to try.
    assert_eq!(slot1.agent, "beta", "{slot1:?}");
}
