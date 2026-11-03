//! Display-only progress from structured records. Phase order is never evidence
//! that an earlier phase succeeded. This view cannot authorize an operation.
use crate::run::{E2eStatus, GateStatus, Liveness, RunState};
use serde::Serialize;

#[derive(Debug, Serialize)]
/// Shared presentation for list cards and run details.
pub struct Progress {
    /// Current activity, including waits and driver exit.
    pub activity: String,
    /// Ordered phases; ordering implies no success.
    pub phases: Vec<Phase>,
    /// Recorded rounds only; no guessed in-flight round number.
    pub rounds: Vec<ReviewProgress>,
}

#[derive(Debug, Serialize)]
/// A stage with its own evidence, independent of adjacent stages.
pub struct Phase {
    /// Stable DOM identity.
    pub key: &'static str,
    /// Human readable stage name.
    pub label: &'static str,
    /// Display state: current, paused, complete, failed, recorded, skipped, unrecorded, unknown or upcoming.
    pub state: &'static str,
    /// Recorded results or active seat names.
    pub detail: String,
}

/// A recorded review attempt, including verification and panel completeness.
#[derive(Debug, Serialize)]
pub struct ReviewProgress {
    /// The persisted round number.
    pub round: usize,
    /// Historical result; does not claim verification of a later head.
    pub detail: String,
}

fn phase(node: &str) -> &'static str {
    match node {
        "prep" => "prep",
        "advise" | "advise-synthesis" => "advice",
        "implement" | "implementing" => "implement",
        "judge" | "judging" | "deliberate" | "deliberating" | "vote" | "voting" => "selection",
        "review" | "reviewing" | "fix" | "verify" => "review",
        "gate" | "gating" | "gate-fix" => "gate",
        "land" | "landing" | "merge" => "land",
        _ => "other",
    }
}

fn implementation_results(s: &RunState) -> Option<String> {
    let n = s
        .candidates
        .iter()
        .filter(|c| c.duration_ms > 0 || c.failed.is_some() || c.verified_noop.is_some())
        .count();
    (n > 0).then(|| format!("{n} implementation results recorded"))
}

