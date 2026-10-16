//! Admission of operator text: a saved deputy is history, not a listener.
//! Called under the question update lock as well as when rendering its card.
use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::ask::{Question, Questions};
use crate::config::Config;

/// Recovery capabilities of the configuration actually held by the daemon.
/// Contains no commands, environment values, or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recovery {
    max_deputies: usize,
    deputy_agents: Vec<String>,
    deputy_fallback: bool,
    /// Default timeout used by the running recovery loops.
    pub answer_timeout: u64,
}

impl Recovery {
    /// Snapshot the same configuration and capacity passed to `Deputies`.
    pub fn new(cfg: Option<&Config>, max_deputies: usize) -> Self {
        Self {
            max_deputies,
            deputy_agents: cfg.map_or_else(Vec::new, |c| {
                c.agents.iter().map(|a| a.id.clone()).collect()
            }),
            deputy_fallback: cfg.is_some_and(|c| c.resolve_roles().is_ok()),
            answer_timeout: cfg.map_or_else(
                || Config::default().graph.answer_timeout,
                |c| c.graph.answer_timeout,
            ),
        }
    }

    fn can_start(&self, agent: &str) -> bool {
        self.max_deputies > 0
            && (self.deputy_fallback
                || (!agent.is_empty() && self.deputy_agents.iter().any(|id| id == agent)))
    }
}

/// Stable refusal reasons, independent of the display language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// The question is already closed.
    Settled,
    /// The deputy has used its start budget.
    DeputyExhausted,
    /// Deputy configuration or its agent is unavailable.
    DeputyDisabled,
    /// The recipient deadline has passed.
    Deadline,
    /// The original agent cannot resume.
    RecipientUnavailable,
    /// No live daemon can recover the recipient.
    DaemonUnavailable,
}

impl UnavailableReason {
    /// Existing language-independent API reason code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::DeputyExhausted => "deputy_exhausted",
            Self::DeputyDisabled => "deputy_disabled",
            Self::Deadline => "deadline",
            Self::RecipientUnavailable => "recipient_unavailable",
            Self::DaemonUnavailable => "daemon_unavailable",
        }
    }

    fn message(self, ja: bool) -> &'static str {
        let (en, jp) = match self {
            Self::Settled => (
                "This question is already settled.",
                "この質問は終了しています。",
            ),
            Self::DeputyExhausted => (
                "Free-text replies are unavailable: the deputy has used all its starts.",
                "自由文を送信できません。代理エージェントの起動回数を使い切りました。",
            ),
            Self::DeputyDisabled => (
                "Free-text replies are unavailable: deputies are disabled or their configuration or agent is unavailable.",
                "自由文を送信できません。代理エージェントが無効か、設定またはエージェントを利用できません。",
            ),
            Self::Deadline => (
                "Free-text replies are unavailable: the recipient's deadline has passed.",
                "自由文を送信できません。受信先の応答期限を過ぎています。",
            ),
            Self::RecipientUnavailable => (
                "Free-text replies are unavailable: the asking agent has no resumable session.",
                "自由文を送信できません。質問したエージェントのセッションを再開できません。",
            ),
            Self::DaemonUnavailable => (
                "Free-text replies are unavailable: no running daemon can recover the recipient.",
                "自由文を送信できません。受信先を復旧するデーモンが稼働していません。",
            ),
        };
        if ja { jp } else { en }
    }
}

/// Typed refusal shared by web and command-line ingress.
#[derive(Debug, thiserror::Error)]
#[error("question_text_unavailable ({code}): {message}", code = .reason.code())]
pub struct TextUnavailable {
    /// Machine-readable refusal reason.
    pub reason: UnavailableReason,
    /// Localized explanation for the operator.
    pub message: String,
}

/// Current delivery state projected to the question card.
#[derive(Debug, Clone, Serialize)]
pub struct Availability {
    /// `live`, automatically recoverable `waiting`, or `unavailable`.
    pub state: &'static str,
    /// Stable reason code, independent of the UI language.
    pub reason: &'static str,
    /// Explanation in the question or configured language.
    pub message: String,
    #[serde(skip)]
    unavailable_reason: Option<UnavailableReason>,
}

impl Availability {
    /// Whether an operator message can reach a live or recoverable recipient.
    pub fn accepts_text(&self) -> bool {
        self.state != "unavailable"
    }
}

