//! Repository reassignment is a new execution boundary, never a run migration.
use std::path::Path;

use anyhow::{Context, Result, ensure};

use crate::{
    queue::{Queue, Task, TaskStatus},
    run::{Liveness, RunState},
};

/// Reassign an idle task atomically, preserving all historical execution records.
/// All callers use this transaction; chat callers must use the inherited bridge.
pub async fn reassign(
    queue: &Queue,
    home: &Path,
    cwd: &Path,
    id: &str,
    selector: &Path,
    actor: &str,
) -> Result<Task> {
    let listed = queue.get(id)?;
    let _claim = queue
        .claim(&listed.id)
        .context("task is claimed; wait for its driver to stop")?;
    let mut task = queue.get(&listed.id)?;
    let expected = task.clone();
    ensure!(
        matches!(
            task.status,
            TaskStatus::Queued | TaskStatus::Held | TaskStatus::Failed | TaskStatus::Parked
        ),
        "cannot reassign a {} task; stop running work, resolve dependencies, or reopen it first",
        task.status.as_str()
    );
    ensure!(
        task.review_of.is_none()
            && task.pr_recovery.is_none()
            && task.replacement.is_none()
            && task.sources.is_empty()
            && task.overrides.as_ref().is_none_or(|o| o.config.is_none()),
        "task retains repository-bound review, import, replacement or explicit config settings; resolve those selections before reassignment"
    );

    // Hold owner locks and execution leases through the task commit. A direct
    // resume cannot race the boundary even though it does not take a task claim.
    let mut owners = Vec::new();
    let mut leases = Vec::new();
    let mut outputs = Vec::new();
    let ids: std::collections::BTreeSet<_> = task
        .runs
        .iter()
        .chain(task.startup_failures.iter().map(|f| &f.run))
        .cloned()
        .collect();
    for run in ids {
        owners
            .push(crate::pr_head::owner_lock(home, &run).with_context(|| {
                format!("run {run} ownership is busy; wait before reassigning")
            })?);
        let state = RunState::load_under(&run, home).with_context(|| {
            format!("cannot verify retained run {run}; repair its record before reassignment")
        })?;
        let claims = crate::daemon::is_working_on(home, &run, jiff::Timestamp::now());
        ensure!(
            state.liveness_under(home, claims) == Liveness::Dead,
            "run {run} driver is live or unknown; stop it and prove liveness gone before reassignment"
        );
        ensure!(
            state.active.is_empty(),
            "run {run} retains active seats or commands; finish process cleanup before reassignment"
        );
        leases.push(crate::driver::maintenance(home, &run)?);
        for path in crate::retention::worktrees(&state).await? {
            outputs.push(crate::retention::OutputLease::take(&path, true)
                .with_context(|| format!("run {run} seat/tool outputs are still reserved; wait for process cleanup before reassignment"))?);
        }
        if let Some(reason) = reassign_blocker(&state) {
            anyhow::bail!("{reason}");
        }
        if let Some(approval) = &state.land_approval {
            let question = crate::ask::Questions::at(home.join("questions"))
                .get(&approval.question)
                .with_context(|| format!("run {run} approval evidence is unavailable"))?;
            ensure!(
                !question.status.open(),
                "run {run} has pending approval {}; resolve that decision through task control decide first",
                approval.question
            );
        }
        ensure!(
            state
                .repair_grants
                .iter()
                .all(|grant| grant.phase == crate::repair_budget::Phase::Complete),
            "run {run} has an unfinished repair grant; finish or resolve that execution first"
        );
        ensure!(
            !task
                .repair_grants
                .iter()
                .any(|receipt| receipt.request.run == run
                    && !state
                        .repair_grants
                        .iter()
                        .any(|g| g.receipt.request.request == receipt.request.request)),
            "run {run} has a reserved repair receipt; resolve its pinned execution first"
        );
    }
    let repo = crate::repo_selector::resolve_repo(selector, cwd, Some(&task.instruction)).await?;
    let repo = crate::git::toplevel(&repo).await?.canonicalize()?;
    let old = crate::git::toplevel(&task.repo).await?.canonicalize()?;
    ensure!(
        repo != old
            && crate::repo_lock::common_dir(&repo).await?
                != crate::repo_lock::common_dir(&old).await?,
        "task is already assigned to this repository"
    );
    let (config, _) = crate::config::Config::discover_fetched(&repo, None)
        .await
        .context("destination repository config/base is unavailable")?;
    let hits = crate::dupes::check(
        queue,
        &home.join("runs"),
        &repo,
        &task.instruction,
        None,
        Some(&task.id),
    );
    crate::dupes::screen_with_config(hits, &task.instruction, None, &repo, Some(&config))
        .await
        .map_err(|dup| {
            anyhow::anyhow!(
                "{}",
                dup.render("Resolve the duplicate work before reassigning this task.")
            )
        })?;

    task.reassign_repository(repo, actor);
    queue.commit_reassignment(&mut task, &expected)?;
    Ok(task)
}