/// Compact activity shared by run cards and task navigation.
pub fn activity(s: &RunState, live: Liveness, waiting: bool) -> String {
    if waiting {
        "Waiting for your answer".to_owned()
    } else if let Some(w) = &s.resource_wait {
        format!("Waiting for {}", w.kind)
    } else if let Some(b) = &s.resource_block {
        format!("Held · {}", b.description())
    } else if s.parked {
        "Parked · work preserved".to_owned()
    } else if s.status.done() {
        s.status.as_str().replace('_', " ")
    } else if live == Liveness::Dead {
        "Driver exited · work preserved".to_owned()
    } else if live == Liveness::Unknown {
        format!("{} · driver unconfirmed", s.status.as_str())
    } else if !s.active.is_empty() {
        format!(
            "{} · {}",
            s.status.as_str(),
            s.active.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    } else {
        format!("{} · no active seat recorded", s.status.as_str())
    }
}

/// Derive progress without mutating persisted state or interpreting event prose.
pub fn build(s: &RunState, live: Liveness, waiting: bool) -> Progress {
    let stopped =
        s.status.done() || s.parked || s.resource_block.is_some() || live == Liveness::Dead;
    let activity = activity(s, live, waiting);
    let review_only = crate::graph::is_review_run(s);
    let halted = matches!(
        s.status,
        crate::run::RunStatus::Blocked
            | crate::run::RunStatus::Failed
            | crate::run::RunStatus::Stalled
    );
    let current = if halted {
        s.resource_block
            .as_ref()
            .map(|b| phase(&b.node))
            .or_else(|| s.events.last().map(|e| phase(&e.node)))
            .unwrap_or("other")
    } else if s.status == crate::run::RunStatus::Prep
        && s.active.values().any(|a| phase(&a.node) == "advice")
    {
        "advice"
    } else {
        phase(s.status.as_str())
    };
    let mut phases = Vec::new();
    for (key, label, applicable, evidence) in [
        (
            "prep",
            "Prepare",
            true,
            s.prep_failure
                .as_ref()
                .map(|_| "Preparation failed".to_owned())
                .or_else(|| {
                    (s.prep_complete == Some(true)).then(|| "Preparation complete".to_owned())
                }),
        ),
        (
            "advice",
            "Design advice",
            !review_only && s.config.graph.advise,
            s.advice
                .as_ref()
                .map(|a| format!("{} advisor records", a.records.len())),
        ),
        (
            "implement",
            "Parallel implementation",
            !review_only,
            implementation_results(s),
        ),
        (
            "selection",
            "Selection",
            !review_only && s.config.graph.implementers > 1 && !s.judge_skipped,
            s.tally.as_ref().map(|_| "Decision recorded".to_owned()),
        ),
        (
            "review",
            "Review & verification",
            true,
            (!s.reviews.is_empty())
                .then(|| format!("{} recorded rounds · see results below", s.reviews.len())),
        ),
        (
            "gate",
            "Final gate",
            true,
            match s.gate_status() {
                GateStatus::NotRun => None,
                GateStatus::NotConfigured => Some("No commands configured".to_owned()),
                GateStatus::Passed => Some("Passed on recorded head".to_owned()),
                GateStatus::Failed => Some("Gate failed".to_owned()),
            },
        ),
        (
            "land",
            "Publish & land",
            true,
            if s.status == crate::run::RunStatus::Merged {
                Some("Merge confirmed".to_owned())
            } else {
                s.merge
                    .as_ref()
                    .map(|_| "Publication outcome recorded · see Landing".to_owned())
            },
        ),
    ] {
        let active: Vec<_> = s
            .active
            .iter()
            .filter(|(_, a)| phase(&a.node) == key)
            .map(|(k, _)| k.as_str())
            .collect();
        let here = (!s.status.done() || halted) && (!active.is_empty() || current == key);
        let state = if !applicable && !here && evidence.is_none() {
            "skipped"
        } else if here && (waiting || s.resource_wait.is_some() || stopped) {
            "paused"
        } else if here && live == Liveness::Unknown {
            "unconfirmed"
        } else if here {
            "current"
        } else if (key == "prep" && s.prep_failure.is_some())
            || (key == "gate" && s.gate_status() == GateStatus::Failed)
        {
            "failed"
        } else if (key == "prep" && s.prep_complete == Some(true))
            || (key == "selection" && s.tally.is_some())
            || (key == "gate" && s.gate_status() == GateStatus::Passed)
            || (key == "land" && s.status == crate::run::RunStatus::Merged)
        {
            "complete"
        } else if evidence.is_some() {
            "recorded"
        } else if s.status.done()
            || [
                "prep",
                "advice",
                "implement",
                "selection",
                "review",
                "gate",
                "land",
            ]
            .iter()
            .position(|p| *p == key)
            .zip(
                [
                    "prep",
                    "advice",
                    "implement",
                    "selection",
                    "review",
                    "gate",
                    "land",
                ]
                .iter()
                .position(|p| *p == current),
            )
            .is_some_and(|(step, now)| step < now)
        {
            "unrecorded"
        } else {
            "upcoming"
        };
        let detail = if !active.is_empty() {
            active.join(" · ")
        } else {
            evidence.unwrap_or_else(|| {
                match state {
                    "skipped" => "Not applicable to this run",
                    "unknown" | "unrecorded" => "No result recorded",
                    "current" => "Current phase",
                    "unconfirmed" => "Driver unconfirmed",
                    "paused" => "Stopped here",
                    _ => "No result yet",
                }
                .to_owned()
            })
        };
        phases.push(Phase {
            key,
            label,
            state,
            detail,
        });
    }
    if s.active.values().any(|a| phase(&a.node) == "other") {
        phases.push(Phase {
            key: "other",
            label: "Other activity",
            state: if stopped || waiting {
                "paused"
            } else if live == Liveness::Unknown {
                "unconfirmed"
            } else {
                "current"
            },
            detail: s
                .active
                .iter()
                .filter(|(_, a)| phase(&a.node) == "other")
                .map(|(k, a)| format!("{k} · {}", a.node))
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    let rounds = s
        .reviews
        .iter()
        .map(|r| {
            let verification = match r.e2e_status() {
                E2eStatus::Passed => "verification passed on recorded head",
                E2eStatus::Failed => "verification failed on recorded head",
                E2eStatus::Deferred => "verification deferred",
                E2eStatus::NotConfigured => "no verification commands configured",
                E2eStatus::ResourceBlocked => "verification resource blocked",
            };
            let panel = if r.expected > 0 {
                format!("{}/{} reviewers answered", r.answered, r.expected)
            } else {
                format!(
                    "{} reviewer records · panel completeness unknown",
                    r.reviews.len()
                )
            };
            ReviewProgress {
                round: r.round,
                detail: format!(
                    "{panel} · {verification}{}",
                    if r.fix.is_some() {
                        " · fix recorded"
                    } else {
                        ""
                    }
                ),
            }
        })
        .collect();
    Progress {
        activity,
        phases,
        rounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, run::RunStatus};

    fn run() -> RunState {
        RunState::new(
            "repo".into(),
            "main".into(),
            "abc".into(),
            "task".into(),
            Config::default(),
        )
    }

    #[test]
    fn progress_does_not_infer_previous_success_from_phase_order() {
        let mut s = run();
        s.status = RunStatus::Gating;
        let p = build(&s, Liveness::Live, false);
        assert_eq!(
            p.phases.iter().find(|p| p.key == "gate").unwrap().state,
            "current"
        );
        assert!(!p.phases.iter().any(|p| p.state == "recorded"));
        assert_eq!(
            p.phases
                .iter()
                .find(|p| p.key == "implement")
                .unwrap()
                .state,
            "unrecorded"
        );
        assert_eq!(
            p.phases.iter().find(|p| p.key == "land").unwrap().state,
            "upcoming"
        );
        s.status = RunStatus::Merged;
        assert_eq!(
            build(&s, Liveness::Dead, false)
                .phases
                .iter()
                .find(|p| p.key == "implement")
                .unwrap()
                .state,
            "unrecorded"
        );
        assert!(
            !build(&s, Liveness::Dead, false)
                .phases
                .iter()
                .any(|p| matches!(p.state, "current" | "recorded"))
        );
    }

    #[test]
    fn progress_parallel_seats_waits_exit_and_unknown_remain_distinct() {
        let mut s = run();
        s.status = RunStatus::Implementing;
        for key in ["implement-A", "implement-B"] {
            s.seat_started("implement", key, std::time::Duration::from_secs(600), 0);
        }
        let p = build(&s, Liveness::Live, false);
        assert!(p.activity.contains("implement-A, implement-B"));
        assert!(
            p.phases
                .iter()
                .find(|p| p.key == "implement")
                .unwrap()
                .detail
                .contains("implement-B")
        );
        assert_eq!(
            build(&s, Liveness::Live, true).activity,
            "Waiting for your answer"
        );
        assert!(
            build(&s, Liveness::Unknown, false)
                .activity
                .contains("unconfirmed")
        );
        let dead = build(&s, Liveness::Dead, false);
        assert!(dead.activity.contains("Driver exited"));
        assert_eq!(
            dead.phases
                .iter()
                .find(|p| p.key == "implement")
                .unwrap()
                .state,
            "paused"
        );
        s.active.clear();
        s.seat_started(
            "new-node",
            "new-seat",
            std::time::Duration::from_secs(60),
            0,
        );
        assert_eq!(
            build(&s, Liveness::Live, false).phases.last().unwrap().key,
            "other"
        );
        assert_eq!(
            build(&s, Liveness::Live, false)
                .phases
                .iter()
                .find(|p| p.key == "implement")
                .unwrap()
                .state,
            "current"
        );
    }

    #[test]
    fn progress_resource_wait_and_stopped_panel_are_not_active_work() {
        let mut s = run();
        s.status = RunStatus::Reviewing;
        s.resource_wait = Some(crate::run::ResourceWait {
            node: "verify".into(),
            kind: "build cache".into(),
            holder: "another run".into(),
            since: jiff::Timestamp::now(),
            limit_secs: 60,
        });
        let p = build(&s, Liveness::Live, false);
        assert_eq!(p.activity, "Waiting for build cache");
        assert_eq!(
            p.phases.iter().find(|p| p.key == "review").unwrap().state,
            "paused"
        );
        s.resource_wait = None;
        s.status = RunStatus::Stalled;
        s.event("review", "Panel stopped");
        let p = build(&s, Liveness::Dead, false);
        assert_eq!(p.activity, "stalled");
        assert_eq!(
            p.phases.iter().find(|p| p.key == "review").unwrap().state,
            "paused"
        );
        s.status = RunStatus::Reviewing;
        assert_eq!(
            build(&s, Liveness::Unknown, false)
                .phases
                .iter()
                .find(|p| p.key == "review")
                .unwrap()
                .state,
            "unconfirmed"
        );
    }

    #[test]
    fn progress_review_only_skips_competition_and_preserves_gate_failure() {
        let mut s = run();
        s.reviewed_commits = Some(vec![]);
        s.status = RunStatus::Reviewing;
        let p = build(&s, Liveness::Live, false);
        assert_eq!(
            p.phases
                .iter()
                .find(|p| p.key == "implement")
                .unwrap()
                .state,
            "skipped"
        );
        assert_eq!(
            p.phases
                .iter()
                .find(|p| p.key == "selection")
                .unwrap()
                .state,
            "skipped"
        );
        assert!(
            p.phases
                .iter()
                .find(|p| p.key == "review")
                .unwrap()
                .detail
                .contains("Current")
        );
        s.status = RunStatus::Blocked;
        s.gate_ran = true;
        s.gate.push(crate::run::CommandOutcome {
            command: "check".into(),
            code: Some(1),
            output_tail: String::new(),
            duration_ms: 1,
            resource_blocked: false,
        });
        let p = build(&s, Liveness::Dead, false);
        assert_eq!(p.activity, "blocked");
        assert_eq!(
            p.phases.iter().find(|p| p.key == "gate").unwrap().detail,
            "Gate failed"
        );
    }
}
