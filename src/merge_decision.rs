//! Persisted owner decisions at the merge gate. Tokens are independent of UI prose.

use serde::{Deserialize, Serialize};

/// Explicit closure confirmation, also usable as a CLI choice.
pub const WITHDRAW: &str = "withdraw: close PR and end task";
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
        || selected_agent(choice).is_some()
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
