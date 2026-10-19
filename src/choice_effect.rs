//! One description of each executable question choice, shared by cards and briefs.
use crate::{ask::Question, land, merge_decision, release_watch, triage};

/// None means this label promises no operation implemented by its issuer.
/// Positional protocols are confined to nodes whose consumers use positions.
pub fn describe(q: &Question, label: &str) -> Option<String> {
    if !q.choices.iter().any(|c| c == label) {
        return None;
    }
    if let Some(action) = q.actions.get(label) {
        return Some(action.describe());
    }
    if q.cwd.is_some() && q.node != crate::conduct::NODE && crate::deputy::kind_of(q).is_none() {
        return Some("deliver this answer to the waiting agent".into());
    }
    let at = q.choices.iter().position(|c| c == label)?;
    let effect = match q.node.as_str() {
        triage::NODE => match at {
            0 => "release and requeue the task",
            1 => "keep the task held",
            2 => "delete the task",
            _ => return None,
        },
        triage::DEPS_NODE => match at {
            0 => "release the dependency task",
            1 => "detach dependants and delete the dependency task",
            2 => "detach dependants",
            _ => return None,
        },
        land::APPROVAL_NODE => match label {
            land::APPROVE => "merge the approved pull request head",
            land::HOLD => "keep the approval held",
            land::REPAIR => "fix findings and re-review the run",
            merge_decision::WITHDRAW => "close the pull request and end the task",
            merge_decision::DISMISS => "end the obsolete approval",
            c if merge_decision::selected_agent(c).is_some() => {
                "start the selected replacement implementer"
            }
            _ => return None,
        },
        crate::bump::NOTICE_NODE if q.seat == "release-watch" => match label {
            release_watch::RERUN_AGAIN => "rerun failed checks once more",
            release_watch::RETRY => "retry the release job",
            release_watch::LEAVE_IT => "dismiss this release watch",
            land::APPROVE => "merge the approved release head",
            land::HOLD => "keep the release held",
            _ => return None,
        },
        crate::github_text::ASK_NODE => match label {
            crate::github_text::POST_ORIGINAL => "post the original text",
            crate::github_text::USE_REWRITE => "post the displayed rewrite",
            crate::github_text::USE_MY_TEXT => "use the owner's supplied text",
            crate::github_text::USE_FALLBACK => "use the fallback text",
            _ => return None,
        },
        crate::reconcile::NODE if q.seat == crate::reconcile::SEAT => match at {
            0 => "push the local branch",
            1 => "keep the remote branch",
            _ => return None,
        },
        _ => return None,
    };
    Some(effect.into())
}

/// Generated conductor prose cannot delegate manual maintenance to the owner.
/// Keep only explicitly executable choices; a safe hold is always available.
pub(crate) fn executable_conductor(q: &mut Question) {
    let offered_choices = !q.choices.is_empty();
    q.choices.retain(|c| q.actions.contains_key(c));
    q.actions.retain(|c, _| q.choices.contains(c));
    if offered_choices && q.choices.is_empty() {
        let hold = "daemon: 保留を続ける".to_owned();
        q.actions
            .insert(hold.clone(), crate::ask::ChoiceAction::Hold);
        q.choices.push(hold);
    }
}

/// Regenerated each turn so old deputy briefs cannot conceal new effects.
pub(crate) fn brief(q: &Question) -> String {
    let mut text =
        String::from("Exact choice effects (daemon/component applies; you only settle):");
    for label in &q.choices {
        text.push_str(&format!(
            "\n- `{label}`: {}",
            describe(q, label).unwrap_or_else(|| "unsupported legacy choice; do not settle".into())
        ));
        if let Some(action) = q.actions.get(label) {
            text.push_str(&format!(
                "\n  Exact persisted action: {}",
                serde_json::to_string(action).expect("choice actions serialize")
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_only_choices_are_replaced_and_new_unmapped_choices_are_rejected() {
        let mut q = Question::new(
            "task".into(),
            crate::conduct::NODE.into(),
            "conduct".into(),
            "maintenance".into(),
            String::new(),
            vec!["operator: edit predecessor record then resume".into()],
        );
        let dir = tempfile::tempdir().unwrap();
        let store = crate::ask::Questions::at(dir.path().join("questions"));
        assert!(store.put_executable(&mut q).is_err());
        executable_conductor(&mut q);
        assert_eq!(q.choices.len(), 1);
        assert!(matches!(
            q.actions.values().next(),
            Some(crate::ask::ChoiceAction::Hold)
        ));
        store.put_executable(&mut q).unwrap();
        q.choices.clear();
        q.actions.clear();
        executable_conductor(&mut q);
        assert!(q.choices.is_empty(), "free-text questions remain free text");
    }

    #[test]
    fn structured_action_parser_preserves_equals_in_a_hold_reason() {
        let (_, action) = crate::ask::ChoiceAction::parse(
            r#"release={"do":"releasetask","task":"t","held_at":"2026-01-01T00:00:00Z","reason":"a=b"}"#, "").unwrap();
        assert!(
            matches!(action, crate::ask::ChoiceAction::ReleaseTask { reason, .. } if reason.as_deref() == Some("a=b"))
        );
    }
}
