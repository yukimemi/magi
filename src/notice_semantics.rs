//! Semantic receipts shared by automatic attention, decisions and outcomes.
//! Unknown producers retain their literal evidence rather than guessing equivalence.
use std::path::Path;

use serde_json::{Value, json};

use crate::{ask::Question, notices::Notice, queue::Task};

fn decision(q: &Question) -> Value {
    // Structured producer context identifies the subject independently of prose.
    let subject = if let Some(context) = &q.decision_context {
        json!(context)
    } else {
        // Effects and targets alone cannot prove the underlying evidence is
        // unchanged. Unstructured questions may carry new facts in their prose.
        json!([q.summary, q.detail])
    };
    json!([subject, q.choices, q.actions, q.merge_target])
}

fn read_run_record(
    home: &Path,
    run: Option<&str>,
) -> Option<(Value, Option<crate::run::RunState>)> {
    let run =
        run.filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))?;
    let bytes = std::fs::read(home.join("runs").join(run).join("run.json")).ok()?;
    let record = serde_json::from_slice::<Value>(&bytes).ok()?;
    let state = serde_json::from_value::<crate::run::RunState>(record.clone())
        .ok()
        .and_then(|state| crate::run::migrate_schema(state).ok());
    Some((record, state))
}

fn active_head_e2e_stop_id(home: &Path, task: Option<&Task>, run: Option<&str>) -> Option<String> {
    if let Some(t) = task
        && !matches!(
            t.status,
            crate::queue::TaskStatus::Running
                | crate::queue::TaskStatus::Failed
                | crate::queue::TaskStatus::Held
        )
    {
        return None;
    }
    if let Some((_, Some(state))) = read_run_record(home, run) {
        return crate::head_e2e_stop::conclusive(&state).map(|stop| stop.id);
    }
    task.and_then(|t| t.head_e2e_stop_id().map(str::to_owned))
}

fn run_evidence(home: &Path, run: Option<&str>) -> Value {
    let Some((record, state)) = read_run_record(home, run) else {
        return Value::Null;
    };
    let mut evidence = serde_json::Map::new();
    // Use the same structured cause as run_ended/run_stopped. The last event
    // and the task's formatted error need not contain this diagnostic.
    let verification_cause = state
        .as_ref()
        .and_then(|state| state.verification_blocking_cause());
    let head_e2e_stop = state
        .as_ref()
        .and_then(crate::head_e2e_stop::conclusive)
        .map(|stop| stop.id);
    evidence.insert("verification_cause".into(), json!(verification_cause));
    if let Some(stop_id) = &head_e2e_stop {
        evidence.insert("head_e2e_stop".into(), json!(stop_id));
    }
    for field in [
        "status",
        "prep_failure",
        "resource_block",
        "candidate_preservation",
        "candidates",
        "pr",
        "pr_head",
        "pr_pushed_head",
        "pr_merged_head",
        "release_bump",
        "tally",
        "reviews",
        "head_e2e",
        "gate_repair_e2e",
        "gate",
        "gate_head",
        "head_e2e",
    ] {
        if let Some(value) = record.get(field) {
            evidence.insert(field.into(), stable(value));
        }
    }
    if head_e2e_stop.is_none() {
        // A new execution of the same run is a new opportunity, even if it fails alike.
        evidence.insert(
            "pass".into(),
            json!(
                record["passes"]
                    .as_array()
                    .and_then(|p| p.last())
                    .map(|p| &p["index"])
            ),
        );
        evidence.insert(
            "cause".into(),
            record["events"]
                .as_array()
                .and_then(|e| e.last())
                .map(|e| e["message"].clone())
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(evidence)
}

fn stable(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "updated_at"
                            | "created_at"
                            | "checked_at"
                            | "verified_at"
                            | "recorded_at"
                            | "at"
                            | "duration_ms"
                            | "pass"
                            | "recorded_events"
                            | "resumption_generation"
                    )
                })
                .map(|(key, value)| (key.clone(), stable(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(stable).collect()),
        other => other.clone(),
    }
}

