//! Applicability of external-merge Attention, never authority to change a run.
use std::path::Path;

use crate::queue::Queue;
use serde_json::Value;

/// A reason to retain a check locally instead of interrupting the owner.
/// Unknown ownership, lifecycle or filesystem errors remain actionable.
pub(crate) fn historical_reason(home: &Path, id: &str, run: &Value) -> Option<&'static str> {
    if matches!(
        run["status"].as_str(),
        Some("superseded" | "already_in_base" | "merged")
    ) || run.pointer("/pr/state").and_then(Value::as_str) == Some("closed")
    {
        return Some("stored lifecycle ended this external-merge check");
    }
    let queue_root = home.join("queue");
    // Queue::list intentionally skips unreadable records. Absence of an owner
    // is evidence here only when the directory itself can also be inspected.
    match std::fs::read_dir(&queue_root) {
        Ok(mut entries) => {
            if entries.any(|entry| entry.is_err()) {
                return None;
            }
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return None,
        _ => {}
    }
    let queue = Queue::at(queue_root);
    let tasks = queue.list();
    let owners: Vec<_> = tasks
        .iter()
        .filter(|t| t.runs.iter().any(|r| r == id))
        .collect();
    if queue.unreadable().is_empty()
        && owners.len() == 1
        && owners[0].runs.last().is_some_and(|last| last != id)
    {
        return Some("the owning task records a later attempt");
    }
    if run["status"].as_str() != Some("blocked")
        || !run.get("merge").is_none_or(Value::is_null)
        || !run.get("pr").is_none_or(Value::is_null)
    {
        return None;
    }
    let candidates = run["candidates"].as_array()?;
    if candidates.is_empty()
        || !candidates
            .iter()
            .all(|c| c["folded"].as_bool() == Some(true))
    {
        return None;
    }
    if !queue.unreadable().is_empty()
        || !owners.is_empty()
        || crate::daemon::is_working_on(home, id, jiff::Timestamp::now())
    {
        return None;
    }
    let repo = Path::new(run["repo"].as_str()?);
    if !repo.is_absolute() {
        return None;
    }
    match std::fs::metadata(repo) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some("unowned folded history has no checkout; external merge remains unknown")
        }
        _ => None,
    }
}

pub(crate) fn recorded_merge(run: &Value) -> bool {
    run.pointer("/pr/state").and_then(Value::as_str) == Some("merged")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Source, Task};

    #[test]
    fn external_merge_applicability_requires_combined_evidence() {
        let home = tempfile::tempdir().unwrap();
        let mut run = serde_json::json!({
            "status": "blocked", "repo": home.path().join("removed"),
            "candidates": [{"folded": true}], "pr": null, "merge": null
        });
        assert!(historical_reason(home.path(), "legacy", &run).is_some());
        run["candidates"][0]["folded"] = false.into();
        assert!(historical_reason(home.path(), "legacy", &run).is_none());
        run["candidates"][0]["folded"] = true.into();
        run["repo"] = serde_json::to_value(home.path()).unwrap();
        assert!(historical_reason(home.path(), "legacy", &run).is_none());
        run["repo"] = serde_json::to_value(home.path().join("removed")).unwrap();
        run["pr"] = serde_json::json!({"state": "unknown"});
        assert!(historical_reason(home.path(), "legacy", &run).is_none());
        run["pr"] = serde_json::json!({"state": "merged"});
        assert!(recorded_merge(&run));
        assert!(
            historical_reason(home.path(), "legacy", &run).is_none(),
            "positive merge evidence must still reach correction"
        );
        run["pr"] = serde_json::json!({"state": "closed"});
        assert!(historical_reason(home.path(), "legacy", &run).is_some());
        run["pr"] = Value::Null;
        for status in ["merged", "superseded", "already_in_base"] {
            run["status"] = status.into();
            assert!(historical_reason(home.path(), "legacy", &run).is_some());
        }
        run["status"] = "blocked".into();
        let queue = Queue::at(home.path().join("queue"));
        let mut task = Task::new(
            "current".into(),
            "current".into(),
            home.path().into(),
            Source::Human,
        );
        task.runs.push("legacy".into());
        queue.put(&mut task).unwrap();
        assert!(historical_reason(home.path(), "legacy", &run).is_none());
        task.runs.push("successor".into());
        queue.put(&mut task).unwrap();
        assert!(historical_reason(home.path(), "legacy", &run).is_some());
        task.runs.push("legacy".into());
        queue.put(&mut task).unwrap();
        assert!(
            historical_reason(home.path(), "legacy", &run).is_none(),
            "resumed work is current"
        );
    }
}
