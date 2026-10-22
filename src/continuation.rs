//! One deterministic answer to "what can an owner's *continue* do for this run?".
//!
//! The question used to be asked twice with different answers: the conductor's
//! model offered `Resume`, and the daemon's admission (`handover::can_resume`)
//! then refused it as "cannot make progress". The owner approved something
//! magi had already decided it would not do. Both sides now call [`classify`],
//! and `Resume` is defined *as* `can_resume`, so the offer and the admission
//! cannot drift apart.
//!
//! Every continuation keeps the task, the pull request, the branch and the
//! history. None of them merges, discards, or starts a new competition.

use crate::run::{RunState, RunStatus};

/// Why a run's review budget is spent. Derived from the recorded rounds only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Rounds remain, or nothing was reviewed yet.
    Open,
    /// A reviewer seat never answered in the last round; nothing was judged.
    ReviewFailed,
    /// Review concluded (clean, disabled, or handed off with no open
    /// findings): what is left is verification or landing.
    VerificationComplete,
    /// A pull request is open and landing can continue from its checkpoint.
    LandingCheckpoint,
    /// The last round still held blocking findings when the budget ran out.
    UnresolvedFindings,
}

/// What a *continue* answer can do for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Continuation {
    /// The existing graph can resume as it is.
    Resume,
    /// Review the surviving branch again (a bounded repair-and-review pass
    /// through the existing takeover path): the fixer works on the findings,
    /// the same pull request head is reused, nothing is merged.
    RepairAndReview {
        /// The surviving branch to review again.
        branch: String,
    },
    /// Nothing safe can be done automatically.
    Blocked {
        /// The specific unmet precondition.
        precondition: String,
        /// What the owner can do next, without run ids or CLI syntax.
        continuation: String,
    },
}

impl Continuation {
    /// Operator-facing sentence for a continuation that is not a plain resume.
    pub fn explain(&self) -> Option<String> {
        match self {
            Self::Resume => None,
            Self::RepairAndReview { branch } => Some(format!(
                "The review budget was spent with unresolved findings, so a plain resume cannot make progress. \
                 Instead the existing branch `{branch}` is reviewed again with one bounded repair pass; \
                 the task, pull request and changes are kept, and nothing is merged."
            )),
            Self::Blocked {
                precondition,
                continuation,
            } => Some(format!("{precondition} {continuation}")),
        }
    }
}

/// Classify why the review budget of `state` stands where it does.
pub fn budget(state: &RunState) -> Budget {
    let rounds = state.config.graph.review_rounds;
    if crate::handover::landing_checkpoint(state) {
        return Budget::LandingCheckpoint;
    }
    if rounds == 0 || state.reviews.iter().any(|r| r.clean) {
        return Budget::VerificationComplete;
    }
    let Some(last) = state.reviews.last() else {
        return Budget::Open;
    };
    if state.reviews.len() < rounds {
        return Budget::Open;
    }
    if last.blocking > 0 {
        Budget::UnresolvedFindings
    } else if last.incomplete() {
        Budget::ReviewFailed
    } else {
        Budget::VerificationComplete
    }
}

/// The one judge. `Resume` is exactly `handover::can_resume`.
pub fn classify(state: &RunState) -> Continuation {
    if state.status.resumable() && state.liveness(false) == crate::run::Liveness::Live {
        return Continuation::Blocked {
            precondition: "The run already has a live driver.".into(),
            continuation: "Wait for it to finish; no second execution is started.".into(),
        };
    }
    if crate::handover::can_resume(state) {
        return Continuation::Resume;
    }
    if !state.status.resumable() {
        return Continuation::Blocked {
            precondition: format!("The run ended as `{:?}` and cannot be resumed.", state.status),
            continuation: "Its task, pull request and history are kept; ask for a review of the branch or file a follow-up instead.".into(),
        };
    }
    if state.released() || state.retired_to.is_some() || state.pr_taken_over_by.is_some() {
        return Continuation::Blocked {
            precondition: "The run's worktree was handed to a newer execution.".into(),
            continuation: "Continue on that newer run; this one is kept as history.".into(),
        };
    }
    let branch = state
        .winner()
        .filter(|c| !c.folded)
        .map(|c| c.branch.clone());
    match (budget(state), branch) {
        (Budget::UnresolvedFindings | Budget::ReviewFailed, Some(branch))
            if state.status == RunStatus::Blocked =>
        {
            Continuation::RepairAndReview { branch }
        }
        (Budget::UnresolvedFindings | Budget::ReviewFailed, None) => Continuation::Blocked {
            precondition: "The review budget is spent and no surviving branch is recorded.".into(),
            continuation: "Ask for the pull request head to be reviewed by naming its branch.".into(),
        },
        (Budget::VerificationComplete, _) => Continuation::Blocked {
            precondition: "Review is complete and the budget is spent; the remaining failure is in verification, not review.".into(),
            continuation: "Ask for the failing verification to be fixed; no further review round is needed.".into(),
        },
        _ => Continuation::Blocked {
            precondition: "The run has no pending work that a resume could execute.".into(),
            continuation: "Ask for the branch to be reviewed again, or hold the task.".into(),
        },
    }
}

