//! Typed stop identity, change detection, and single-use owner approval for
//! conclusive current-head `verify.e2e` failures (`RunState::head_e2e`).
//!
//! `graph::verify_current_head` correctly refuses to treat a recorded red
//! `head_e2e` as green on an unchanged candidate SHA. This module stops
//! automatic queue settlement, dispatch, conductor, triage, standing chat,
//! and notifications from re-running or re-paging the same unchanged failure
//! while still permitting changed facts, fresh repair evidence, and explicit
//! one-shot owner approvals.
use std::fmt::Write as _;
use std::path::Path;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::proc::Quiet as _;
use crate::queue::{HoldSource, Queue, Task, TaskStatus};
use crate::run::{CommandOutcome, MergeRepairPhase, RunState, RunStatus};

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

/// Notice stage recorded in [`crate::queue::NoticeCause::stage`] for a
/// conclusive `head_e2e` hold.
pub const NOTICE_STAGE: &str = "head-e2e-stop";

/// Stable identity of a conclusive current-head `verify.e2e` failure.
///
/// `id` hashes only the durable failure facts (`run`, `head`, `base`,
/// `generation`, and the command/result fingerprint). Pass index, wall-clock
/// timestamps, command duration, and rendered prose are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    /// Stable stop identifier (`head-e2e:<hex>`).
    pub id: String,
    /// Run that recorded this failure.
    pub run: String,
    /// Exact candidate HEAD that failed `verify.e2e`.
    pub head: String,
    /// Base SHA (`<remote>/<base>`) observed when the failure was recorded.
    pub base: String,
    /// Verification history generation at the time this result was recorded.
    pub generation: usize,
    /// SHA-256 digest of the failing command sequence and results (excluding duration).
    pub evidence: String,
    /// Timestamp when the verification result was recorded (not part of `id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<Timestamp>,
    /// Run pass index that actually executed the verification (not part of `id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_pass: Option<usize>,
    /// Number of events in `RunState::events` right after recording this failure (not part of `id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_events: Option<usize>,
    /// Task `resumption.generation` active when the recording pass began (not part of `id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumption_generation: Option<u64>,
}

/// One explicit owner release/resume approval consumed against a `head_e2e` stop.
///
/// Persisted on [`RunState::head_e2e_resumptions`] before executing the pass so
/// restarts and automatic retries can never reuse the same approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resumption {
    /// Owning task id.
    pub task: String,
    /// Stop id that this approval authorized one retry against.
    pub stop_id: String,
    /// Consumed [`crate::queue::Resumption::generation`].
    pub generation: u64,
    /// Timestamp of the consumed approval.
    pub approval_at: Timestamp,
}

/// Result of checking whether candidate/base/repair facts changed since [`Stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FactCheck {
    /// Candidate HEAD, remote base SHA, and repair inputs all match the stop.
    Unchanged,
    /// At least one material fact changed, permitting existing recovery paths.
    Changed(String),
    /// One or more Git references could not be read; never treated as a change.
    Unreadable(String),
}

/// Action required when holding a task for a conclusive `head_e2e` stop.
#[derive(Debug, Clone)]
pub(crate) struct HoldAction {
    pub(crate) stop: Stop,
    pub(crate) reason: String,
    pub(crate) detail: String,
    pub(crate) diagnostic: Option<String>,
}

impl HoldAction {
    /// Apply this hold to `task` without resetting `attempts` or erasing history.
    pub(crate) fn apply(&self, task: &mut Task) {
        let existing_diagnostic = task.diagnostic.clone();
        task.hold_head_e2e(self.stop.clone(), self.reason.clone(), self.detail.clone());
        task.head_e2e_notice(&self.stop);
        task.diagnostic = self.diagnostic.clone().or(existing_diagnostic);
    }
}

/// Pre-attempt gate decision for a task whose latest run has a conclusive `head_e2e` stop.
#[derive(Debug, Clone)]
pub(crate) enum AttemptGate {
    /// Hold the task immediately without starting a pass or charging an attempt.
    Hold(HoldAction),
    /// Permit this run once under an explicit, unconsumed owner approval.
    ConsumeOwnerApproval {
        run: String,
        stop_id: String,
        generation: u64,
        approval_at: Timestamp,
    },
}

/// How held-task triage should classify a `Held` task with respect to `head_e2e`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriageStatus {
    /// Not a `head_e2e` machine hold; ordinary triage rules apply.
    NotHeadE2e,
    /// Conclusive `head_e2e` stop still stands (unchanged or unreadable); keep held quietly.
    HoldStillActive,
    /// Candidate HEAD, remote base, or repair evidence changed; safe to resume without resetting attempts.
    ResolvedChanged,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn outcomes_fingerprint(outcomes: &[CommandOutcome]) -> String {
    let normalized: Vec<_> = outcomes
        .iter()
        .map(|o| (&o.command, o.code, o.resource_blocked, &o.output_tail))
        .collect();
    let bytes = serde_json::to_vec(&normalized).unwrap_or_default();
    sha256_hex(&bytes)
}