fn snapshot(
    home: &Path,
    task: Option<&Task>,
    run: Option<&str>,
    extra: Value,
    q: Option<&Question>,
) -> String {
    let questions = crate::ask::Questions::at(home.join("questions"));
    let mut blockers: Vec<_> = task
        .into_iter()
        .flat_map(|t| &t.blocked_by)
        .map(|id| {
            questions.get(id).map_or_else(
                |_| json!(id),
                |q| json!([decision(&q), q.status, q.resolution()]),
            )
        })
        .collect();
    blockers.sort_by_key(Value::to_string);
    blockers.dedup();
    let mut resolutions: Vec<_> = questions
        .list()
        .into_iter()
        .filter(|q| q.answer.is_some())
        .filter(|q| {
            task.is_some_and(|t| {
                q.run == t.id
                    || t.runs.contains(&q.run)
                    || q.routed_task.as_deref() == Some(t.id.as_str())
            }) || run.is_some_and(|r| q.run == r)
        })
        .map(|q| json!([decision(&q), q.answer, q.answered_at]))
        .collect();
    resolutions.sort_by_key(Value::to_string);
    let head_e2e_stop = active_head_e2e_stop_id(home, task, run);
    let task_value = if let Some(stop_id) = &head_e2e_stop
        && extra.is_null()
    {
        task.map(|t| {
            json!({
                "id": t.id,
                "state": "head_e2e_stop",
                "run": run,
                "instruction": t.instruction,
                "blockers": blockers,
                "stop_id": stop_id,
            })
        })
    } else {
        task.map(|t| {
            json!({
                "id": t.id, "state": t.status, "run": run, "instruction": t.instruction,
                "blockers": blockers, "review_branch": t.review_branch,
                "cause": t.hold_notice_cause,
                "error": t.last_error, "hold": t.hold_reason,
                "diagnostic": t.diagnostic, "block": t.block_reason,
                "answers": t.answers, "applied": t.actions_applied,
                "triage": t.triage_applied, "operator_decision": t.operator_decided_at,
                "attempts": t.attempts,
                "completion_event": t.completion_history.last(),
            })
        })
    };
    // Delivery timestamps and question IDs are deliberately absent. Owner
    // resolutions delimit separate decision opportunities without changing holds.
    json!({ "decision": q.map(decision), "base": {
        "version": 1,
        "task": task_value,
        "run": run, "evidence": run_evidence(home, run),
        "extra": extra, "resolutions": resolutions,
    }})
    .to_string()
}

pub(crate) fn outcome(home: &Path, task: &Task) -> String {
    snapshot(
        home,
        Some(task),
        task.runs.last().map(String::as_str),
        Value::Null,
        None,
    )
}

pub(crate) fn question(home: &Path, task: &Task, run: &str, q: &Question) -> String {
    snapshot(home, Some(task), Some(run), Value::Null, Some(q))
}

pub(crate) fn attention(
    home: &Path,
    task: Option<&Task>,
    run: Option<&str>,
    n: &Notice,
) -> Option<String> {
    let held = task
        .and_then(crate::notices::task_held)
        .is_some_and(|held| held.key == n.key && held.message == n.message);
    let failed = run.is_some_and(|id| {
        if n.key != format!("run:{id}") {
            return false;
        }
        let cause = n
            .message
            .strip_prefix(&format!("Run {id} ended "))
            .and_then(|s| s.split_once(". ").map(|(_, cause)| cause))
            .or_else(|| {
                n.message
                    .strip_prefix(&format!("Run {id} stopped with an error. "))
            })
            .or_else(|| {
                n.message
                    .strip_prefix(&format!("Run {id} stopped with an error: "))
            });
        let evidence = run_evidence(home, run);
        cause.is_some_and(|cause| {
            [
                evidence["prep_failure"].as_str(),
                evidence["verification_cause"].as_str(),
                evidence
                    .pointer("/resource_block/detail")
                    .and_then(Value::as_str),
                evidence["candidate_preservation"]
                    .as_object()
                    .and_then(|m| m.values().next())
                    .and_then(Value::as_str),
                evidence["cause"].as_str(),
                task.and_then(|t| t.last_error.as_deref()),
            ]
            .into_iter()
            .flatten()
            .any(|known| known.lines().next() == Some(cause))
        })
    });
    let head_e2e_stop = active_head_e2e_stop_id(home, task, run);
    let head_e2e_match = head_e2e_stop
        .as_deref()
        .is_some_and(|stop_id| n.cause.as_deref() == Some(stop_id));
    // Normalize only evidence we can confirm. Immediate graph errors, changed
    // explicit causes and unrelated notices retain their literal evidence.
    let extra = if (n.cause.is_none() || head_e2e_match) && (held || failed) {
        Value::Null
    } else {
        json!([n.key, n.cause, n.since, n.severity, n.message, n.link])
    };
    Some(snapshot(home, task, run, extra, None))
}

