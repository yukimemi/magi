//! What a pull request description may say about commits and verification.
//!
//! A fixer's report is written inside its own sandbox, at one moment: "could
//! not commit", "HTTP tests could not run". magi may then commit the work
//! itself and verify a later commit successfully, so copying the report into
//! the description verbatim makes it contradict the final state. These
//! functions are pure: they read the structured records (`FixRecord::committed`,
//! `ReviewRound::verified_head`, `RunState::gate` / `gate_head`) and the final
//! commit, and build public facts without copying historical reports. The
//! raw report stays in `RunState` and the run's local artifacts untouched.
//! Everything generated here is English (the GitHub text gate).

use crate::run::{E2eStatus, ReviewRound, RunState};

/// The commit the pull request will carry, as far as the caller knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalHead<'a> {
    /// Not asked (previews, tests): no verification section added.
    Unreported,
    /// Asked, but it could not be read: nothing is judged verified.
    Unknown,
    /// The final commit.
    At(&'a str),
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn all_ok(outcomes: &[crate::run::CommandOutcome]) -> bool {
    !outcomes.is_empty() && outcomes.iter().all(|o| o.ok())
}

/// Public review history contains structured facts only. Operator reports,
/// finding prose and rejection reasons remain untouched in local records.
pub fn review_rounds(state: &RunState) -> String {
    let mut out = String::new();
    for r in &state.reviews {
        let Some(fix) = &r.fix else { continue };
        if out.is_empty() {
            out.push_str("## Review rounds\n\n");
        }
        out.push_str(&format!("### Round {}\n\n", r.round));
        out.push_str(if fix.committed {
            "The fix is committed on the branch (including work committed by magi).\n\n"
        } else {
            "This round produced no commit.\n\n"
        });
        for id in &fix.addressed {
            out.push_str(&format!("- Addressed: `{id}`\n"));
        }
        for rejection in &fix.rejected {
            out.push_str(&format!("- Declined in this round: `{}`\n", rejection.id));
        }
        out.push('\n');
    }
    out
}

/// Evidence for the independent public description. Raw reports are never
/// input: only open findings and explicit decline reasons need paraphrasing.
pub fn summary_evidence(state: &RunState, head: FinalHead<'_>) -> String {
    let open = state.last_round_findings();
    let declined: Vec<_> = state
        .reviews
        .iter()
        .filter_map(|r| r.fix.as_ref())
        .flat_map(|f| &f.rejected)
        .map(|rejection| {
            let finding = state
                .reviews
                .iter()
                .flat_map(|r| &r.reviews)
                .flat_map(|r| &r.findings)
                .find(|f| f.id == rejection.id);
            serde_json::json!({"id": rejection.id, "reason": rejection.why,
                "finding": finding})
        })
        .collect();
    let verification = serde_json::json!({
        "gate": state.gate.iter().map(|o| (&o.command, o.code, o.resource_blocked)).collect::<Vec<_>>(),
        "tests": state.reviews.iter().map(|r| (r.round, &r.verified_head,
            r.e2e.iter().map(|o| (&o.command, o.code, o.resource_blocked)).collect::<Vec<_>>())).collect::<Vec<_>>()
    });
    format!(
        "{}{}\nOpen findings (local source material):\n{}\nDeclined findings (local source material):\n{}\nVerification commands and outcomes:\n{verification}",
        review_rounds(state),
        final_verification(state, head),
        serde_json::to_string(&open).unwrap_or_default(),
        serde_json::to_string(&declined).unwrap_or_default()
    )
}

/// A generation policy change or a new evidence snapshot invalidates old prose.
pub fn summary_fingerprint(state: &RunState, evidence: &str) -> String {
    crate::github_text::fingerprint(
        &format!(
            "public-description-v3:{}",
            state.config.graph.github_language
        ),
        evidence,
    )
}

/// Every finding requiring public context must be covered by the generator.
pub fn required_finding_ids(state: &RunState) -> Vec<String> {
    let mut ids: Vec<_> = state
        .last_round_findings()
        .iter()
        .map(|f| f.id.clone())
        .collect();
    ids.extend(
        state
            .reviews
            .iter()
            .filter_map(|r| r.fix.as_ref())
            .flat_map(|f| f.rejected.iter().map(|r| r.id.clone())),
    );
    ids.sort();
    ids.dedup();
    ids
}

/// Check the prose before appending structural facts: an ID in the appended
/// finding list cannot substitute for the generator's explanation of it.
pub fn covers_required_findings(state: &RunState, prose: &str) -> bool {
    required_finding_ids(state).iter().all(|id| {
        prose
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .any(|word| word == id)
    })
}

fn last_checked(state: &RunState) -> Option<&ReviewRound> {
    state
        .reviews
        .iter()
        .rev()
        .find(|r| r.e2e_status() != E2eStatus::NotConfigured)
}

/// The `## Final verification` section, from the records alone. Empty for
/// [`FinalHead::Unreported`].
pub fn final_verification(state: &RunState, head: FinalHead<'_>) -> String {
    if head == FinalHead::Unreported {
        return String::new();
    }
    let at = match head {
        FinalHead::At(h) => Some(h),
        _ => None,
    };
    let mut lines = vec![match at {
        Some(h) => format!("- Final commit: `{}`", short(h)),
        None => "- Final commit: unknown, so no check below is claimed for it".to_owned(),
    }];
    let ran_on = |recorded: Option<&str>| -> String {
        match (at, recorded) {
            (Some(h), Some(r)) if h == r => "on the final commit".to_owned(),
            (_, Some(r)) => format!(
                "on an earlier commit (`{}`); later changes were not re-verified by it",
                short(r)
            ),
            (_, None) => "on a commit that was not recorded".to_owned(),
        }
    };
    match last_checked(state) {
        None => lines.push("- Tests (`verify.e2e`): not configured or never run".to_owned()),
        Some(r) => lines.push(match r.e2e_status() {
            E2eStatus::Passed => format!(
                "- Tests (`verify.e2e`): passed {} (round {})",
                ran_on(r.verified_head.as_deref()),
                r.round
            ),
            E2eStatus::Failed => format!(
                "- Tests (`verify.e2e`): FAILED {} (round {})",
                ran_on(r.verified_head.as_deref()),
                r.round
            ),
            E2eStatus::Deferred => format!(
                "- Tests (`verify.e2e`): deferred in round {}, not run",
                r.round
            ),
            E2eStatus::ResourceBlocked => format!(
                "- Tests (`verify.e2e`): not run in round {} (blocked on a shared resource)",
                r.round
            ),
            E2eStatus::NotConfigured => unreachable!(),
        }),
    }
    lines.push(if !state.gate_ran {
        "- Gate (`verify.gate`): not run".to_owned()
    } else if state.gate.is_empty() {
        "- Gate (`verify.gate`): no commands configured".to_owned()
    } else if !all_ok(&state.gate) {
        format!(
            "- Gate (`verify.gate`): FAILED {}",
            ran_on(state.gate_head.as_deref())
        )
    } else {
        format!(
            "- Gate (`verify.gate`): passed {}",
            ran_on(state.gate_head.as_deref())
        )
    });
    if !state.gate_fixes.is_empty() {
        lines.push(format!(
            "- Gate-fix rounds spent: {}",
            state.gate_fixes.len()
        ));
    }
    if !state.rebase_fixes.is_empty() {
        lines.push(format!(
            "- Rebase conflict rounds spent: {}",
            state.rebase_fixes.len()
        ));
    }
    format!("## Final verification\n\n{}\n\n", lines.join("\n"))
}