/// Extract a conclusive `head_e2e` stop from `state`, if and only if every
/// strict conclusiveness condition holds.
///
/// Returns `None` for inconclusive/resource-blocked verification, empty
/// outcomes, parked runs, base-sync/prep/merge/gate failures, transferred
/// ownership, or active repair grants/merge repairs.
pub fn conclusive(state: &RunState) -> Option<Stop> {
    if state.status != RunStatus::Blocked
        || state.parked
        || state.resource_block.is_some()
        || state.base_sync_blocked()
        || state.prep_failure.is_some()
        || state.merge_decision.is_some()
        || state.land_unrelated.is_some()
        || state.land_approval.is_some()
        || state.pr.is_some()
        || state.pr_publication_pending
        || state.github_text.as_ref().is_some_and(|g| !g.resolved)
        || state.released_to.is_some()
        || state.retired_to.is_some()
        || state.pr_taken_over_by.is_some()
        || state
            .merge_repairs
            .last()
            .is_some_and(|r| r.phase != MergeRepairPhase::Complete)
        || state
            .repair_grants
            .last()
            .is_some_and(|g| g.phase != crate::repair_budget::Phase::Complete)
        || state.winner().is_none()
        || crate::graph::review_conclusion(&state.reviews, state.config.graph.review_rounds)
            != Some(RunStatus::Gating)
        || (state.gate_ran && state.gate.iter().any(|o| !o.ok()))
    {
        return None;
    }

    let e2e = state.head_e2e.as_ref()?;
    if e2e.head.is_empty()
        || e2e.passed
        || e2e.outcomes.is_empty()
        || e2e.outcomes.iter().any(|o| o.resource_blocked)
        || !e2e.outcomes.iter().any(|o| !o.ok())
    {
        return None;
    }

    // Legacy records lack the verification base. Use the run's starting base,
    // not the mutable synchronization tip, to keep their stop identity stable.
    let base = e2e
        .base_commit
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&state.base_commit)
        .to_owned();
    let generation = e2e.generation.unwrap_or(state.verification_history.len());
    let evidence = outcomes_fingerprint(&e2e.outcomes);
    let id_input = serde_json::to_vec(&(&state.id, &e2e.head, &base, generation, &evidence))
        .unwrap_or_default();
    let id = format!("head-e2e:{}", &sha256_hex(&id_input)[..32]);
    let recorded_at = e2e
        .recorded_at
        .or_else(|| {
            state
                .events
                .iter()
                .rev()
                .find(|ev| ev.node == "e2e" && ev.message.contains(&e2e.head))
                .map(|ev| ev.at)
        })
        .or(Some(state.updated_at));

    Some(Stop {
        id,
        run: state.id.clone(),
        head: e2e.head.clone(),
        base,
        generation,
        evidence,
        recorded_at,
        recorded_pass: e2e.pass,
        recorded_events: e2e.recorded_events,
        resumption_generation: e2e.resumption_generation,
    })
}

/// True only when the latest pass on `state` is provably a no-op reconsumption
/// of an earlier pass's conclusive `head_e2e` failure with zero new events.
///
/// Legacy records without `recorded_pass` / `recorded_events` return `false`
/// so historical attempts are never retroactively altered.
pub(crate) fn reconsumed_without_new_verification(state: &RunState, stop: &Stop) -> bool {
    let (Some(current), Some(recorded_pass), Some(recorded_events)) = (
        state.passes.last(),
        stop.recorded_pass,
        stop.recorded_events,
    ) else {
        return false;
    };
    recorded_pass < current.index && state.events.len() == recorded_events
}

fn git_rev_parse_sync(dir: &Path, rev: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .quiet()
        .output()
        .map_err(|e| format!("git rev-parse {rev} in {}: {e}", dir.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("git rev-parse {rev} failed in {}", dir.display())
        } else {
            format!("git rev-parse {rev} failed in {}: {stderr}", dir.display())
        });
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.is_empty() {
        Err(format!("git rev-parse {rev} returned empty output"))
    } else {
        Ok(sha)
    }
}