/// The same heartbeat and upgrade lease readers used by the daemon UI.
fn recovering(home: &Path, now: Timestamp, cfg: Option<&Config>) -> Option<Recovery> {
    let daemon = crate::daemon::read_status(home)?;
    let handover = crate::updater::read_progress(home).is_some_and(|p| {
        matches!(
            p.stage,
            crate::updater::Stage::Parking | crate::updater::Stage::Restarting
        ) && crate::updater::live_lease(&p, crate::updater::read_lease(home).as_ref(), now)
            .is_some()
    });
    let running = daemon.running(now);
    let recovery = daemon.question_recovery?;
    if running {
        return Some(recovery);
    }
    // During handover a successor has not published its capabilities yet.
    // Do not promise recovery if prospective settings changed in the gap.
    if handover && cfg.is_some_and(|c| recovery == Recovery::new(Some(c), c.daemon.max_deputies)) {
        Some(recovery)
    } else {
        None
    }
}

/// Inspect current leases and the existing deputy/waiter recovery prerequisites.
pub fn availability(store: &Questions, q: &Question, cfg: Option<&Config>) -> Availability {
    let now = Timestamp::now();
    let home = store.root().parent().unwrap_or(store.root());
    let ja = q.merge_target.as_ref().map_or_else(
        || cfg.is_some_and(|c| c.graph.language == "ja"),
        |t| t.language == "ja",
    );
    let result = |state, reason, en: &str, jp: &str| Availability {
        state,
        reason,
        unavailable_reason: None,
        message: if ja { jp } else { en }.to_owned(),
    };
    let unavailable = |reason: UnavailableReason| Availability {
        state: "unavailable",
        reason: reason.code(),
        message: reason.message(ja).to_owned(),
        unavailable_reason: Some(reason),
    };
    if !q.status.open() {
        return unavailable(UnavailableReason::Settled);
    }
    if store.read_lease(&q.id).is_some_and(|l| l.fresh(now)) {
        return result(
            "live",
            "lease",
            if q.waiting_on_agent() {
                "Waiting for the agent to reply."
            } else {
                "An agent is listening."
            },
            if q.waiting_on_agent() {
                "エージェントの返答を待っています。"
            } else {
                "エージェントが応答を受け取れます。"
            },
        );
    }
    let recovery = recovering(home, now, cfg);
    let timeout = recovery.as_ref().map_or_else(
        || Config::default().graph.answer_timeout,
        |r| r.answer_timeout,
    );
    if crate::deputy::kind_of(q).is_some() {
        if q.deputy
            .as_ref()
            .is_some_and(|d| d.starts >= crate::deputy::MAX_STARTS)
        {
            return unavailable(UnavailableReason::DeputyExhausted);
        }
        let Some(recovery) = recovery.as_ref() else {
            return unavailable(UnavailableReason::DaemonUnavailable);
        };
        if !recovery.can_start(crate::deputy::agent_of(q)) {
            return unavailable(UnavailableReason::DeputyDisabled);
        }
        if !crate::deputy::restart_in_time(q, timeout, now) {
            return unavailable(UnavailableReason::Deadline);
        }
    } else {
        let waiter = crate::waiter::Waiter::new(store.clone(), home.to_path_buf(), cfg.cloned());
        if q.cwd.is_none() || !waiter.can_resume(q) {
            return unavailable(UnavailableReason::RecipientUnavailable);
        }
        if crate::waiter::decide(q, None, false, timeout, now) == crate::waiter::Action::Expire {
            return unavailable(UnavailableReason::Deadline);
        }
    }
    if recovery.is_some() {
        result(
            "waiting",
            "recovery",
            "Waiting for automatic delivery: the recipient is starting, restarting, or waiting for a free slot.",
            "自動配送待ちです。受信先の起動・再起動、または実行枠の空きを待っています。",
        )
    } else {
        unavailable(UnavailableReason::DaemonUnavailable)
    }
}

/// Must run against the latest question inside `Questions::update`. A refusal
/// returns before changing the thread, answer, or delivery cursor.
pub fn require(store: &Questions, q: &Question, cfg: Option<&Config>) -> Result<()> {
    let delivery = availability(store, q, cfg);
    if let Some(reason) = delivery.unavailable_reason {
        return Err(TextUnavailable {
            reason,
            message: delivery.message,
        }
        .into());
    }
    Ok(())
}
