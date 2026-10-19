//! The graft phase's decisions: whether it runs and what it is told.
//!
//! Between the tally and the review loop a fixer round ports the strengths
//! the judges attributed to the losing candidates onto the winner
//! (`graph::Runner::graft`). Everything that can be decided without I/O lives
//! here so the skip conditions are unit-tested directly.

use std::collections::BTreeSet;

use crate::blind;
use crate::run::RunState;

/// Strengths kept per candidate, and characters kept per strength.
const MAX_ITEMS: usize = 8;
const MAX_CHARS: usize = 400;

/// Why the graft phase did nothing. The wording is the run event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// `[graph] graft_runner_up = false`.
    Off,
    /// Fewer than two candidates ever existed.
    SoloCandidate,
    /// The tally named nobody.
    NoWinner,
    /// The verdict rests on too few judges.
    NoQuorum,
    /// No judge attributed a strength to a losing candidate.
    NoStrengths,
    /// Finished earlier, or the run is already past the point it applies.
    AlreadyDone,
}

impl Skip {
    /// The reason as shown in the run's events.
    pub fn reason(&self) -> &'static str {
        match self {
            Skip::Off => "[graph] graft_runner_up is off",
            Skip::SoloCandidate => "only one candidate",
            Skip::NoWinner => "no winner",
            Skip::NoQuorum => "the tally did not meet quorum",
            Skip::NoStrengths => "the judges recorded no strengths for the losing candidates",
            Skip::AlreadyDone => "already done",
        }
    }
}

/// What a graft round is asked to port, by label only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The winning label.
    pub winner: char,
    /// `(label, branch, strengths)` for each losing candidate with any.
    pub strengths: Vec<(char, String, Vec<String>)>,
}

/// The strengths of a plan with the ids the prompt shows (`A1`, `A2`, ...).
pub fn requested(plan: &Plan) -> Vec<crate::run::GraftStrength> {
    plan.strengths
        .iter()
        .flat_map(|&(label, _, ref items)| {
            items
                .iter()
                .enumerate()
                .map(move |(i, text)| crate::run::GraftStrength {
                    id: format!("{label}{}", i + 1),
                    source: label,
                    text: text.clone(),
                })
        })
        .collect()
}

/// Decide whether the graft runs and with what input. Pure.
pub fn plan(state: &RunState) -> Result<Plan, Skip> {
    if !state.config.graph.graft_runner_up {
        return Err(Skip::Off);
    }
    if state.candidates.len() < 2 {
        return Err(Skip::SoloCandidate);
    }
    let Some(tally) = state.tally.as_ref() else {
        return Err(Skip::NoWinner);
    };
    if !tally.met_quorum {
        return Err(Skip::NoQuorum);
    }
    // Reviews already exist, or a record settled the question: a resumed run
    // never gets a graft inserted after the fact.
    if !state.reviews.is_empty() || state.graft.as_ref().is_some_and(|g| g.outcome.settled()) {
        return Err(Skip::AlreadyDone);
    }
    let winner = tally.winner;
    let mut strengths = Vec::new();
    for c in &state.candidates {
        // A folded loser has lost its branch: nothing to port from.
        if c.label == winner || c.folded {
            continue;
        }
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        for j in state.judgements.iter().filter(|j| j.failed.is_none()) {
            let Some(list) = j
                .strengths
                .iter()
                .find(|(k, _)| {
                    k.trim().chars().next().map(|ch| ch.to_ascii_uppercase()) == Some(c.label)
                })
                .map(|(_, v)| v)
            else {
                continue;
            };
            for raw in list {
                let clean: String = blind::sanitize_prose(raw, &state.config.blind)
                    .chars()
                    .take(MAX_CHARS)
                    .collect();
                if !clean.trim().is_empty() && seen.insert(clean.to_lowercase()) {
                    items.push(clean);
                }
            }
        }
        items.truncate(MAX_ITEMS);
        if !items.is_empty() {
            strengths.push((c.label, c.branch.clone(), items));
        }
    }
    if strengths.is_empty() {
        return Err(Skip::NoStrengths);
    }
    Ok(Plan { winner, strengths })
}

