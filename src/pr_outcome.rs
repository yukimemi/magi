//! What a pull request description may say about commits and verification.
//!
//! A fixer's report is written inside its own sandbox, at one moment: "could
//! not commit", "HTTP tests could not run". magi may then commit the work
//! itself and verify a later commit successfully, so copying the report into
//! the description verbatim makes it contradict the final state. These
//! functions are pure: they read the structured records (`FixRecord::committed`,
//! `ReviewRound::verified_head`, `RunState::gate` / `gate_head`) and the final
//! commit, and decide which sentences of a historical report still hold. The
//! raw report stays in `RunState` and the run's local artifacts untouched.
//! Everything generated here is English (the GitHub text gate).

use crate::run::{E2eStatus, ReviewRound, RunState};

/// The commit the pull request will carry, as far as the caller knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalHead<'a> {
    /// Not asked (previews, tests): notes are kept verbatim, no section added.
    Unreported,
    /// Asked, but it could not be read: nothing is judged verified.
    Unknown,
    /// The final commit.
    At(&'a str),
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// A sentence that says the fixer could not commit.
fn is_commit_limit(s: &str) -> bool {
    let l = s.to_lowercase();
    [
        "uncommitted",
        "not committed",
        "could not commit",
        "couldn't commit",
        "cannot commit",
        "can't commit",
        "unable to commit",
    ]
    .iter()
    .any(|p| l.contains(p))
}

/// A sentence that says tests or checks could not run.
fn is_test_limit(s: &str) -> bool {
    let l = s.to_lowercase();
    let cannot = [
        "could not run",
        "couldn't run",
        "cannot run",
        "can't run",
        "unable to run",
        "not run",
        "did not run",
        "sandbox",
    ]
    .iter()
    .any(|p| l.contains(p));
    cannot
        && ["test", "http", "network", "sandbox", "check", "build"]
            .iter()
            .any(|w| l.contains(w))
}

/// Split a line into sentences, keeping each one's trailing separator.
fn sentences(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        if matches!(bytes[i], b'.' | b';') && bytes.get(i + 1).is_some_and(|b| *b == b' ') {
            out.push(&line[start..=i + 1]);
            start = i + 2;
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

fn all_ok(outcomes: &[crate::run::CommandOutcome]) -> bool {
    !outcomes.is_empty() && outcomes.iter().all(|o| o.ok())
}

/// Did the gate pass on exactly this commit?
fn gate_passed_at(state: &RunState, head: &str) -> bool {
    state.gate_ran && all_ok(&state.gate) && state.gate_head.as_deref() == Some(head)
}

/// Did a round after `round` pass `verify.e2e` on exactly this commit, or the
/// gate?
fn verified_after(state: &RunState, round: usize, head: &str) -> bool {
    gate_passed_at(state, head)
        || state.reviews.iter().any(|r| {
            r.round > round
                && r.e2e_status() == E2eStatus::Passed
                && r.verified_head.as_deref() == Some(head)
        })
}

/// The `## Review fixes` section (empty when no round has notes) and how many
/// sentences of the fixers' reports the final state contradicts.
pub fn review_fixes(state: &RunState, head: FinalHead<'_>) -> (String, usize) {
    let mut kept: Vec<(usize, String)> = Vec::new();
    let mut dropped = 0;
    for r in &state.reviews {
        let Some(fix) = r.fix.as_ref() else { continue };
        let notes = fix.notes.trim();
        if notes.is_empty() {
            continue;
        }
        let commit_obsolete = fix.committed && head != FinalHead::Unreported;
        let test_obsolete = match head {
            FinalHead::At(h) => verified_after(state, r.round, h),
            _ => false,
        };
        let mut lines = Vec::new();
        for line in notes.lines() {
            let mut kept_line = String::new();
            for s in sentences(line) {
                let obsolete =
                    (commit_obsolete && is_commit_limit(s)) || (test_obsolete && is_test_limit(s));
                if obsolete {
                    dropped += 1;
                } else {
                    kept_line.push_str(s);
                }
            }
            if !kept_line.trim().is_empty() {
                lines.push(kept_line.trim_end().to_owned());
            }
        }
        let mut text = lines.join("\n");
        if head != FinalHead::Unreported {
            let state_line = if fix.committed {
                "_The fix is committed on the branch (magi commits any work the fixer left behind)._"
            } else {
                "_This round produced no commit._"
            };
            text = if text.is_empty() {
                state_line.to_owned()
            } else {
                format!("{text}\n\n{state_line}")
            };
        }
        kept.push((r.round, text));
    }
    if kept.is_empty() {
        return (String::new(), dropped);
    }
    let mut out = String::from("## Review fixes\n\n");
    if let [(_, only)] = kept.as_slice() {
        out.push_str(only);
        out.push_str("\n\n");
    } else {
        for (round, text) in &kept {
            out.push_str(&format!("### Round {round}\n\n{text}\n\n"));
        }
    }
    if dropped > 0 {
        out.push_str(&format!(
            "_{dropped} statement(s) from a fixer's sandboxed session about commit or test limits were left out because the final state below supersedes them; the full reports stay in the run's local records._\n\n"
        ));
    }
    (out, dropped)
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
            E2eStatus::Failed => format!("- Tests (`verify.e2e`): FAILED in round {}", r.round),
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
        "- Gate (`verify.gate`): FAILED".to_owned()
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