/// Refuse historical execution even when a caller holds a stale Runner.
pub(crate) fn ensure_current(queue: &Queue, run: &str) -> Result<()> {
    if let Some(task) = crate::direct::resume_owner(queue, run) {
        let state =
            RunState::load_under(run, queue.root().parent().context("queue has no parent")?)?;
        ensure!(
            task.execution_run(run) && task.assignment_accepts(&state),
            "run {run} belongs to a previous repository assignment of task {}; release the task to start fresh in its current repository",
            task.id
        );
    }
    Ok(())
}

/// Repository-bound publication and owner decisions must settle before a move.
fn reassign_blocker(state: &RunState) -> Option<String> {
    let pr = state
        .pr
        .as_ref()
        .map_or("(no recorded PR)", |pr| pr.url.as_str());
    let reason = if state.pr_publication_pending || state.land_sync_pending {
        "has an unresolved publication/synchronization receipt; reconcile it through existing run controls first"
    } else if state
        .pr
        .as_ref()
        .is_some_and(|pr| !matches!(pr.state.as_str(), "closed" | "merged"))
    {
        "retains an open or unknown PR; resolve its disposition through existing landing controls first; reassignment never closes or adopts a PR"
    } else if state
        .merge_decision
        .as_ref()
        .is_some_and(|decision| !decision.closed)
    {
        return Some(format!(
            "run {} has an unresolved owner merge decision for {}; complete that decision through existing landing controls before reassignment",
            state.id,
            state.merge_decision.as_ref().expect("decision").target.pr
        ));
    } else if state.status == crate::run::RunStatus::Landing {
        "is landing a change; finish or resolve landing through existing run controls before reassignment"
    } else {
        return None;
    };
    Some(format!("run {} (PR {pr}) {reason}", state.id))
}

/// Advisory evidence only: old branches and unanswered questions stay in place.
pub fn advisory_notes(task: &Task, home: &Path) -> Vec<String> {
    let mut branches = std::collections::BTreeSet::new();
    for run in task
        .runs
        .iter()
        .chain(task.startup_failures.iter().map(|failure| &failure.run))
    {
        if let Ok(state) = RunState::load_under(run, home) {
            for candidate in state.candidates {
                branches.insert((state.repo.clone(), candidate.branch));
            }
        }
    }
    let questions = crate::ask::Questions::at(home.join("questions"))
        .list()
        .into_iter()
        .filter(|q| {
            q.status.open()
                && (q.run == task.id || task.runs.contains(&q.run))
                && task.stale_question(q)
        })
        .count();
    let mut notes = Vec::new();
    if !branches.is_empty() {
        notes.push(format!("{} retained branch(es) remain in their original repositories; inspect historical runs for branch names before disposing of them", branches.len()));
    }
    if questions > 0 {
        notes.push(format!("{questions} unanswered question(s) are stale and cannot restart earlier repository work"));
    }
    notes
}
