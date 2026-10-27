//! One-line explanations for runs that stopped without completing.
use crate::config::MergeMode;
use crate::run::{CommandOutcome, GateStatus, RunState, RunStatus};

/// Derive a header explanation, using only recorded facts. A newer attempt
/// must have a different id and a non-terminal status to qualify as in flight.
pub(crate) fn of(state: &RunState, latest: Option<(&str, RunStatus)>) -> Option<String> {
    let fallback = match state.status {
        RunStatus::Blocked => "magi stopped short of merging.",
        RunStatus::Stalled => {
            "The judging panel never reached a quorum, so no verdict was recorded. The work is kept."
        }
        RunStatus::Failed => "The graph could not complete.",
        _ => return None,
    };
    let blocking = state
        .open_findings()
        .into_iter()
        .filter(|f| f.severity.blocks())
        .count();
    let reason = if state.gate_status() == GateStatus::Failed {
        state
            .gate
            .iter()
            .find(|c| !c.ok() && !c.resource_blocked)
            .map(|c| command_failure("Gate", c))
    } else {
        None
    }
    .or_else(|| {
        state
            .resource_block
            .as_ref()
            .map(|block| block.description())
    })
    .or_else(|| {
        state
            .base_sync
            .as_ref()?
            .conflict
            .as_ref()
            .map(|detail| format!("Base-sync conflict: {detail}"))
    })
    .or_else(|| {
        let merge = state.merge.as_ref()?;
        (!merge.ok && !merge.empty).then(|| {
            format!(
                "{} failed: {}",
                if merge.mode == MergeMode::Pr {
                    "Merge / PR-open"
                } else {
                    "Merge"
                },
                merge.detail
            )
        })
    })
    .or_else(|| {
        (blocking > 0).then(|| format!("{blocking} open Major-or-above review finding(s)."))
    })
    .or_else(|| {
        if state.quota.is_empty() {
            return None;
        }
        let seats = state.quota.len();
        match state.status {
            // A stall is a lost quorum; the lost seats explain it even when
            // viable candidates remain.
            RunStatus::Stalled => Some(format!(
                "Quota loss: {seats} seat(s); the judging panel lost its quorum."
            )),
            RunStatus::Failed if state.viable().is_empty() => Some(format!(
                "Quota loss: {seats} seat(s); no viable candidates remain."
            )),
            _ => None,
        }
    })
    .or_else(|| {
        state
            .pre_gate
            .iter()
            .find(|c| !c.ok() && !c.resource_blocked)
            .map(|c| command_failure("Pre-gate", c))
    })
    .unwrap_or_else(|| fallback.to_owned());
    let reason = if latest.is_some_and(|(id, status)| id != state.id && !status.done()) {
        format!("Earlier attempt: {reason}")
    } else {
        reason
    };
    Some(one_line(&reason))
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_failure(kind: &str, command: &CommandOutcome) -> String {
    let mut name = None;
    let mut failed = None;
    let mut failures = false;
    for line in command.output_tail.lines() {
        let line = line.trim();
        if let Some(test) = line
            .strip_prefix("test ")
            .and_then(|s| s.strip_suffix(" ... FAILED"))
        {
            name.get_or_insert(test.to_owned());
        } else if let Some(test) = line
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            name.get_or_insert(test.to_owned());
        } else if line == "failures:" {
            failures = true;
        } else if failures && !line.is_empty() {
            if line
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':'))
            {
                name.get_or_insert(line.to_owned());
            }
            failures = false;
        }
        if line.starts_with("test result: FAILED.") {
            for part in line.split(';') {
                if let Some(n) = part
                    .trim()
                    .strip_suffix(" failed")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    failed = Some(n);
                }
            }
        }
    }
    let mut reason = format!("{kind} failed: {}", command.command);
    if let Some(name) = name {
        reason.push_str(&format!(" — {name}"));
    }
    if let Some(n) = failed {
        reason.push_str(&format!(" ({n} failed)"));
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{BaseSync, MergeOutcome, QuotaLoss};
    use serde_json::json;

    fn state() -> RunState {
        let mut s = RunState::new(
            "/repo".into(),
            "main".into(),
            "abc".into(),
            "task".into(),
            crate::config::Config::default(),
        );
        s.status = RunStatus::Blocked;
        s
    }

    fn command(output: &str) -> CommandOutcome {
        CommandOutcome {
            command: "cargo make check".into(),
            code: Some(1),
            output_tail: output.into(),
            duration_ms: 0,
            resource_blocked: false,
        }
    }

    #[test]
    fn gate_names_the_command_test_and_failure_count() {
        let mut s = state();
        s.gate_ran = true;
        s.gate = vec![command(
            "failures:\n    web::tests::talks_report_independent_thinking_claims_and_queue_a_second_message\n\ntest result: FAILED. 1813 passed; 1 failed; 0 ignored;",
        )];
        assert_eq!(
            of(&s, None).unwrap(),
            "Gate failed: cargo make check — web::tests::talks_report_independent_thinking_claims_and_queue_a_second_message (1 failed)"
        );
        for output in [
            "test sample::test ... FAILED",
            "---- sample::test stdout ----",
        ] {
            s.gate[0].output_tail = output.into();
            assert_eq!(
                of(&s, None).unwrap(),
                "Gate failed: cargo make check — sample::test"
            );
        }
        s.gate[0].output_tail = "unparseable output".into();
        assert_eq!(of(&s, None).unwrap(), "Gate failed: cargo make check");
    }

    #[test]
    fn base_conflict_and_merge_failure() {
        let mut s = state();
        s.base_sync = Some(BaseSync {
            tip: "abc".into(),
            behind: 1,
            attempts: 1,
            conflict: Some("conflict\n in\tfile".into()),
            already_in: None,
        });
        assert_eq!(
            of(&s, None).unwrap(),
            "Base-sync conflict: conflict in file"
        );
        s.merge = Some(MergeOutcome {
            mode: MergeMode::Pr,
            ok: false,
            detail: "gh refused".into(),
            empty: false,
        });
        assert!(of(&s, None).unwrap().starts_with("Base-sync conflict"));
        s.base_sync = None;
        assert_eq!(of(&s, None).unwrap(), "Merge / PR-open failed: gh refused");
        s.merge.as_mut().unwrap().mode = MergeMode::Local;
        assert_eq!(of(&s, None).unwrap(), "Merge failed: gh refused");
        s.merge.as_mut().unwrap().empty = true;
        assert_eq!(of(&s, None).unwrap(), "magi stopped short of merging.");
    }

    #[test]
    fn counts_only_open_blocking_findings_and_gate_takes_precedence() {
        let mut s = state();
        s.reviews = serde_json::from_value(json!([{
            "round": 1, "head": "abc", "clean": false, "reviews": [{
                "reviewer": 1, "agent": "", "findings": [
                    {"severity": "major", "title": "a"},
                    {"severity": "blocker", "title": "b"},
                    {"severity": "minor", "title": "c"}
                ]
            }]
        }]))
        .unwrap();
        assert_eq!(
            of(&s, None).unwrap(),
            "2 open Major-or-above review finding(s)."
        );
        s.gate_ran = true;
        s.gate = vec![command("")];
        assert_eq!(of(&s, None).unwrap(), "Gate failed: cargo make check");
        s.gate_ran = false;
        s.reviews[0].clean = true;
        assert_eq!(of(&s, None).unwrap(), "magi stopped short of merging.");
    }

    #[test]
    fn quota_is_not_inferred_from_old_history_for_blocked_runs() {
        let mut s = state();
        s.quota.push(QuotaLoss {
            cause: crate::agent::QuotaCause::UsageLimit,
            agent: String::new(),
            seat: "judge-1".into(),
            node: "judge".into(),
            at: s.created_at,
            reset: None,
        });
        assert_eq!(of(&s, None).unwrap(), "magi stopped short of merging.");
        for status in [RunStatus::Stalled, RunStatus::Failed] {
            s.status = status;
            assert!(of(&s, None).unwrap().starts_with("Quota loss: 1 seat(s)"));
        }
        s.candidates = serde_json::from_value(json!([{
            "index": 0, "label": "A", "agent": "", "branch": "candidate",
            "worktree": "/repo"
        }]))
        .unwrap();
        assert_eq!(of(&s, None).unwrap(), "The graph could not complete.");
        // Viable candidates but a collapsed panel: still quota.
        s.status = RunStatus::Stalled;
        assert_eq!(
            of(&s, None).unwrap(),
            "Quota loss: 1 seat(s); the judging panel lost its quorum."
        );
    }

    #[test]
    fn pre_gate_fallback_and_non_stopped_statuses() {
        let mut s = state();
        s.pre_gate = vec![command("")];
        assert_eq!(of(&s, None).unwrap(), "Pre-gate failed: cargo make check");
        s.pre_gate[0].resource_blocked = true;
        assert_eq!(of(&s, None).unwrap(), "magi stopped short of merging.");
        s.pre_gate.clear();
        for (status, expected) in [
            (RunStatus::Blocked, "magi stopped short of merging."),
            (RunStatus::Failed, "The graph could not complete."),
            (
                RunStatus::Stalled,
                "The judging panel never reached a quorum, so no verdict was recorded. The work is kept.",
            ),
        ] {
            s.status = status;
            assert_eq!(of(&s, None).unwrap(), expected);
        }
        s.status = RunStatus::Reviewing;
        assert!(of(&s, None).is_none());
    }

    #[test]
    fn earlier_attempt_requires_a_different_non_terminal_run() {
        let s = state();
        assert!(
            of(&s, Some(("new", RunStatus::Reviewing)))
                .unwrap()
                .starts_with("Earlier attempt: ")
        );
        for latest in [
            None,
            Some((s.id.as_str(), RunStatus::Reviewing)),
            Some(("new", RunStatus::Blocked)),
            Some(("new", RunStatus::Ready)),
        ] {
            assert!(!of(&s, latest).unwrap().starts_with("Earlier attempt: "));
        }
    }
}