/// Check whether candidate HEAD, remote base SHA, or repair/task inputs have
/// materially changed since `stop`. Read failures are reported via
/// [`FactCheck::Unreadable`] and are never treated as evidence of change.
pub(crate) fn inspect_facts(task: &Task, state: &RunState, stop: &Stop) -> FactCheck {
    if task.fresh_start {
        return FactCheck::Changed("explicit fresh start requested".into());
    }
    if task.pr_recovery.is_some() {
        return FactCheck::Changed("explicit PR recovery selection present".into());
    }
    if task.replacement.as_ref().is_some_and(|replacement| {
        state.replaces.as_ref().is_none_or(|applied| {
            applied.question != replacement.question
                || applied.run != replacement.run
                || applied.agent != replacement.agent
        })
    }) {
        return FactCheck::Changed("new explicit replacement selection present".into());
    }
    if !crate::daemon::compatible_run_instruction(state, task) {
        return FactCheck::Changed("task instruction changed".into());
    }
    if crate::repair_budget::pending(task, state) {
        return FactCheck::Changed("pending repair grant present".into());
    }

    let Some(winner) = state.winner() else {
        return FactCheck::Unreadable("retained winner record is missing".into());
    };
    let mut diagnostics = Vec::new();

    if winner.worktree.exists() {
        match git_rev_parse_sync(&winner.worktree, "HEAD") {
            Ok(wt_head) if wt_head != stop.head => {
                return FactCheck::Changed(format!(
                    "winner worktree HEAD moved from {} to {}",
                    short(&stop.head),
                    short(&wt_head)
                ));
            }
            Ok(_) => {}
            Err(err) => diagnostics.push(err),
        }
    } else {
        diagnostics.push(format!(
            "winner worktree {} is unavailable",
            winner.worktree.display()
        ));
    }

    let branch_ref = format!("refs/heads/{}", winner.branch);
    match git_rev_parse_sync(&state.repo, &branch_ref) {
        Ok(branch_head) if branch_head != stop.head => {
            return FactCheck::Changed(format!(
                "candidate branch {} moved from {} to {}",
                winner.branch,
                short(&stop.head),
                short(&branch_head)
            ));
        }
        Ok(_) => {}
        Err(err) => diagnostics.push(err),
    }

    let tracking = format!(
        "refs/remotes/{}/{}",
        state.config.merge.remote, state.base_branch
    );
    // A successful sync can incorporate a new base without changing HEAD.
    // Its tip is the dispatch baseline; the failed verification's base
    // remains immutable evidence for the stop identity.
    let synchronized_base = state
        .base_sync
        .as_ref()
        .filter(|sync| sync.conflict.is_none() && !sync.tip.is_empty())
        .map_or(stop.base.as_str(), |sync| sync.tip.as_str());
    match git_rev_parse_sync(&state.repo, &tracking) {
        Ok(base_tip) if !synchronized_base.is_empty() && base_tip != synchronized_base => {
            return FactCheck::Changed(format!(
                "remote base {} moved from {} to {}",
                tracking,
                short(synchronized_base),
                short(&base_tip)
            ));
        }
        Ok(_) => {}
        Err(err) => diagnostics.push(err),
    }

    if !diagnostics.is_empty() {
        FactCheck::Unreadable(diagnostics.join("; "))
    } else {
        FactCheck::Unchanged
    }
}

/// Return the task's unconsumed explicit owner approval if one was recorded
/// after `stop`, or `None` if no new approval exists.
pub(crate) fn pending_owner_approval<'a>(
    task: &'a Task,
    state: &RunState,
    stop: &Stop,
) -> Option<&'a crate::queue::Resumption> {
    let approval = task.resumption.as_ref()?;
    if state
        .head_e2e_resumptions
        .iter()
        .any(|r| r.task == task.id && r.generation >= approval.generation)
    {
        return None;
    }
    match stop.resumption_generation {
        Some(recorded_gen) => (approval.generation > recorded_gen).then_some(approval),
        None => stop
            .recorded_at
            .is_some_and(|at| approval.at > at)
            .then_some(approval),
    }
}

/// Record that an explicit owner approval was consumed for one pass against `stop_id`.
pub(crate) fn record_consumed_approval(
    state: &mut RunState,
    task_id: &str,
    stop_id: &str,
    generation: u64,
    approval_at: Timestamp,
) {
    if state
        .head_e2e_resumptions
        .iter()
        .any(|r| r.task == task_id && r.generation >= generation)
    {
        return;
    }
    state.head_e2e_resumptions.push(Resumption {
        task: task_id.to_owned(),
        stop_id: stop_id.to_owned(),
        generation,
        approval_at,
    });
}

/// Deterministic, actionable hold reason naming the stop evidence and recovery options.
pub(crate) fn hold_reason(
    task: &Task,
    state: &RunState,
    stop: &Stop,
    read_diagnostic: Option<&str>,
) -> String {
    let failed_summary = state
        .head_e2e
        .as_ref()
        .and_then(|e2e| e2e.outcomes.iter().find(|o| !o.ok() && !o.resource_blocked))
        .map(|o| crate::stop_reason::command_failure("verify.e2e", o))
        .unwrap_or_else(|| format!("verify.e2e failed on head {}", short(&stop.head)));
    let base_note = if stop.base.is_empty() {
        String::new()
    } else {
        format!(", base {}", short(&stop.base))
    };
    let read_note = read_diagnostic
        .map(|diag| format!(" Repository check could not confirm a change ({diag})."))
        .unwrap_or_default();
    format!(
        "[head-e2e] {failed_summary} on head {}{base_note} [run {}, stop {}].{read_note} Automatic retries are held while candidate HEAD and base remain unchanged. To recover: update the candidate branch or {}/{}, explicitly release/resume once (`magi task release {}` or `magi run --resume {}`), or edit the task instruction.",
        short(&stop.head),
        stop.run,
        stop.id,
        state.config.merge.remote,
        state.base_branch,
        task.id,
        stop.run,
    )
}

fn load_latest_run(queue: &Queue, task: &Task) -> Option<RunState> {
    let home = queue.root().parent()?;
    let id = task.execution_runs().last()?;
    RunState::load_under(id, home).ok().filter(|s| s.id == *id)
}

fn build_hold(
    task: &Task,
    state: &RunState,
    stop: Stop,
    read_diagnostic: Option<&str>,
) -> HoldAction {
    let reason = hold_reason(task, state, &stop, read_diagnostic);
    let detail = state
        .verification_blocking_cause()
        .unwrap_or_else(|| format!("verify.e2e failed on head {}; nothing published", stop.head));
    let diagnostic = crate::daemon::diagnostic_for(state);
    HoldAction {
        stop,
        reason,
        detail,
        diagnostic,
    }
}