/// The reason a record in a non-final state is rolled back on resume.
pub const INTERRUPTED: &str = "interrupted";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::{Candidate, GraftOutcome, GraftRecord, Judgement, Tally};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn cand(label: char) -> Candidate {
        Candidate {
            index: 0,
            label,
            agent: "a".to_owned(),
            branch: format!("magi/r/{label}"),
            worktree: PathBuf::from("/w"),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }
    }

    fn judge(strengths: &[(&str, &[&str])]) -> Judgement {
        Judgement {
            judge: 1,
            seat: "judge-1".to_owned(),
            agent: "a".to_owned(),
            ranking: vec!['A', 'B'],
            reasons: BTreeMap::new(),
            confidence: None,
            strengths: strengths
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
                .collect(),
            order: vec![0, 1],
            failed: None,
            duration_ms: 0,
        }
    }

    fn tally(winner: char, met_quorum: bool) -> Tally {
        Tally {
            first_choice: BTreeMap::new(),
            borda: BTreeMap::new(),
            winner,
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 1,
            present: 1,
            quorum: 1,
            met_quorum,
            uncontested: None,
        }
    }

    /// Two candidates, A won, one judge praised B's error handling.
    fn ready() -> RunState {
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        s.candidates = vec![cand('A'), cand('B')];
        s.judgements = vec![judge(&[("B", &["handles the empty input"])])];
        s.tally = Some(tally('A', true));
        s
    }

    #[test]
    fn a_ready_run_plans_the_losers_strengths_by_label() {
        let p = plan(&ready()).unwrap();
        assert_eq!(p.winner, 'A');
        assert_eq!(
            p.strengths,
            vec![(
                'B',
                "magi/r/B".to_owned(),
                vec!["handles the empty input".to_owned()]
            )]
        );
    }

    #[test]
    fn each_skip_condition_is_named() {
        let mut s = ready();
        s.config.graph.graft_runner_up = false;
        assert_eq!(plan(&s), Err(Skip::Off));

        let mut s = ready();
        s.candidates.truncate(1);
        assert_eq!(plan(&s), Err(Skip::SoloCandidate));

        let mut s = ready();
        s.tally = None;
        assert_eq!(plan(&s), Err(Skip::NoWinner));

        let mut s = ready();
        s.tally = Some(tally('A', false));
        assert_eq!(plan(&s), Err(Skip::NoQuorum));

        let mut s = ready();
        s.judgements = vec![judge(&[])];
        assert_eq!(plan(&s), Err(Skip::NoStrengths));

        // Strengths for the winner itself, or for a label that does not
        // exist, are not input.
        let mut s = ready();
        s.judgements = vec![judge(&[("A", &["x"]), ("Z", &["y"])])];
        assert_eq!(plan(&s), Err(Skip::NoStrengths));

        // A failed judge's strengths do not count.
        let mut s = ready();
        s.judgements[0].failed = Some("timeout".to_owned());
        assert_eq!(plan(&s), Err(Skip::NoStrengths));

        let mut s = ready();
        s.graft = Some(GraftRecord {
            pre: "x".to_owned(),
            post: None,
            lines: None,
            limit: None,
            agent: None,
            outcome: GraftOutcome::RolledBack("no change".to_owned()),
            requested: Vec::new(),
            adoption: None,
            adoption_error: None,
        });
        assert_eq!(plan(&s), Err(Skip::AlreadyDone));
    }

    #[test]
    fn strengths_are_deduplicated_across_judges() {
        let mut s = ready();
        s.judgements = vec![
            judge(&[("b", &["Same point", "another"])]),
            judge(&[("B", &["same point"])]),
        ];
        let p = plan(&s).unwrap();
        assert_eq!(p.strengths[0].2, vec!["Same point", "another"]);
    }
}
