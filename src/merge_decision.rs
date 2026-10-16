//! Persisted owner decisions at the merge gate. Tokens are independent of UI prose.

use serde::{Deserialize, Serialize};

/// Explicit closure confirmation, also usable as a CLI choice.
pub const WITHDRAW: &str = "withdraw: close PR and end task";
/// Retire obsolete local work without changing the PR on the forge.
pub const DISMISS: &str = "dismiss: end obsolete approval without changing PR";
/// Prefix followed by the exact configured agent id.
pub const REIMPLEMENT: &str = "reimplement:";

/// The target displayed by an approval card, retained across legacy upgrades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// PR URL at the time of the decision.
    pub pr: String,
    /// Exact head displayed in the decision panel.
    pub head: String,
    /// Operator language used by the card.
    pub language: String,
}

/// A stop decision survives head movement and driver restarts. The question
/// retains the owner answer; this record checkpoints its application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    /// Approval question id and operation idempotency key.
    pub question: String,
    /// Immutable PR and displayed head binding.
    pub target: Target,
    /// Exact owner choice token, including the selected implementer.
    pub choice: String,
    #[serde(default)]
    /// True only after the forge confirms the PR is closed without merging.
    pub closed: bool,
    #[serde(default)]
    /// New attempt id, recorded before dispatching its agents.
    pub replacement_run: Option<String>,
    #[serde(default)]
    /// New PR URL, once the replacement publishes it.
    pub replacement_pr: Option<String>,
}

/// Same task and attachments, a new attempt with an immutable implementer pin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replacement {
    /// Approval question id and operation idempotency key.
    pub question: String,
    /// Previous execution, which must have stopped before dispatch.
    pub run: String,
    /// PR URL at the time of the decision.
    pub pr: String,
    /// Configured implementer id; never substituted by fallback.
    pub agent: String,
    /// Review evidence carried alongside the original instruction.
    pub context: String,
}

/// Extract the explicit implementer from a choice token.
pub fn selected_agent(choice: &str) -> Option<&str> {
    choice.strip_prefix(REIMPLEMENT).filter(|id| !id.is_empty())
}

/// Whether this choice must stop landing before any further mutations.
pub fn stops(choice: &str) -> bool {
    choice.eq_ignore_ascii_case(crate::land::HOLD)
        || choice == WITHDRAW
        || choice == DISMISS
        || selected_agent(choice).is_some()
}

/// Apply a dismissal while the question writer holds its lock. Unlike withdrawal,
/// this needs no forge access or queue owner. Claims serialize with a restarting
/// daemon; the publication lock serializes with standalone driver takeover.
/// Persist the stop before settling the answer so a partial write fails closed
/// and retrying the same question completes the operation.
pub(crate) fn dismiss(home: &std::path::Path, q: &crate::ask::Question) -> anyhow::Result<()> {
    use crate::run::{Liveness, RunState, RunStatus};
    use anyhow::ensure;

    ensure!(
        q.node == crate::land::APPROVAL_NODE,
        "only merge approvals can be dismissed"
    );
    let queue = crate::queue::Queue::at(home.join("queue"));
    let owners: Vec<_> = queue
        .list()
        .into_iter()
        .filter(|t| t.runs.contains(&q.run))
        .collect();
    let mut claims = Vec::new();
    for task in &owners {
        claims.push(queue.claim(&task.id)?);
    }
    let _lock = crate::pr_head::owner_lock(home, &q.run)?;
    let mut state = RunState::load_under(&q.run, home)?;
    ensure!(
        state.liveness(false) == Liveness::Dead,
        "stop the current driver before dismissing this obsolete approval"
    );
    ensure!(
        state.land_armed_head.is_none(),
        "disable existing PR auto-merge before dismissing this approval"
    );
    let binding = state
        .land_approval
        .as_ref()
        .filter(|b| b.question == q.id)
        .ok_or_else(|| anyhow::anyhow!("this is no longer the run's approval question"))?;
    let pr = state
        .pr
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("approval has no recorded PR"))?;
    let target = q.merge_target.clone().unwrap_or_else(|| Target {
        pr: pr.url.clone(),
        head: binding.head.clone(),
        language: state.config.graph.language.clone(),
    });
    ensure!(
        target.pr == pr.url && target.head == binding.head,
        "approval target does not match the recorded run"
    );
    ensure!(
        state
            .merge_decision
            .as_ref()
            .is_none_or(|d| d.question == q.id && d.choice == DISMISS),
        "another owner decision already controls this run"
    );
    if state.merge_decision.is_none() {
        state.merge_decision = Some(Decision {
            question: q.id.clone(),
            target,
            choice: DISMISS.into(),
            closed: false,
            replacement_run: None,
            replacement_pr: None,
        });
        if !state.status.done() {
            state.status = RunStatus::Superseded;
        }
        state.parked = false;
        state.event(
            "land",
            "Owner dismissed obsolete approval; execution retired, PR unchanged",
        );
        state.save_under(home)?;
    }
    for owner in owners {
        let mut task = queue.get(&owner.id)?;
        // A later attempt or an already completed task owns its own outcome.
        if task.runs.last() == Some(&q.run) && task.status != crate::queue::TaskStatus::Done {
            task.succeed();
            task.mark_action_applied(&q.id);
            queue.put(&mut task)?;
        }
    }
    Ok(())
}

/// Only an applied hold can be reopened by an explicit run/task resume.
/// Failed closure and replacement decisions retain their stop checkpoint.
pub(crate) fn resumable_hold(state: &crate::run::RunState) -> bool {
    state.status == crate::run::RunStatus::Blocked
        && state.merge_decision.as_ref().is_some_and(|decision| {
            !decision.closed && decision.choice.eq_ignore_ascii_case(crate::land::HOLD)
        })
}

/// Chat consultation may request a stop/repair only with the exact latest
/// choice. Closing a PR explicitly names the consequence in its token.
pub fn exact_owner_choice(choice: &str, message: &str, quote: &str) -> bool {
    if choice.eq_ignore_ascii_case(crate::land::HOLD) {
        message.trim().eq_ignore_ascii_case(choice) && quote.trim().eq_ignore_ascii_case(choice)
    } else {
        (stops(choice) || choice == crate::land::REPAIR)
            && message.trim() == choice
            && quote.trim() == choice
    }
}
