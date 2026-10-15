//! Admission of operator text: a saved deputy is history, not a listener.
//! Called under the question update lock as well as when rendering its card.
use std::path::Path;

use anyhow::{Result, bail};
use jiff::Timestamp;
use serde::Serialize;

use crate::ask::{Question, Questions};
use crate::config::Config;

/// Current delivery state projected to the question card.
#[derive(Debug, Clone, Serialize)]
pub struct Availability {
    /// `live`, automatically recoverable `waiting`, or `unavailable`.
    pub state: &'static str,
    /// Stable reason code, independent of the UI language.
    pub reason: &'static str,
    /// Explanation in the question or configured language.
    pub message: String,
}

impl Availability {
    /// Whether an operator message can reach a live or recoverable recipient.
    pub fn accepts_text(&self) -> bool {
        self.state != "unavailable"
    }
}

/// The same heartbeat and upgrade lease readers used by the daemon UI.
fn recovering(home: &Path, now: Timestamp) -> bool {
    crate::daemon::read_status(home).is_some_and(|s| s.running(now))
        || crate::updater::read_progress(home).is_some_and(|p| {
            matches!(
                p.stage,
                crate::updater::Stage::Parking | crate::updater::Stage::Restarting
            ) && crate::updater::live_lease(&p, crate::updater::read_lease(home).as_ref(), now)
                .is_some()
        })
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
        message: if ja { jp } else { en }.to_owned(),
    };
    if !q.status.open() {
        return result(
            "unavailable",
            "settled",
            "This question is already settled.",
            "この質問は終了しています。",
        );
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
    let timeout = cfg.map_or_else(
        || Config::default().graph.answer_timeout,
        |c| c.graph.answer_timeout,
    );
    if crate::deputy::kind_of(q).is_some() {
        if q.deputy
            .as_ref()
            .is_some_and(|d| d.starts >= crate::deputy::MAX_STARTS)
        {
            return result(
                "unavailable",
                "deputy_exhausted",
                "Free-text replies are unavailable: the deputy has used all its starts.",
                "自由文を送信できません。代理エージェントの起動回数を使い切りました。",
            );
        }
        if !crate::deputy::can_start(cfg, crate::deputy::agent_of(q)) {
            return result(
                "unavailable",
                "deputy_disabled",
                "Free-text replies are unavailable: deputies are disabled or their configuration or agent is unavailable.",
                "自由文を送信できません。代理エージェントが無効か、設定またはエージェントを利用できません。",
            );
        }
        if !crate::deputy::restart_in_time(q, timeout, now) {
            return result(
                "unavailable",
                "deadline",
                "Free-text replies are unavailable: the recipient's deadline has passed.",
                "自由文を送信できません。受信先の応答期限を過ぎています。",
            );
        }
    } else {
        let waiter = crate::waiter::Waiter::new(store.clone(), home.to_path_buf(), cfg.cloned());
        if q.cwd.is_none() || !waiter.can_resume(q) {
            return result(
                "unavailable",
                "recipient_unavailable",
                "Free-text replies are unavailable: the asking agent has no resumable session.",
                "自由文を送信できません。質問したエージェントのセッションを再開できません。",
            );
        }
        if crate::waiter::decide(q, None, false, timeout, now) == crate::waiter::Action::Expire {
            return result(
                "unavailable",
                "deadline",
                "Free-text replies are unavailable: the recipient's deadline has passed.",
                "自由文を送信できません。受信先の応答期限を過ぎています。",
            );
        }
    }
    if recovering(home, now) {
        result(
            "waiting",
            "recovery",
            "Waiting for automatic delivery: the recipient is starting, restarting, or waiting for a free slot.",
            "自動配送待ちです。受信先の起動・再起動、または実行枠の空きを待っています。",
        )
    } else {
        result(
            "unavailable",
            "daemon_unavailable",
            "Free-text replies are unavailable: no running daemon can recover the recipient.",
            "自由文を送信できません。受信先を復旧するデーモンが稼働していません。",
        )
    }
}

/// Must run against the latest question inside `Questions::update`. A refusal
/// returns before changing the thread, answer, or delivery cursor.
pub fn require(store: &Questions, q: &Question, cfg: Option<&Config>) -> Result<()> {
    let delivery = availability(store, q, cfg);
    if !delivery.accepts_text() {
        bail!(
            "question_text_unavailable ({}): {}",
            delivery.reason,
            delivery.message
        );
    }
    Ok(())
}
