//! Task/chat-native decision lifecycle. Delivery, reading and conversation turns
//! are deliberately absent: only an effect receipt or explicit settlement ends
//! a decision. This record travels with the durable inquiry, not a UI card.
use anyhow::{Result, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// State of the requested decision effect, independent of message transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The owner has not requested an operation.
    AwaitingOwner,
    /// An owner request is recorded; the consumer has not confirmed its effect.
    Queued,
    /// The consumer has begun the operation.
    InProgress,
    /// A failure or uncertain outcome needs reconciliation before retry.
    RetryNeeded,
    /// A durable receipt confirms the effect, or explicit settlement ends it.
    Resolved,
}

/// An append-only explanation of a state change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// Time recorded, never used as approval or resolution evidence.
    pub at: Timestamp,
    /// Resulting lifecycle status.
    pub status: Status,
    /// Owner request, effect receipt or settlement evidence.
    pub evidence: String,
}

/// Durable decision state embedded in an inquiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    /// Concise reason shown on a phone without opening the conversation.
    pub reason: String,
    /// Current effect status.
    pub status: Status,
    /// Recorded owner request; migration never manufactures one.
    pub request: Option<String>,
    /// Current actionable error, including uncertainty about an external call.
    pub error: Option<String>,
    /// The consumer proved that the last attempt failed before any effect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_safe: bool,
    /// Exact source response generation accepted by a fresh owner command.
    /// Migration never fills this field from a saved reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_answer_at: Option<Timestamp>,
    /// Explicit owner confirmation that an accepted legacy effect has no
    /// remaining work. Operational source readers must not consume it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy_settled: bool,
    /// Complete lifecycle history; never pruned by delivery or read receipts.
    pub history: Vec<Event>,
}

impl Decision {
    /// File an inquiry which requires owner input.
    pub fn awaiting(reason: String) -> Self {
        Self {
            reason,
            status: Status::AwaitingOwner,
            request: None,
            error: None,
            retry_safe: false,
            source_answer_at: None,
            legacy_settled: false,
            history: vec![],
        }
    }

    fn event(&mut self, status: Status, evidence: &str) -> bool {
        if self.status == status && self.history.last().is_some_and(|e| e.evidence == evidence) {
            return false;
        }
        self.status = status;
        self.history.push(Event {
            at: Timestamp::now(),
            status,
            evidence: evidence.to_owned(),
        });
        true
    }

    /// Record acceptance, without claiming that the command has been applied.
    pub fn accept(&mut self, request: &str) -> Result<bool> {
        ensure!(!request.trim().is_empty(), "decision request is empty");
        if let Some(prior) = &self.request {
            ensure!(
                prior == request,
                "decision already binds a different request"
            );
            return Ok(false);
        }
        ensure!(
            matches!(self.status, Status::AwaitingOwner | Status::RetryNeeded),
            "decision needs reconciliation"
        );
        self.request = Some(request.to_owned());
        self.error = None;
        Ok(self.event(Status::Queued, request))
    }

    /// The consumer begins the accepted effect; no automatic retry of uncertainty.
    pub fn start(&mut self) -> Result<bool> {
        ensure!(self.status == Status::Queued, "decision is not queued");
        ensure!(self.request.is_some(), "decision has no owner request");
        self.retry_safe = false;
        Ok(self.event(Status::InProgress, "effect consumer started"))
    }

    /// The command was accepted but a worker/forge effect is still outstanding.
    pub(crate) fn queued(&mut self) -> Result<bool> {
        ensure!(
            self.request.is_some() && self.status == Status::InProgress,
            "decision is not executing an owner request"
        );
        Ok(self.event(Status::Queued, "command accepted; effect not yet confirmed"))
    }

    /// Retain an unsuccessful or uncertain operation in the active list.
    pub fn fail(&mut self, error: &str) -> Result<bool> {
        ensure!(
            self.status != Status::Resolved,
            "decision is already resolved"
        );
        ensure!(!error.trim().is_empty(), "decision error is empty");
        self.error = Some(error.to_owned());
        self.retry_safe = false;
        Ok(self.event(Status::RetryNeeded, error))
    }

    /// Record proof that no operation began. This is distinct from uncertainty
    /// after a write or external call; only an owner retry may requeue it.
    pub(crate) fn fail_before_effect(&mut self, error: &str) -> Result<bool> {
        let prior = self.retry_safe;
        let changed = self.fail(error)?;
        self.retry_safe = true;
        Ok(changed || !prior)
    }

