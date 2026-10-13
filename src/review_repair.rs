//! Durable requests for bounded repair of findings at the merge gate.
//! Old review evidence stays immutable; no merge permission is carried forward.

use serde::{Deserialize, Serialize};

use crate::run::{CommandOutcome, GateFixRecord, ReviewRound};

/// One owner-requested repair, keyed by the approval question. Old evidence
/// remains immutable while the active review budget starts afresh on this run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Approval question that uniquely requested this operation.
    pub question: String,
    /// Exact PR head shown to the owner.
    pub head: String,
    /// Existing PR URL; repair never creates another PR.
    pub pr: String,
    /// Immutable review evidence preceding this repair, including final votes.
    pub reviews: Vec<ReviewRound>,
    /// Verification gate evidence preceding this repair.
    pub gate: Vec<CommandOutcome>,
    /// Commit the previous gate checked.
    #[serde(default)]
    pub gate_head: Option<String>,
    /// Previous bounded gate repairs, preserved before starting a new budget.
    #[serde(default)]
    pub gate_fixes: Vec<GateFixRecord>,
    /// Previous pre-gate command evidence.
    #[serde(default)]
    pub pre_gate: Vec<CommandOutcome>,
    /// Durable checkpoint used to avoid repeating an interrupted fix.
    pub phase: Phase,
    /// Why repair could not continue, with operator recovery guidance.
    pub failure: Option<String>,
}

/// Durable stages of a bounded repair at the merge gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Recorded before beginning work.
    Requested,
    /// Fixer dispatched; interruption rolls back before holding, never retries the fixer.
    Fixing,
    /// Fix finished (including no change); fresh review and gate are underway.
    Reviewing,
    /// Updated review and gate completed; a fresh merge decision is required.
    Complete,
    /// Repair held with explicit guidance.
    Failed,
}