// A decision delivery also covers its underlying failure report. The reverse
// does not hold: an attention report never presents the required choice.
pub(crate) fn covers(stored: &str, incoming: &str) -> bool {
    if stored == incoming {
        return true;
    }
    let (Ok(old), Ok(new)) = (
        serde_json::from_str::<Value>(stored),
        serde_json::from_str::<Value>(incoming),
    ) else {
        return false;
    };
    old.get("base").is_some() && old["base"] == new["base"] && new["decision"].is_null()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(home: &Path) -> Task {
        let mut task = Task::new(
            "task".into(),
            "instruction".into(),
            home.into(),
            crate::queue::Source::Human,
        );
        task.runs.push("run".into());
        task.last_error = Some("no candidate produced a change".into());
        task.hold_machine(task.last_error.clone());
        std::fs::create_dir_all(home.join("runs/run")).unwrap();
        write_run(home, "head-a", 0, "no candidate produced a change");
        task
    }

    fn write_run(home: &Path, head: &str, pass: usize, cause: &str) {
        std::fs::write(
            home.join("runs/run/run.json"),
            json!({
                "status": "failed", "pr_head": head,
                "passes": [{"index": pass}], "events": [{"message": cause, "at": "old"}]
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn semantic_failure_normalizes_held_and_failed_in_both_orders() {
        let home = tempfile::tempdir().unwrap();
        let task = fixture(home.path());
        let held = crate::notices::task_held(&task).unwrap();
        let failed = Notice::error(
            "run:run",
            "Run run ended failed. no candidate produced a change",
        );
        let a = attention(home.path(), Some(&task), Some("run"), &held).unwrap();
        let b = attention(home.path(), Some(&task), Some("run"), &failed).unwrap();
        assert!(covers(&a, &b) && covers(&b, &a));
        assert_eq!(a, outcome(home.path(), &task));
        let mut regenerated = failed.clone();
        regenerated.generation += 10;
        regenerated.delivery_id = Some("new-delivery".into());
        regenerated.last_at = jiff::Timestamp::now();
        assert_eq!(
            b,
            attention(home.path(), Some(&task), Some("run"), &regenerated).unwrap()
        );
        regenerated.message = "Run run stopped with an error: new independent failure".into();
        assert!(!covers(
            &b,
            &attention(home.path(), Some(&task), Some("run"), &regenerated).unwrap()
        ));
        regenerated.cause = Some("new cause".into());
        assert!(!covers(
            &b,
            &attention(home.path(), Some(&task), Some("run"), &regenerated).unwrap()
        ));
    }

    #[test]
    fn semantic_new_evidence_pass_state_target_and_decision_are_delivered() {
        let home = tempfile::tempdir().unwrap();
        let mut task = fixture(home.path());
        let original = outcome(home.path(), &task);
        for (head, pass, cause) in [
            ("head-b", 0, "no candidate produced a change"),
            ("head-a", 1, "no candidate produced a change"),
            ("head-a", 0, "new failure"),
        ] {
            write_run(home.path(), head, pass, cause);
            assert!(!covers(&original, &outcome(home.path(), &task)));
        }
        write_run(home.path(), "head-a", 0, "no candidate produced a change");
        let mut q = Question::new(
            task.id.clone(),
            "conduct".into(),
            "conductor".into(),
            "Resume?".into(),
            "explanation".into(),
            vec!["resume".into(), "hold".into()],
        );
        q.decision_context = Some("same target and evidence".into());
        let required = question(home.path(), &task, "run", &q);
        assert!(!covers(&original, &required));
        assert!(covers(&required, &original));
        q.id = "regenerated-question".into();
        q.summary = "Rephrased".into();
        q.detail = "Another explanation".into();
        assert!(covers(&required, &question(home.path(), &task, "run", &q)));
        q.choices.push("discard".into());
        assert!(!covers(&required, &question(home.path(), &task, "run", &q)));
        q.choices.pop();
        q.merge_target = Some(crate::merge_decision::Target {
            pr: "pr".into(),
            head: "new-head".into(),
            language: "en".into(),
        });
        assert!(!covers(&required, &question(home.path(), &task, "run", &q)));
        q.merge_target = None;
        let before_answer = question(home.path(), &task, "run", &q);
        let questions = crate::ask::Questions::at(home.path().join("questions"));
        let mut answered = q.clone();
        answered
            .answer(crate::ask::Answer::Choice("hold".into()))
            .unwrap();
        questions.put(&mut answered).unwrap();
        assert!(!covers(
            &before_answer,
            &question(home.path(), &task, "run", &q)
        ));
        task.operator_decided_at = Some(jiff::Timestamp::now());
        assert!(!covers(&required, &question(home.path(), &task, "run", &q)));
        task.status = crate::queue::TaskStatus::Done;
        assert!(!covers(&original, &outcome(home.path(), &task)));
    }

    #[test]
    fn completion_occurrences_never_share_a_semantic_receipt() {
        let home = tempfile::tempdir().unwrap();
        let mut task = fixture(home.path());
        let context = crate::queue::CompletionContext::new(
            crate::queue::CompletionActor::Web,
            Some("owner cancelled".into()),
            home.path(),
            &task,
        );
        task.succeed(context);
        let first = outcome(home.path(), &task);
        task.completion_history
            .last_mut()
            .unwrap()
            .occurrence
            .push_str("-next");
        let next = outcome(home.path(), &task);
        assert!(!covers(&first, &next));
        assert!(!covers(&next, &first));
    }

    #[test]
    fn unchanged_e2e_rerun_stop_keeps_semantic_receipt_and_distinguishes_new_verdict() {
        let home = tempfile::tempdir().unwrap();
        let task = fixture(home.path());
        let write_e2e = |verdict_kind: &str, dur: u64| {
            std::fs::write(
                home.path().join("runs/run/run.json"),
                json!({
                    "status": "blocked",
                    "head_e2e": {
                        "head": "0b11aaaa",
                        "passed": false,
                        "outcomes": [{
                            "command": "cargo make test",
                            "code": 100,
                            "duration_ms": dur,
                            "resource_blocked": false,
                            "artifact": "artifacts/e2e-4.out",
                            "rerun_comparison": {
                                "head": "0b11aaaa",
                                "verdict": { "kind": verdict_kind, "reason": "unknown" },
                                "first": { "attempt": 1, "code": 1, "duration_ms": dur, "artifact": "artifacts/e2e-3.out", "excerpt": "a", "extraction": { "kind": "unknown", "reason": "x" }, "intermediate": {} },
                                "second": { "attempt": 2, "code": 100, "duration_ms": dur, "artifact": "artifacts/e2e-4.out", "excerpt": "b", "extraction": { "kind": "unknown", "reason": "y" }, "intermediate": {} }
                            }
                        }]
                    },
                    "passes": [{"index": 0}],
                    "events": [{"message": "verify.e2e failed", "at": "t1"}]
                })
                .to_string(),
            )
            .unwrap();
        };
        write_e2e("unknown", 1000);
        let first = outcome(home.path(), &task);
        // Same saved stop facts (even if only duration_ms differs): duplicate suppressed.
        write_e2e("unknown", 2500);
        let unchanged = outcome(home.path(), &task);
        assert!(covers(&first, &unchanged) && covers(&unchanged, &first));
        // Distinct verdict changes the semantic receipt.
        write_e2e("distinct", 2500);
        let changed = outcome(home.path(), &task);
        assert!(!covers(&first, &changed));
    }
}
