//! The `[roles] fixer` fallback chain, shared by every node that asks a fixer.
//!
//! Four nodes turn a fixer into a seat: the review loop's fix rounds (and the
//! operator-selected fix), the gate-fix round, `land`'s fix rounds and the
//! rebase conflict round. Each of them runs a plain forward `for` over
//! [`attempts`] and moves to the next entry only when the call advances
//! ([`crate::agent::output_advances`], the same decision point the other
//! chained roles use); this module holds what they would otherwise each
//! write and drift on: the order, the seat of each entry, and what a
//! handover looks like in the record.
//!
//! **Sticky within a run, never persisted.** Once a fallback agent answered a
//! fixer node, later calls in the same run start from it, so its session
//! carries across rounds instead of the first agent being re-asked (and its
//! quota re-billed) every round. The memory is the run's own
//! [`RunState::handovers`]: the last handover on a [`FIX_NODES`] node whose
//! `to` is on the chain. The report and the behaviour therefore read one
//! record, and nothing is written to the config. The entries before the start
//! are still tried afterwards (each id at most once per call), so a chain
//! whose current agent fails can come back round to the others.
//!
//! A fallback agent always gets a fresh [`SeatState`] ([`seat_for`]), never
//! the previous agent's seat renamed: its session id is minted from the agent
//! too, because the previous agent's uuid is already taken by the CLI.

use crate::agent::{AgentOutput, SeatState};
use crate::config::{AgentSpec, ResolvedRoles};
use crate::run::{Candidate, FailClass, Handover, RunState};

/// The nodes whose handovers [`attempts`] reads as "who fixes now". Every
/// consumer records its handovers under exactly one of these, so the two sides
/// cannot disagree: a name here that nobody records would send every round
/// back to the first agent and bill its quota again.
pub const FIX_NODES: [&str; 4] = ["fix", "gate-fix", "land", "rebase"];

/// The agents to ask, in order, each with the seat key it sits in.
///
/// With `[roles] fixer` unset: the winner's own implementer, whose
/// conversation continues now that the competition is over (one entry, as it
/// has always been). With a chain: every entry once, starting at the sticky
/// agent. An entry that is the winner's own agent sits in the winner's seat
/// (`impl-<label>`), any other in the shared `fix` seat.
pub fn attempts(
    state: &RunState,
    roles: &ResolvedRoles,
    winner: &Candidate,
) -> Vec<(AgentSpec, String)> {
    let own = || {
        (
            state
                .config
                .agent(&winner.agent)
                .cloned()
                .unwrap_or_else(|_| roles.implementers[winner.index].clone()),
            format!("impl-{}", winner.label),
        )
    };
    let Some(chain) = roles.fixer.as_deref().filter(|c| !c.is_empty()) else {
        return vec![own()];
    };
    let start = sticky_start(chain, &state.handovers);
    (0..chain.len())
        .map(|k| &chain[(start + k) % chain.len()])
        .map(|f| {
            if f.id == winner.agent {
                own()
            } else {
                (f.clone(), "fix".to_owned())
            }
        })
        .collect()
}

/// Where in `chain` a call starts: the agent the last fixer-node handover
/// moved to, else the first.
fn sticky_start(chain: &[AgentSpec], handovers: &[Handover]) -> usize {
    handovers
        .iter()
        .rev()
        .filter(|h| FIX_NODES.contains(&h.node.as_str()))
        .find_map(|h| chain.iter().position(|s| s.id == h.to))
        .unwrap_or(0)
}

/// Fetch or create the seat `key` for `agent`. The same agent continues its
/// conversation; another agent takes a fresh seat whose session id is minted
/// from the agent as well, so `agent::has_session` is false and the full
/// context is sent again.
pub fn seat_for(state: &mut RunState, key: &str, agent: &str) -> SeatState {
    if let Some(existing) = state.seats.get(key)
        && existing.agent == agent
    {
        return existing.clone();
    }
    let fresh = if state.seats.contains_key(key) {
        crate::graph::handover_seat(key, agent, state.next_seat_seed())
    } else {
        SeatState::new(key, agent, state.seed)
    };
    state.seats.insert(key.to_owned(), fresh.clone());
    fresh
}

/// How a call that did not satisfy the chain failed, and a short reason, for
/// the handover record (`land` / `rebase` call the CLI directly).
pub fn failure_of(out: &anyhow::Result<AgentOutput>) -> (FailClass, String) {
    match out {
        Err(e) => (
            FailClass::Other("error".to_owned()),
            format!("{e:#}")
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(160)
                .collect(),
        ),
        Ok(o) if o.quota_exhausted() => (FailClass::Quota, "rate limited (quota)".to_owned()),
        Ok(o) if o.timed_out => (FailClass::Timeout, "timed out".to_owned()),
        Ok(o) => (
            FailClass::Other("unusable".to_owned()),
            format!("nothing usable (exit {:?})", o.exit_code),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentKind;
    use jiff::Timestamp;
    use std::collections::BTreeMap;

    fn spec(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["true".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }
    }

    fn handover(node: &str, to: &str) -> Handover {
        Handover {
            at: Timestamp::now(),
            node: node.to_owned(),
            seat: "fix".to_owned(),
            from: "x".to_owned(),
            to: to.to_owned(),
            reason: String::new(),
        }
    }

    #[test]
    fn the_start_follows_the_last_fixer_handover_on_the_chain() {
        let chain = [spec("a"), spec("b"), spec("c")];
        assert_eq!(sticky_start(&chain, &[]), 0);
        // A reviewer's handover is not a fixer's.
        assert_eq!(sticky_start(&chain, &[handover("review", "c")]), 0);
        // An agent off the chain is ignored.
        assert_eq!(sticky_start(&chain, &[handover("fix", "z")]), 0);
        assert_eq!(
            sticky_start(&chain, &[handover("fix", "b"), handover("rebase", "c")]),
            2
        );
        assert_eq!(
            sticky_start(&chain, &[handover("fix", "c"), handover("land", "a")]),
            0
        );
    }
}