/// Shared eligibility check for [`crate::daemon::claim_dispatch`].
///
/// Returns `Some(HoldAction)` when dispatching `task` would only reconsume an
/// unchanged conclusive `head_e2e` failure without a new explicit owner
/// approval or changed candidate/base/repair facts.
pub(crate) fn dispatch_hold(queue: &Queue, task: &Task) -> Option<HoldAction> {
    match check_attempt(queue, task) {
        Some(AttemptGate::Hold(action)) => Some(action),
        Some(AttemptGate::ConsumeOwnerApproval { .. }) | None => None,
    }
}

/// Shared eligibility check for [`crate::daemon::attempt`].
pub(crate) fn check_attempt(queue: &Queue, task: &Task) -> Option<AttemptGate> {
    let state = load_latest_run(queue, task)?;
    let stop = conclusive(&state)?;
    if task
        .sync_only
        .as_ref()
        .is_some_and(|request| request.run == state.id)
    {
        return None;
    }
    let read_diagnostic = match inspect_facts(task, &state, &stop) {
        FactCheck::Changed(_) => return None,
        FactCheck::Unchanged => None,
        FactCheck::Unreadable(diag) => Some(diag),
    };
    if let Some(approval) = pending_owner_approval(task, &state, &stop) {
        return Some(AttemptGate::ConsumeOwnerApproval {
            run: stop.run,
            stop_id: stop.id,
            generation: approval.generation,
            approval_at: approval.at,
        });
    }
    Some(AttemptGate::Hold(build_hold(
        task,
        &state,
        stop,
        read_diagnostic.as_deref(),
    )))
}

/// True when automatic conductor recovery (`Review` / `Requeue` / re-hold) must
/// be suppressed because the latest run sits on an unchanged conclusive
/// `head_e2e` stop.
pub(crate) fn blocks_automatic_recovery(queue: &Queue, task: &Task) -> bool {
    let Some(state) = load_latest_run(queue, task) else {
        return false;
    };
    let Some(stop) = conclusive(&state) else {
        return false;
    };
    !matches!(inspect_facts(task, &state, &stop), FactCheck::Changed(_))
}

/// Classification for [`crate::triage::run_once`] on a `Held` machine task.
pub(crate) fn triage_status(queue: &Queue, task: &Task) -> TriageStatus {
    if task.status != TaskStatus::Held
        || task.hold_source != Some(HoldSource::Machine)
        || !task.has_head_e2e_cause()
    {
        return TriageStatus::NotHeadE2e;
    }
    let Some(state) = load_latest_run(queue, task) else {
        return TriageStatus::HoldStillActive;
    };
    if let Some(stop) = conclusive(&state) {
        return match inspect_facts(task, &state, &stop) {
            FactCheck::Changed(_) => TriageStatus::ResolvedChanged,
            FactCheck::Unchanged | FactCheck::Unreadable(_) => TriageStatus::HoldStillActive,
        };
    }
    if state.status.resumable()
        && !state.base_sync_blocked()
        && state.prep_failure.is_none()
        && state.resource_block.is_none()
    {
        TriageStatus::ResolvedChanged
    } else {
        TriageStatus::NotHeadE2e
    }
}

