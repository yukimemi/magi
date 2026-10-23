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
                "The review budget was spent before review succeeded, so a plain resume cannot make progress. \
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

/// Release owner-approved work through the same admission used by recovery
/// cards. Unreadable or blocked prior work stays held, with the approval in
/// `Task::resumption`; it must never become an implicit fresh competition.
pub fn release_task(task: &mut crate::queue::Task, source: &str, question: Option<&str>) {
    release_loaded(task, source, question, RunState::load);
}

fn release_loaded(
    task: &mut crate::queue::Task,
    source: &str,
    question: Option<&str>,
    load: impl FnOnce(&str) -> anyhow::Result<RunState>,
) {
    // An explicit revision is separate authority; keep the established edit
    // workflow rather than treating its earlier run as the requested work.
    let revised = task
        .resumption
        .as_ref()
        .is_some_and(|r| r.instruction != task.instruction);
    let prior = task.runs.last().map(|id| load(id));
    task.release_operator(source, question);
    if revised || task.approved_review {
        return;
    }
    let Some(prior) = prior else { return };
    match prior {
        Err(e) => task.hold_machine(Some(format!(
            "The prior execution record cannot be read: {e:#}. The approved continuation is retained; restore the record and release this task. No new competition was started."
        ))),
        Ok(state) if crate::daemon::compatible_run_instruction(&state, task) => {
            match classify(&state) {
                Continuation::RepairAndReview { branch } => task.request_approved_review(branch),
                Continuation::Blocked { precondition, continuation } if state.status.resumable() => {
                    task.hold_machine(Some(format!("{precondition} {continuation}")));
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Diagnose a task's prior work before offering release. A temporary live
/// driver is a scheduling wait, rather than a permanently unusable choice.
pub(crate) fn task_offer_block(task: &crate::queue::Task) -> Option<String> {
    if task.approved_review
        || task.runs.is_empty()
        || task
            .resumption
            .as_ref()
            .is_some_and(|r| r.instruction != task.instruction)
    {
        return None;
    }
    let id = task.runs.last()?;
    match RunState::load(id) {
        Ok(state) if crate::daemon::compatible_run_instruction(&state, task) => {
            if state.status.resumable() && state.liveness(false) == crate::run::Liveness::Live {
                return None;
            }
            match classify(&state) {
                Continuation::Blocked {
                    precondition,
                    continuation,
                } if state.status.resumable() => Some(format!("{precondition} {continuation}")),
                _ => None,
            }
        }
        Ok(_) => None,
        Err(e) => Some(format!(
            "The prior execution record cannot be read: {e:#}. Restore it before continuing; the task and its work remain held."
        )),
    }
}

/// Offer-time revalidation: drop every `resume` choice whose run `classify`
/// says nothing can continue (or whose record cannot be read), so an action
/// the daemon's admission would reject is never offered. A resume that maps to
/// a bounded repair-and-review stays offered. Keep a safe hold when no choice
/// is left and explain the blocking precondition in the question.
pub fn revalidate_offer(q: &mut crate::ask::Question) {
    let stale: Vec<(String, String)> = q
        .actions
        .iter()
        .filter_map(|(label, action)| match action {
            crate::ask::ChoiceAction::Resume { run } => {
                let reason = match RunState::load(run) {
                    Ok(s) if s.status.resumable()
                        && s.liveness(false) == crate::run::Liveness::Live => return None,
                    Ok(s) => match classify(&s) {
                        Continuation::Blocked { precondition, continuation } =>
                            format!("{precondition} {continuation}"),
                        _ => return None,
                    },
                    Err(e) => format!("The execution record cannot be read: {e:#}. Restore it before continuing; existing work remains held."),
                };
                Some((label.clone(), reason))
            }
            _ => None,
        })
        .collect();
    let removed = !stale.is_empty();
    for (label, reason) in stale {
        q.actions.remove(&label);
        q.choices.retain(|c| c != &label);
        q.detail.push_str(&format!("\n\n{reason}"));
    }
    if removed && q.choices.is_empty() {
        crate::choice_effect::safe_hold(q);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::{Candidate, ReviewRound};
    use std::path::PathBuf;

    #[test]
    fn owner_release_preserves_blocked_work_and_evidence() {
        use crate::queue::{Source, Task, TaskStatus};
        for unreadable in [false, true] {
            let mut run = state(RunStatus::Blocked);
            with_winner(&mut run);
            run.reviews.push(round(0, 2, true));
            run.gate_ran = true;
            let mut task = Task::new(
                "work".into(),
                run.instruction.clone(),
                PathBuf::from("/repo"),
                Source::Human,
            );
            task.start(run.id.clone());
            task.hold_machine(Some("gate failed".into()));
            let history = task.runs.clone();
            release_loaded(&mut task, "cli", None, |_| {
                if unreadable {
                    anyhow::bail!("record unavailable")
                }
                Ok(run)
            });
            assert_eq!(task.status, TaskStatus::Held);
            assert_eq!(task.runs, history);
            assert_eq!(task.resumption.as_ref().unwrap().source, "cli");
            assert!(
                task.hold_reason
                    .as_deref()
                    .unwrap()
                    .contains(if unreadable {
                        "record unavailable"
                    } else {
                        "verification"
                    })
            );
        }
    }

    #[test]
    fn owner_release_selects_bounded_review_and_retains_target_until_start() {
        use crate::queue::{Source, Task, TaskStatus};
        let mut run = state(RunStatus::Blocked);
        with_winner(&mut run);
        run.reviews.push(round(1, 2, false));
        let mut task = Task::new(
            "work".into(),
            run.instruction.clone(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        task.start(run.id.clone());
        task.hold_machine(None);
        release_loaded(&mut task, "cli", None, |_| Ok(run));
        assert!(task.approved_review);
        assert_eq!(task.status, TaskStatus::Queued);
        for parked in [false, true] {
            if parked {
                task.park("resource wait");
            } else {
                task.hold_machine(Some("resource wait".into()));
            }
            // Simulate store/reload between a failed startup and triage release.
            task = serde_json::from_str(&serde_json::to_string(&task).unwrap()).unwrap();
            task.release_operator("triage", Some("question"));
            assert!(task.approved_review);
            assert_eq!(task.review_branch.as_deref(), Some("magi/task-1"));
        }
        task.start("review-run".into());
        assert!(!task.approved_review);
        assert!(task.review_branch.is_none());
        assert_eq!(task.runs.len(), 2);
    }

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

    #[test]
    fn a_temporary_live_driver_does_not_remove_the_recovery_choice() {
        crate::run::pin_test_home();
        let mut run = state(RunStatus::Blocked);
        run.driver_exited = false;
        run.begin_pass();
        run.save().unwrap();
        let mut driver = crate::driver::DriverGuard::acquire(&crate::run::home(), &run).unwrap();
        driver.record(&run).unwrap();
        assert_eq!(run.liveness(false), crate::run::Liveness::Live);
        let mut q = crate::ask::Question::new(
            "task".into(),
            "conduct".into(),
            "conductor".into(),
            "continue".into(),
            "".into(),
            vec!["resume".into()],
        );
        q.actions.insert(
            "resume".into(),
            crate::ask::ChoiceAction::Resume {
                run: run.id.clone(),
            },
        );
        revalidate_offer(&mut q);
        assert!(q.actions.contains_key("resume"));
        drop(driver);
        revalidate_offer(&mut q);
        assert!(q.actions.contains_key("resume"));
    }

    #[test]
    fn stale_resume_offer_is_replaced_after_verification_state_changes() {
        crate::run::pin_test_home();
        let mut run = state(RunStatus::Blocked);
        with_winner(&mut run);
        run.save().unwrap();
        let mut q = crate::ask::Question::new(
            "task".into(),
            "conduct".into(),
            "conductor".into(),
            "continue".into(),
            "".into(),
            vec!["resume".into()],
        );
        q.actions.insert(
            "resume".into(),
            crate::ask::ChoiceAction::Resume {
                run: run.id.clone(),
            },
        );
        revalidate_offer(&mut q);
        assert!(q.actions.contains_key("resume"));
        run.reviews.push(round(0, 2, true));
        run.gate_ran = true;
        run.save().unwrap();
        revalidate_offer(&mut q);
        assert!(!q.actions.contains_key("resume"));
        assert_eq!(
            q.actions.get(&q.choices[0]),
            Some(&crate::ask::ChoiceAction::Hold)
        );
    }

    #[test]
    fn removing_the_last_choice_leaves_a_safe_hold() {
        let mut q = crate::ask::Question::new(
            "t".into(),
            "conduct".into(),
            "conductor".into(),
            "s".into(),
            "d".into(),
            vec!["resume".into()],
        );
        q.actions.insert(
            "resume".into(),
            crate::ask::ChoiceAction::Resume { run: "gone".into() },
        );
        revalidate_offer(&mut q);
        assert_eq!(q.choices.len(), 1);
        assert_eq!(
            q.actions.get(&q.choices[0]),
            Some(&crate::ask::ChoiceAction::Hold)
        );
    }
}
