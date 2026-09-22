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
}