/// Offer-time revalidation: drop every `resume` choice whose run `classify`
/// says nothing can continue (or whose record cannot be read), so an action
/// the daemon's admission would reject is never offered. A resume that maps to
/// a bounded repair-and-review stays offered. The caller supplies the safe
/// fallback when no choice is left.
pub fn revalidate_offer(q: &mut crate::ask::Question) {
    let stale: Vec<String> = q
        .actions
        .iter()
        .filter_map(|(label, action)| match action {
            crate::ask::ChoiceAction::Resume { run } => match RunState::load(run) {
                Ok(s) if !matches!(classify(&s), Continuation::Blocked { .. }) => None,
                _ => Some(label.clone()),
            },
            _ => None,
        })
        .collect();
    for label in stale {
        q.actions.remove(&label);
        q.choices.retain(|c| c != &label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::{Candidate, ReviewRound};
    use std::path::PathBuf;

    fn state(status: RunStatus) -> RunState {
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".into(),
            "abc1234def".into(),
            "t".into(),
            Config::default(),
        );
        s.status = status;
        s.driver_exited = true;
        s.config.graph.review_rounds = 1;
        s
    }

    fn round(blocking: usize, answered: usize, clean: bool) -> ReviewRound {
        serde_json::from_value(serde_json::json!({
            "round": 1, "head": "h", "reviews": [], "e2e": [],
            "verify_retried": false, "e2e_deferred": false,
            "blocking": blocking, "answered": answered, "expected": 2, "clean": clean,
        }))
        .unwrap()
    }

    fn with_winner(s: &mut RunState) {
        let mut c: Candidate = serde_json::from_value(serde_json::json!({
            "index": 0, "label": "A", "stat": "", "files": 1, "commits": 1, "agent": "x", "branch": "magi/task-1", "worktree": "/w",
            "summary": "", "empty": false,
        }))
        .unwrap();
        c.folded = false;
        s.candidates = vec![c];
        s.tally = Some(
            serde_json::from_value(serde_json::json!({
                "first_choice": {}, "borda": {}, "winner": "A", "unanimous_initial": true,
                "deliberated": false, "changed_votes": 0, "unanimous_final": true,
                "met_quorum": true
            }))
            .unwrap(),
        );
    }

    #[test]
    fn spent_budget_is_told_apart() {
        let mut s = state(RunStatus::Blocked);
        s.reviews = vec![round(2, 2, false)];
        assert_eq!(budget(&s), Budget::UnresolvedFindings);
        s.reviews = vec![round(0, 1, false)];
        assert_eq!(budget(&s), Budget::ReviewFailed);
        s.reviews = vec![round(0, 2, true)];
        assert_eq!(budget(&s), Budget::VerificationComplete);
    }

    #[test]
    fn exhausted_review_with_findings_offers_a_bounded_repair_not_a_resume() {
        let mut s = state(RunStatus::Blocked);
        s.reviews = vec![round(2, 2, false)];
        with_winner(&mut s);
        assert!(!crate::handover::can_resume(&s));
        assert_eq!(
            classify(&s),
            Continuation::RepairAndReview {
                branch: "magi/task-1".into()
            }
        );
    }

    #[test]
    fn completed_review_never_asks_for_another_review() {
        let mut s = state(RunStatus::Blocked);
        s.reviews = vec![round(0, 2, true)];
        s.gate_ran = true;
        with_winner(&mut s);
        let Continuation::Blocked { precondition, .. } = classify(&s) else {
            panic!("expected blocked");
        };
        assert!(precondition.contains("verification"));
    }

    #[test]
    fn resume_is_exactly_the_admission_policy() {
        for status in [RunStatus::Blocked, RunStatus::Gating, RunStatus::Failed] {
            for reviews in [vec![], vec![round(1, 2, false)], vec![round(0, 2, true)]] {
                let mut s = state(status);
                s.reviews = reviews;
                with_winner(&mut s);
                assert_eq!(
                    classify(&s) == Continuation::Resume,
                    crate::handover::can_resume(&s)
                );
            }
        }
    }

    #[test]
    fn no_branch_gives_a_specific_precondition() {
        let mut s = state(RunStatus::Blocked);
        s.reviews = vec![round(2, 2, false)];
        let Continuation::Blocked { precondition, .. } = classify(&s) else {
            panic!("expected blocked");
        };
        assert!(precondition.contains("no surviving branch"));
    }

    #[test]
    fn offer_drops_a_resume_that_admission_would_reject() {
        let mut q = crate::ask::Question::new(
            "t".into(),
            "conduct".into(),
            "conductor".into(),
            "s".into(),
            "d".into(),
            vec!["resume".into(), "hold".into()],
        );
        q.actions.insert(
            "resume".into(),
            crate::ask::ChoiceAction::Resume {
                run: "no-such-run".into(),
            },
        );
        q.actions
            .insert("hold".into(), crate::ask::ChoiceAction::Hold);
        revalidate_offer(&mut q);
        assert_eq!(q.choices, ["hold"]);
        assert!(!q.actions.contains_key("resume"));
    }
}