    /// Requeue the same owner request after a proven pre-execution failure.
    pub(crate) fn retry_unapplied(&mut self) -> Result<bool> {
        ensure!(
            self.status == Status::RetryNeeded && self.retry_safe && self.request.is_some(),
            "effect needs receipt reconciliation before retry"
        );
        self.error = None;
        self.retry_safe = false;
        Ok(self.event(
            Status::Queued,
            "owner retried a request proven not to have started",
        ))
    }

    /// Called only after rereading the operation's authoritative durable receipt.
    /// Also repairs a crash between application and updating this projection.
    pub(crate) fn applied(&mut self, receipt: &str) -> Result<bool> {
        ensure!(!receipt.trim().is_empty(), "effect receipt is empty");
        if self.status == Status::Resolved {
            return Ok(false);
        }
        self.error = None;
        self.retry_safe = false;
        Ok(self.event(Status::Resolved, receipt))
    }

    /// Owner settlement is distinct from an application receipt. Legacy
    /// consumers cannot always prove their external outcome; the owner may
    /// explicitly confirm that no effect remains after inspecting it.
    pub(crate) fn settle_legacy(&mut self, reason: &str) -> Result<bool> {
        ensure!(
            self.request
                .as_deref()
                .is_some_and(|r| r.starts_with("decision: ")),
            "only an accepted legacy source decision supports this settlement"
        );
        ensure!(
            self.status != Status::InProgress,
            "effect consumer is still executing"
        );
        ensure!(!reason.trim().is_empty(), "settlement reason is empty");
        if self.status == Status::Resolved {
            return Ok(false);
        }
        self.legacy_settled = true;
        self.error = None;
        self.retry_safe = false;
        Ok(self.event(Status::Resolved, reason))
    }

    /// Explicitly settle discussion without an operation, or supersede it with
    /// another inquiry. A queued/uncertain effect must first be reconciled by
    /// its consumer; an owner reply alone cannot hide it.
    pub fn settle(&mut self, reason: &str) -> Result<bool> {
        ensure!(
            self.status == Status::AwaitingOwner
                || (self.status == Status::RetryNeeded
                    && (self.request.is_none() || self.retry_safe)),
            "an accepted effect requires receipt reconciliation"
        );
        ensure!(!reason.trim().is_empty(), "settlement reason is empty");
        Ok(self.event(Status::Resolved, reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_is_not_application_and_history_explains_completion() {
        let mut decision = Decision::awaiting("Review the publication".into());
        decision.accept("owner requested publication").unwrap();
        assert_eq!(decision.status, Status::Queued);
        assert!(decision.settle("read the message").is_err());
        decision.start().unwrap();
        assert_eq!(decision.status, Status::InProgress);
        decision
            .applied("publication receipt: PR 42 confirmed")
            .unwrap();
        assert_eq!(decision.status, Status::Resolved);
        assert_eq!(decision.history.len(), 3);
        assert!(!decision.applied("same receipt after restart").unwrap());
    }

    #[test]
    fn uncertain_effect_cannot_be_blindly_replayed_or_settled() {
        let mut decision = Decision::awaiting("Retry checks".into());
        decision.accept("rerun request 12").unwrap();
        decision.start().unwrap();
        decision
            .fail("network lost after request: inspect receipt 12")
            .unwrap();
        let saved = serde_json::to_vec(&decision).unwrap();
        let mut restarted: Decision = serde_json::from_slice(&saved).unwrap();
        assert_eq!(restarted.status, Status::RetryNeeded);
        assert!(restarted.start().is_err());
        assert!(restarted.settle("reply was read").is_err());
        assert!(restarted.accept("a different request").is_err());
        restarted
            .applied("receipt 12: execution confirmed by forge")
            .unwrap();
        assert_eq!(restarted.status, Status::Resolved);
        assert!(restarted.error.is_none());
    }

    #[test]
    fn explicit_settlement_requires_a_reason_and_executes_nothing() {
        let mut decision = Decision::awaiting("Choose a direction".into());
        assert!(decision.settle("").is_err());
        decision
            .settle("owner explicitly confirmed no change is necessary")
            .unwrap();
        assert_eq!(decision.status, Status::Resolved);
        assert!(decision.request.is_none());
    }
}