/// Refusal message for standing chat recovery (`resume` / `retry` / `release`)
/// when the retained run is stopped on an unchanged conclusive `head_e2e` red.
pub(crate) fn standing_refusal(task: &Task, state: &RunState) -> Option<String> {
    let stop = conclusive(state)?;
    match inspect_facts(task, state, &stop) {
        FactCheck::Changed(_) => None,
        FactCheck::Unchanged => Some(format!(
            "conclusive head_e2e failure ({}) on unchanged head {} / base {}; standing recovery cannot re-dispatch unchanged work. Update the candidate branch or {}/{}, grant a repair, or ask the owner for an explicit decision.",
            stop.id,
            short(&stop.head),
            short(&stop.base),
            state.config.merge.remote,
            state.base_branch,
        )),
        FactCheck::Unreadable(diag) => Some(format!(
            "conclusive head_e2e failure ({}) could not verify any candidate/base change ({diag}); standing recovery cannot re-dispatch unchanged work. Inspect the retained repository state or ask the owner.",
            stop.id,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::Question;
    use crate::config::Config;
    use crate::notices;
    use crate::queue::{Queue, Source, Task};
    use crate::run::{Candidate, CommandOutcome, GateRepairE2e, RunState, RunStatus, Tally};
    use crate::talk::{Talks, seeded_for_test};

    fn git_sync(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn init_repo_with_origin(
        root: &Path,
    ) -> (std::path::PathBuf, std::path::PathBuf, String, String) {
        let origin = root.join("origin.git");
        let repo = root.join("repo");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        git_sync(&origin, &["init", "--bare", "-b", "main"]);
        git_sync(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git_sync(&repo, &["add", "README.md"]);
        git_sync(&repo, &["commit", "-m", "initial"]);
        let base_sha = git_sync(&repo, &["rev-parse", "HEAD"]);
        git_sync(
            &repo,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        git_sync(&repo, &["push", "-u", "origin", "main"]);
        git_sync(&repo, &["checkout", "-b", "feat/candidate"]);
        std::fs::write(repo.join("change.txt"), "candidate\n").unwrap();
        git_sync(&repo, &["add", "change.txt"]);
        git_sync(&repo, &["commit", "-m", "candidate change"]);
        let head_sha = git_sync(&repo, &["rev-parse", "HEAD"]);
        git_sync(&repo, &["checkout", "main"]);
        let wt = root.join("wt");
        git_sync(
            &repo,
            &["worktree", "add", &wt.to_string_lossy(), "feat/candidate"],
        );
        (repo, wt, base_sha, head_sha)
    }

    fn failed_outcome(
        command: &str,
        code: Option<i32>,
        tail: &str,
        resource_blocked: bool,
    ) -> CommandOutcome {
        CommandOutcome {
            command: command.into(),
            code,
            output_tail: tail.into(),
            resource_blocked,
            ..Default::default()
        }
    }

    fn sample_blocked_state(repo: &Path, wt: &Path, base: &str, head: &str) -> RunState {
        let mut cfg = Config::default();
        cfg.graph.review_rounds = 0;
        cfg.verify.e2e = vec!["cargo test --test typography".into()];
        let mut state = RunState::new(
            repo.to_path_buf(),
            "main".into(),
            base.into(),
            "fix typography".into(),
            cfg,
        );
        state.status = RunStatus::Blocked;
        let candidate: Candidate = serde_json::from_value(serde_json::json!({
            "index": 0,
            "label": "A",
            "agent": "existing",
            "branch": "feat/candidate",
            "worktree": wt,
            "commits": 1,
        }))
        .unwrap();
        state.candidates.push(candidate);
        state.tally = Some(
            serde_json::from_value::<Tally>(serde_json::json!({
                "first_choice": {"A": 1},
                "borda": {"A": 1},
                "winner": "A",
                "unanimous_initial": true,
                "deliberated": false,
                "changed_votes": 0,
                "unanimous_final": true,
            }))
            .unwrap(),
        );
        state.begin_pass();
        let mut e2e = GateRepairE2e::new(
            head.into(),
            false,
            vec![failed_outcome(
                "cargo test --test typography",
                Some(1),
                "test typo_spec ... FAILED",
                false,
            )],
        );
        e2e.base_commit = Some(base.into());
        e2e.generation = Some(0);
        e2e.pass = Some(0);
        e2e.recorded_at = Some(Timestamp::now());
        e2e.resumption_generation = Some(0);
        state.event(
            "e2e",
            format!("verify.e2e failed on head {head}; nothing published"),
        );
        e2e.recorded_events = Some(state.events.len());
        state.head_e2e = Some(e2e);
        state
    }

    #[test]
    fn conclusive_excludes_inconclusive_resource_other_stops_and_active_repairs() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let base_state = sample_blocked_state(&repo, &wt, &base, &head);
        assert!(conclusive(&base_state).is_some());

        // Inconclusive resource-blocked outcome.
        let mut s = base_state.clone();
        s.head_e2e.as_mut().unwrap().outcomes[0].resource_blocked = true;
        assert!(conclusive(&s).is_none());

        // Empty outcomes placeholder (interrupted before finishing).
        let mut s = base_state.clone();
        s.head_e2e.as_mut().unwrap().outcomes.clear();
        assert!(conclusive(&s).is_none());

        // Passed head_e2e.
        let mut s = base_state.clone();
        s.head_e2e.as_mut().unwrap().passed = true;
        assert!(conclusive(&s).is_none());

        // Parked run.
        let mut s = base_state.clone();
        s.parked = true;
        assert!(conclusive(&s).is_none());

        // Resource block present.
        let mut s = base_state.clone();
        s.resource_block = Some(crate::run::ResourceBlock {
            cause: crate::run::ResourceBlockCause::Other,
            node: "e2e".into(),
            kind: "shared build cache".into(),
            detail: "busy".into(),
            at: Timestamp::now(),
        });
        assert!(conclusive(&s).is_none());

        // Base-sync stop present.
        let mut s = base_state.clone();
        s.base_sync = Some(crate::run::BaseSync {
            tip: base.clone(),
            behind: 1,
            attempts: 1,
            conflict: Some("conflict".into()),
            already_in: None,
        });
        assert!(conclusive(&s).is_none());

        // Prep failure present.
        let mut s = base_state.clone();
        s.prep_failure = Some("setup failed".into());
        assert!(conclusive(&s).is_none());

        // Gate failure takes precedence.
        let mut s = base_state.clone();
        s.gate_ran = true;
        s.gate = vec![failed_outcome(
            "cargo make check",
            Some(1),
            "lint failed",
            false,
        )];
        assert!(conclusive(&s).is_none());

        // Active repair grant in progress.
        let mut s = base_state.clone();
        s.repair_grants.push(crate::repair_budget::Applied {
            receipt: crate::repair_budget::Receipt {
                request: crate::repair_budget::Request {
                    task: "t".into(),
                    run: s.id.clone(),
                    kind: crate::repair_budget::Kind::Gate,
                    rounds: 1,
                    head: head.clone(),
                    expect_pass: 0,
                    request: "req-1".into(),
                    reason: "repair".into(),
                    tip: None,
                    instruction: None,
                    replace_request: None,
                    evidence_digest: None,
                },
                at: Timestamp::now(),
                decision: None,
                contract: String::new(),
                stop: None,
                legacy: None,
            },
            spent: 0,
            phase: crate::repair_budget::Phase::Pending,
            review_start: 0,
            head: head.clone(),
            succeeded: false,
            conflict: None,
        });
        assert!(conclusive(&s).is_none());
    }

    #[test]
    fn stop_identity_ignores_pass_time_duration_and_prose_and_tracks_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let s1 = sample_blocked_state(&repo, &wt, &base, &head);
        let stop1 = conclusive(&s1).unwrap();

        // Varying pass index, timestamps, duration_ms, and event prose must not change stop.id.
        let mut s2 = s1.clone();
        s2.begin_pass();
        s2.head_e2e.as_mut().unwrap().pass = Some(5);
        s2.head_e2e.as_mut().unwrap().recorded_at = Some(
            Timestamp::now()
                .checked_add(jiff::SignedDuration::from_hours(2))
                .unwrap(),
        );
        s2.head_e2e.as_mut().unwrap().outcomes[0].duration_ms = 999_999;
        s2.event("e2e", "rephrased prose");
        let stop2 = conclusive(&s2).unwrap();
        assert_eq!(stop1.id, stop2.id);

        // Changing head, base, generation, command, exit code, or output MUST change stop.id.
        let mut diff_head = s1.clone();
        diff_head.head_e2e.as_mut().unwrap().head =
            "1111111111111111111111111111111111111111".into();
        assert_ne!(conclusive(&diff_head).unwrap().id, stop1.id);

        let mut diff_base = s1.clone();
        diff_base.head_e2e.as_mut().unwrap().base_commit =
            Some("2222222222222222222222222222222222222222".into());
        assert_ne!(conclusive(&diff_base).unwrap().id, stop1.id);

        let mut diff_gen = s1.clone();
        diff_gen.head_e2e.as_mut().unwrap().generation = Some(1);
        assert_ne!(conclusive(&diff_gen).unwrap().id, stop1.id);

        let mut diff_cmd = s1.clone();
        diff_cmd.head_e2e.as_mut().unwrap().outcomes[0].command = "cargo test --test other".into();
        assert_ne!(conclusive(&diff_cmd).unwrap().id, stop1.id);

        let mut diff_out = s1.clone();
        diff_out.head_e2e.as_mut().unwrap().outcomes[0].output_tail = "different failure".into();
        assert_ne!(conclusive(&diff_out).unwrap().id, stop1.id);
    }

    #[test]
    fn legacy_stop_identity_survives_base_sync_without_new_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let mut state = sample_blocked_state(&repo, &wt, &base, &head);
        let task = Task::new(
            "t".into(),
            "fix typography".into(),
            repo.clone(),
            Source::Human,
        );
        state.head_e2e.as_mut().unwrap().base_commit = None;
        let before = conclusive(&state).unwrap();
        git_sync(&repo, &["update-ref", "refs/remotes/origin/main", &head]);
        assert!(matches!(
            inspect_facts(&task, &state, &before),
            FactCheck::Changed(_)
        ));
        state.base_sync = Some(crate::run::BaseSync {
            tip: head.clone(),
            behind: 0,
            attempts: 1,
            conflict: None,
            already_in: None,
        });
        let after = conclusive(&state).unwrap();
        assert_eq!(before.id, after.id);
        assert_eq!(after.base, base);
        assert_eq!(inspect_facts(&task, &state, &after), FactCheck::Unchanged);

        state.head_e2e.as_mut().unwrap().base_commit = Some(String::new());
        assert_eq!(conclusive(&state).unwrap().id, before.id);

        let mut different_base = state.clone();
        different_base.base_commit = head.clone();
        assert_ne!(conclusive(&different_base).unwrap().id, before.id);

        let mut different_head = state.clone();
        different_head.head_e2e.as_mut().unwrap().head = base.clone();
        assert_ne!(conclusive(&different_head).unwrap().id, before.id);

        let mut different_failure = state.clone();
        different_failure.head_e2e.as_mut().unwrap().outcomes[0].output_tail =
            "another failure".into();
        assert_ne!(conclusive(&different_failure).unwrap().id, before.id);

        state.head_e2e.as_mut().unwrap().base_commit = Some(head);
        let recorded = conclusive(&state).unwrap();
        assert_ne!(recorded.id, before.id);
        state.base_commit = "another initial base".into();
        state.base_sync.as_mut().unwrap().tip = base;
        assert_eq!(conclusive(&state).unwrap().id, recorded.id);
    }

    #[test]
    fn reconsumption_detection_is_exact_and_conservative_on_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let mut state = sample_blocked_state(&repo, &wt, &base, &head);
        let stop = conclusive(&state).unwrap();
        assert!(!reconsumed_without_new_verification(&state, &stop));

        // Legacy record (no recorded_pass): never retroactively refunded.
        let mut legacy = state.clone();
        legacy.begin_pass();
        legacy.head_e2e.as_mut().unwrap().pass = None;
        let legacy_stop = conclusive(&legacy).unwrap();
        assert!(!reconsumed_without_new_verification(&legacy, &legacy_stop));

        // New pass with zero new events: provably reconsumed.
        state.begin_pass();
        let stop = conclusive(&state).unwrap();
        assert!(reconsumed_without_new_verification(&state, &stop));

        // New pass with a new event: not a pure reconsumption.
        state.event("e2e", "fresh verification event");
        let stop = conclusive(&state).unwrap();
        assert!(!reconsumed_without_new_verification(&state, &stop));
    }

    #[test]
    fn inspect_facts_detects_head_base_and_repair_changes_and_refuses_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let state = sample_blocked_state(&repo, &wt, &base, &head);
        let stop = conclusive(&state).unwrap();
        let task = Task::new(
            "t".into(),
            "fix typography".into(),
            repo.clone(),
            Source::Human,
        );

        assert_eq!(inspect_facts(&task, &state, &stop), FactCheck::Unchanged);

        // Moving remote base SHA (even without changing candidate HEAD) is detected.
        std::fs::write(repo.join("base2.txt"), "moved\n").unwrap();
        git_sync(&repo, &["add", "base2.txt"]);
        git_sync(&repo, &["commit", "-m", "advance base"]);
        git_sync(&repo, &["push", "origin", "main"]);
        assert!(matches!(
            inspect_facts(&task, &state, &stop),
            FactCheck::Changed(msg) if msg.contains("remote base")
        ));

        // Reset remote base back to original SHA so we can test candidate HEAD move.
        git_sync(&repo, &["reset", "--hard", &base]);
        git_sync(&repo, &["push", "--force", "origin", "main"]);
        assert_eq!(inspect_facts(&task, &state, &stop), FactCheck::Unchanged);

        // Moving candidate HEAD in worktree is detected.
        std::fs::write(wt.join("change2.txt"), "fix\n").unwrap();
        git_sync(&wt, &["add", "change2.txt"]);
        git_sync(&wt, &["commit", "-m", "fix candidate"]);
        assert!(matches!(
            inspect_facts(&task, &state, &stop),
            FactCheck::Changed(msg) if msg.contains("HEAD moved")
        ));

        // Reset candidate back, then break the tracking ref so git read fails -> Unreadable, NOT Changed!
        git_sync(&wt, &["reset", "--hard", &head]);
        assert_eq!(inspect_facts(&task, &state, &stop), FactCheck::Unchanged);
        let mut missing_base = state.clone();
        missing_base.base_branch = "nonexistent-base".into();
        assert!(matches!(
            inspect_facts(&task, &missing_base, &stop),
            FactCheck::Unreadable(_)
        ));
    }

    #[test]
    fn dispatch_and_triage_respect_explicit_sync_and_hold_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let mut state = sample_blocked_state(&repo, &wt, &base, &head);
        let queue = Queue::at(tmp.path().join("queue"));
        let mut task = Task::new(
            "t".into(),
            "fix typography".into(),
            repo.clone(),
            Source::Human,
        );
        task.start(state.id.clone());
        let replacement = crate::merge_decision::Replacement {
            question: "replacement".into(),
            run: "previous".into(),
            pr: "previous-pr".into(),
            agent: "existing".into(),
            context: String::new(),
        };
        task.replacement = Some(replacement.clone());
        state.replaces = Some(replacement);
        state.save_under(tmp.path()).unwrap();
        let stop = conclusive(&state).unwrap();
        let hold =
            dispatch_hold(&queue, &task).expect("historical replacement is not new evidence");
        hold.apply(&mut task);
        assert_eq!(triage_status(&queue, &task), TriageStatus::HoldStillActive);
        task.resume_head_e2e_wait();
        task.sync_only = Some(crate::base_sync_only::Request {
            run: state.id.clone(),
            at: Timestamp::now(),
        });
        assert!(dispatch_hold(&queue, &task).is_none());
        task.sync_only = None;
        assert!(dispatch_hold(&queue, &task).is_some());

        git_sync(&repo, &["update-ref", "refs/remotes/origin/main", &head]);
        task.hold_machine(Some("disk capacity unavailable".into()));
        task.mark_disk_capacity();
        assert_eq!(triage_status(&queue, &task), TriageStatus::NotHeadE2e);
        build_hold(&task, &state, stop, None).apply(&mut task);
        assert_eq!(triage_status(&queue, &task), TriageStatus::ResolvedChanged);
    }

    #[test]
    fn conductor_preserves_independent_question_on_unchanged_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let mut state = sample_blocked_state(&repo, &wt, &base, &head);
        state.save_under(tmp.path()).unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let questions = crate::ask::Questions::at(tmp.path().join("questions"));
        let mut task = Task::new("t".into(), "fix typography".into(), repo, Source::Human);
        task.start(state.id.clone());
        dispatch_hold(&queue, &task).unwrap().apply(&mut task);
        queue.put(&mut task).unwrap();
        crate::conduct::apply(
            &queue,
            &questions,
            &crate::conduct::Verdict {
                decisions: vec![crate::conduct::Decision {
                    id: task.id.clone(),
                    blocked_by: Vec::new(),
                    missing_dependencies: Vec::new(),
                    reason: Some("Independent scope decision".into()),
                    recovery: Some(crate::conduct::Recovery::Hold),
                    question: Some("Should the scope include another component?".into()),
                    choices: vec!["hold".into()],
                    actions: Default::default(),
                    question_decision: None,
                    evidence: Vec::new(),
                }],
            },
        )
        .unwrap();
        let blocked = queue.get(&task.id).unwrap();
        assert_eq!(blocked.status, TaskStatus::Blocked);
        let question = questions.get(&blocked.blocked_by[0]).unwrap();
        assert!(question.status.open());
        assert_eq!(
            question.summary,
            "Should the scope include another component?"
        );
    }

    #[test]
    fn notice_semantics_unifies_run_ended_task_held_and_outcome_for_same_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let mut state = sample_blocked_state(&repo, &wt, &base, &head);
        state.save_under(tmp.path()).unwrap();
        let stop = conclusive(&state).unwrap();

        let talks = Talks::at(tmp.path().join("talks"));
        let mut chat = seeded_for_test("chat-1", repo.clone());
        talks.put(&mut chat).unwrap();

        let queue = Queue::at(tmp.path().join("queue"));
        let mut task = Task::new(
            "typography".into(),
            "fix typography".into(),
            repo.clone(),
            Source::Agent {
                run: chat.id.clone(),
                node: crate::queue::CHAT_NODE.into(),
            },
        );
        task.start(state.id.clone());
        // Even while task is still Running (race before settlement), run_ended normalizes identically.
        let run_notice = notices::run_ended(&state).unwrap();
        assert_eq!(run_notice.cause.as_deref(), Some(stop.id.as_str()));
        let sem_run_while_running = crate::notice_semantics::attention(
            tmp.path(),
            Some(&task),
            Some(&state.id),
            &run_notice,
        )
        .unwrap();

        let hold = build_hold(&task, &state, stop.clone(), None);
        hold.apply(&mut task);
        queue.put(&mut task).unwrap();
        let held_notice = notices::task_held(&task).unwrap();
        assert_eq!(held_notice.cause.as_deref(), Some(stop.id.as_str()));

        let sem_held = crate::notice_semantics::attention(
            tmp.path(),
            Some(&task),
            Some(&state.id),
            &held_notice,
        )
        .unwrap();
        let sem_run_after_hold = crate::notice_semantics::attention(
            tmp.path(),
            Some(&task),
            Some(&state.id),
            &run_notice,
        )
        .unwrap();
        let sem_outcome = crate::notice_semantics::outcome(tmp.path(), &task);

        assert_eq!(sem_run_while_running, sem_held);
        assert_eq!(sem_held, sem_run_after_hold);
        assert_eq!(sem_held, sem_outcome);

        // A separate admission failure on the same retained run must not
        // inherit the earlier E2E delivery receipt.
        let mut disk_held = task.clone();
        disk_held.release_operator("cli", None);
        disk_held.hold_machine(Some("disk capacity unavailable".into()));
        disk_held.mark_disk_capacity();
        let disk_outcome = crate::notice_semantics::outcome(tmp.path(), &disk_held);
        assert!(!crate::notice_semantics::covers(
            &sem_outcome,
            &disk_outcome
        ));
        let disk_notice = notices::task_held(&disk_held).unwrap();
        let disk_attention = crate::notice_semantics::attention(
            tmp.path(),
            Some(&disk_held),
            Some(&state.id),
            &disk_notice,
        )
        .unwrap();
        assert!(!crate::notice_semantics::covers(
            &sem_outcome,
            &disk_attention
        ));

        // New pass reconsuming the same stop (and later held_at) still produces the same semantic receipt.
        state.begin_pass();
        state.save_under(tmp.path()).unwrap();
        let sem_next_pass = crate::notice_semantics::outcome(tmp.path(), &task);
        assert!(crate::notice_semantics::covers(
            &sem_outcome,
            &sem_next_pass
        ));

        // Distinct new failure (new head) produces a distinct receipt that is NOT covered.
        let mut new_head_state = state.clone();
        new_head_state.head_e2e.as_mut().unwrap().head =
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        new_head_state.save_under(tmp.path()).unwrap();
        let new_stop = conclusive(&new_head_state).unwrap();
        assert_ne!(new_stop.id, stop.id);
        let new_hold = build_hold(&task, &new_head_state, new_stop, None);
        new_hold.apply(&mut task);
        let sem_new_head = crate::notice_semantics::outcome(tmp.path(), &task);
        assert!(!crate::notice_semantics::covers(
            &sem_outcome,
            &sem_new_head
        ));

        // Restore original stop and verify an actionable new Question is NEVER flattened.
        state.save_under(tmp.path()).unwrap();
        hold.apply(&mut task);
        let q = Question::new(
            task.id.clone(),
            "conduct".into(),
            "conductor".into(),
            "Need direction".into(),
            "Choose next step".into(),
            vec!["resume".into(), "hold".into()],
        );
        let sem_question = crate::notice_semantics::question(tmp.path(), &task, &state.id, &q);
        assert!(!crate::notice_semantics::covers(
            &sem_outcome,
            &sem_question
        ));
    }

    #[test]
    fn standing_refusal_blocks_unchanged_and_permits_changed_head_or_base() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, wt, base, head) = init_repo_with_origin(tmp.path());
        let state = sample_blocked_state(&repo, &wt, &base, &head);
        let task = Task::new(
            "t".into(),
            "fix typography".into(),
            repo.clone(),
            Source::Human,
        );

        let refused = standing_refusal(&task, &state).expect("unchanged must be refused");
        assert!(refused.contains("head-e2e:"));
        assert!(refused.contains("standing recovery"));

        // Moving candidate HEAD permits standing recovery again.
        std::fs::write(wt.join("fix.txt"), "fix\n").unwrap();
        git_sync(&wt, &["add", "fix.txt"]);
        git_sync(&wt, &["commit", "-m", "fix"]);
        assert!(standing_refusal(&task, &state).is_none());
    }
}
