//! Owner-requested Git-only passes over a retained winner.
use anyhow::{Context as _, Result, bail, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{
    git,
    queue::{CompletionActor, CompletionContext, OperatorResume, Task, TaskStatus},
    run::{BaseSync, RunState, RunStatus},
};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub run: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Pass {
    pub request: Request,
    pub(crate) pass_index: usize,
    pub required_base: String,
    pub tip: Option<String>,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub result: Outcome,
    pub verification_allowed: bool,
    #[serde(default)]
    reservation: Option<usize>,
    // Only this exact commit may have been installed by an interrupted pass.
    prepared_head: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Incomplete,
    AlreadyCurrent,
    Synchronized,
    AlreadyInBase,
    Stopped(String),
}

impl Outcome {
    pub(crate) fn succeeded(&self) -> bool {
        matches!(
            self,
            Self::AlreadyCurrent | Self::Synchronized | Self::AlreadyInBase
        )
    }
}

impl Pass {
    pub(crate) fn new(request: Request, state: &RunState) -> Self {
        Self {
            request,
            pass_index: state.passes.len(),
            required_base: format!("{}/{}", state.config.merge.remote, state.base_branch),
            tip: None,
            head_before: None,
            head_after: None,
            result: Outcome::Incomplete,
            reservation: None,
            verification_allowed: false,
            prepared_head: None,
        }
    }

    pub fn summary(&self) -> String {
        match &self.result {
            Outcome::Incomplete => {
                "Base synchronization only: incomplete; verification was not allowed. Resume with --sync-base-only to finish".into()
            }
            Outcome::AlreadyCurrent => {
                "HEAD already contains the base; verification was not allowed. Explicitly resume to continue"
                    .into()
            }
            Outcome::Synchronized => {
                "Base synchronized; verification was not allowed. Explicitly resume to continue"
                    .into()
            }
            Outcome::AlreadyInBase => {
                "Winner already integrated into the base; verification was not allowed".into()
            }
            Outcome::Stopped(why) => {
                format!("Base synchronization only: {why}; verification was not allowed")
            }
        }
    }

    pub(crate) fn report_line(&self) -> String {
        format!(
            "{} — {} @ {}; HEAD {} → {}; {}",
            self.request.at,
            self.required_base,
            self.tip.as_deref().unwrap_or("not fetched"),
            self.head_before.as_deref().unwrap_or("unknown"),
            self.head_after.as_deref().unwrap_or("unknown"),
            self.summary(),
        )
    }
}

pub(crate) fn validate_run(state: &RunState, home: &Path) -> Result<()> {
    ensure!(
        matches!(
            state.status,
            RunStatus::Reviewing
                | RunStatus::Gating
                | RunStatus::Blocked
                | RunStatus::Failed
                | RunStatus::Voting
        ) && state.tally.as_ref().is_some_and(|t| t.met_quorum)
            && state
                .winner()
                .is_some_and(|winner| winner.viable() && !winner.folded)
            && state.prep_failure.is_none(),
        "sync-base-only requires a stopped run with a decided, retained winner"
    );
    ensure!(
        !state.base_sync_blocked(),
        "sync-base-only cannot replace an existing base synchronization stop. {}",
        state.base_sync_next_action()
    );
    ensure!(
        state.pr.is_none()
            && state.pr_pushed_head.is_none()
            && state.pr_recovery.is_none()
            && !state.pr_publication_pending
            && state.merge.is_none()
            && state.land_approval.is_none()
            && state.merge_decision.is_none()
            && state.merge_repairs.is_empty()
            && state.land_rebase_from.is_none()
            && !state.base_recovery_pending()
            && state.github_text.is_none()
            && state
                .repair_grants
                .last()
                .is_none_or(|grant| grant.phase == crate::repair_budget::Phase::Complete),
        "sync-base-only cannot change a publication, landing or repair checkpoint"
    );
    ensure!(
        state
            .base_sync_recovery
            .operations
            .last()
            .is_none_or(|op| op.finished)
            && !home
                .join("runs")
                .join(&state.id)
                .join("base-sync")
                .try_exists()?,
        "an unfinished ordinary base sync requires its existing recovery path"
    );
    Ok(())
}

/// Queue under the owning task's claim, including explicit recovery of a dead worker.
pub fn queue(task: &mut Task, home: &Path, run: &str) -> Result<()> {
    let state = crate::resume::validate(home, run)?;
    crate::pr_head::ensure_owner_record(&state, None)?;
    validate_run(&state, home)?;
    let orphan = task
        .sync_only
        .as_ref()
        .filter(|request| task.status == TaskStatus::Running && request.run == run);
    let _ownership = if orphan.is_some() {
        let owner = crate::pr_head::owner_lock(home, run)?;
        let disk = RunState::load_under(run, home)?;
        ensure!(
            state.sync_only_passes == disk.sync_only_passes,
            "sync-only state changed during admission"
        );
        let claimed = crate::daemon::is_working_on_other(home, run, Timestamp::now())?;
        ensure!(
            disk.liveness_under(home, claimed) == crate::run::Liveness::Dead,
            "the recorded sync-only driver is live or its exit is unproven"
        );
        Some((owner, crate::driver::maintenance(home, run)?))
    } else {
        None
    };
    ensure!(
        task.execution_runs().last() == Some(&state.id)
            && (orphan.is_some()
                || matches!(
                    task.status,
                    TaskStatus::Held | TaskStatus::Parked | TaskStatus::Failed
                ))
            && !task.fresh_start
            && task.pr_recovery.is_none()
            && !crate::repair_budget::pending(task, &state),
        "sync-base-only requires the task's latest held execution with no pending replacement or repair"
    );
    let request = state
        .pending_sync_only()
        .map(|pass| pass.request.clone())
        .or_else(|| {
            orphan
                .filter(|request| {
                    !state
                        .sync_only_passes
                        .iter()
                        .any(|pass| &pass.request == *request)
                })
                .cloned()
        })
        .unwrap_or_else(|| Request {
            run: state.id.clone(),
            at: Timestamp::now(),
        });
    let at = Timestamp::now();
    crate::resume::release_standing(task);
    task.operator_decided_at = Some(at);
    task.resume_override = Some(OperatorResume {
        question_id: String::new(),
        at: request.at,
        conductor_rehold: None,
        forced: true,
        pinned_run: Some(state.id.clone()),
    });
    task.sync_only = Some(request);
    Ok(())
}

pub(crate) fn settle(
    task: &mut Task,
    request: &Request,
    state: Option<&RunState>,
    detail: Option<String>,
) {
    let pass = state
        .and_then(|state| state.sync_only_passes.last())
        .filter(|pass| pass.request == *request);
    let reason = detail
        .or_else(|| pass.map(Pass::summary))
        .unwrap_or_else(|| {
            "Base synchronization only: interrupted before start; verification was not allowed"
                .into()
        });
    task.sync_only = None;
    task.resume_override = None;
    if let Some(state) = state.filter(|state| state.status == RunStatus::AlreadyInBase)
        && pass.is_some_and(|pass| pass.result == Outcome::AlreadyInBase)
    {
        task.succeed(CompletionContext::from_run(
            CompletionActor::DaemonSettle,
            Some(reason.clone()),
            task,
            state,
        ));
        task.last_error = Some(reason);
        return;
    }
    task.last_error = Some(reason.clone());
    task.hold_after_sync_only(reason);
}

struct Checkout {
    path: PathBuf,
    disabled_filters: Vec<String>,
}

impl Checkout {
    async fn raw(&self, args: &[&str]) -> Result<git::GitOut> {
        let mut command = Vec::with_capacity(11 + 2 * self.disabled_filters.len() + args.len());
        command.extend([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
        ]);
        for config in &self.disabled_filters {
            command.extend(["-c", config.as_str()]);
        }
        command.extend_from_slice(args);
        git::git_raw_command(&self.path, &command, true).await
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        let out = self.raw(args).await?;
        ensure!(out.ok(), "Git {} failed: {}", args[0], out.stderr);
        Ok(out.stdout)
    }
}

async fn checkout(state: &RunState) -> Result<Checkout> {
    let winner = state.winner().context("retained winner is missing")?;
    let mut git = Checkout {
        path: winner
            .worktree
            .canonicalize()
            .context("retained winner checkout is missing")?,
        disabled_filters: Vec::new(),
    };
    let top = PathBuf::from(git.git(&["rev-parse", "--show-toplevel"]).await?).canonicalize()?;
    let holder = git::worktree_holding(&state.repo, &winner.branch).await?;
    ensure!(
        top == git.path
            && state.repo.canonicalize()? != git.path
            && git.path.join(".git").is_file()
            && holder.and_then(|p| p.canonicalize().ok()).as_ref() == Some(&git.path)
            && git.git(&["symbolic-ref", "HEAD"]).await? == format!("refs/heads/{}", winner.branch),
        "winner checkout no longer holds its recorded branch"
    );
    let drivers = git
        .raw(&["config", "--get-regexp", r"^merge\..*\.driver$"])
        .await?;
    ensure!(
        drivers.code == Some(1),
        "sync-base-only refuses external merge drivers or unreadable Git configuration"
    );
    let filters = git
        .raw(&[
            "config",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process)$",
        ])
        .await?;
    ensure!(
        matches!(filters.code, Some(0 | 1)),
        "cannot inspect configured Git filters"
    );
    let names: std::collections::BTreeSet<_> = filters
        .stdout
        .lines()
        .filter_map(|key| key.rsplit_once('.').map(|(name, _)| name))
        .collect();
    // An unused filter (e.g. Git for Windows' global LFS entry) is harmless.
    // A matching attribute must fail, not call a program or write raw content.
    for name in names {
        for setting in ["clean=", "smudge=", "process=", "required=true"] {
            git.disabled_filters.push(format!("{name}.{setting}"));
        }
    }
    ensure!(
        git.git(&[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=dirty"
        ])
        .await?
        .is_empty(),
        "winner checkout is dirty"
    );
    let git_dir = PathBuf::from(git.git(&["rev-parse", "--absolute-git-dir"]).await?);
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
        "sequencer",
    ] {
        ensure!(
            !git_dir.join(marker).try_exists()?,
            "winner checkout has an unfinished Git operation"
        );
    }
    Ok(git)
}

async fn fetch_base(checkout: &Checkout, state: &RunState) -> Result<String> {
    let remote = &state.config.merge.remote;
    let branch = &state.base_branch;
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    let args = [
        "fetch",
        "--quiet",
        "--no-auto-maintenance",
        "--no-tags",
        "--no-prune",
        "--no-prune-tags",
        "--recurse-submodules=no",
        "--",
        remote,
        &refspec,
    ];
    let budget = std::time::Duration::from_secs(state.config.graph.timeout_resource_wait);
    tokio::time::timeout(budget, checkout.git(&args))
        .await
        .context("fetching the required base timed out")??;
    // FETCH_HEAD belongs to the linked checkout, unlike the tracking ref which
    // the scheduler can move independently of this fetch.
    checkout
        .git(&["rev-parse", "--verify", "FETCH_HEAD^{commit}"])
        .await
}

pub(crate) async fn sync(state: &mut RunState) -> Result<Outcome> {
    let checkout = checkout(state).await?;
    let _outputs = crate::retention::OutputLease::take(&checkout.path, true)?;
    let head = checkout.git(&["rev-parse", "HEAD"]).await?;
    let pass = state.sync_only_passes.last_mut().unwrap();
    pass.head_after = Some(head.clone());
    let saved = pass.reservation.is_some() || pass.prepared_head.is_some();
    let tip = if saved {
        let op = state
            .base_sync_recovery
            .operations
            .last()
            .context("interrupted synchronization has no budget reservation")?;
        ensure!(
            pass.reservation.is_none_or(|id| id == op.id)
                && pass.head_before.as_ref() == Some(&op.head)
                && pass.tip.as_ref() == Some(&op.tip)
                && (op.head == head || pass.prepared_head.as_ref() == Some(&head)),
            "interrupted synchronization has unrecognized reservation or HEAD; inspect the retained checkout"
        );
        op.tip.clone()
    } else {
        pass.head_before = Some(head.clone());
        pass.tip = None;
        state.save()?;
        let tip = fetch_base(&checkout, state).await?;
        state.sync_only_passes.last_mut().unwrap().tip = Some(tip.clone());
        state.save()?;
        tip
    };
    let behind = git::commits_ahead(&checkout.path, &head, &tip).await?;
    let attempts = state.base_sync.as_ref().map_or(0, |sync| sync.attempts);
    if behind != 0 && !saved {
        state.ensure_base_episode();
        if state.base_episode_used() >= crate::graph::BASE_SYNC_ROUNDS {
            let why = "base synchronization budget is exhausted";
            stop(
                state,
                tip,
                behind,
                attempts,
                &head,
                why,
                crate::base_sync_recovery::StopKind::Exhausted,
            );
            bail!("{why}");
        }
        state.base_sync = Some(BaseSync {
            tip: tip.clone(),
            behind,
            attempts,
            conflict: None,
            already_in: None,
        });
        state.reserve_base_operation(&head, &tip);
        // There is no scratch rebase. The pass itself owns replay of this
        // spent reservation; record both together before building the merge.
        state.sync_only_passes.last_mut().unwrap().reservation =
            state.base_sync_recovery.operations.last().map(|op| op.id);
        state.finish_base_operation(None);
        state.save()?;
    }
    if behind != 0 {
        let prepared =
            if let Some(commit) = state.sync_only_passes.last().unwrap().prepared_head.clone() {
                commit
            } else {
                let tree = checkout
                    .raw(&["merge-tree", "--write-tree", &head, &tip])
                    .await?;
                if tree.code == Some(1) {
                    let why = format!(
                        "base merge conflicted; winner left unchanged ({})",
                        tree.stdout
                            .lines()
                            .skip(1)
                            .take(5)
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                    stop(
                        state,
                        tip,
                        behind,
                        state.base_sync.as_ref().unwrap().attempts,
                        &head,
                        &why,
                        crate::base_sync_recovery::StopKind::Conflict,
                    );
                    bail!("{why}");
                }
                ensure!(tree.ok(), "Git merge-tree failed: {}", tree.stderr);
                if head != state.base_commit
                    && tree.stdout
                        == checkout
                            .git(&["rev-parse", &format!("{tip}^{{tree}}")])
                            .await?
                {
                    return Ok(already_integrated(state, tip, behind));
                }
                let prepared = checkout
                    .git(&[
                        "-c",
                        "user.name=magi candidate",
                        "-c",
                        "user.email=magi@localhost",
                        "-c",
                        "commit.gpgSign=false",
                        "commit-tree",
                        &tree.stdout,
                        "-p",
                        &head,
                        "-p",
                        &tip,
                        "-m",
                        "Merge base before verification",
                    ])
                    .await?;
                state.sync_only_passes.last_mut().unwrap().prepared_head = Some(prepared.clone());
                state.save()?;
                prepared
            };
        ensure!(
            checkout.git(&["rev-parse", "HEAD"]).await? == head,
            "winner HEAD moved during base synchronization"
        );
        let updated = checkout
            .raw(&[
                "merge",
                "--ff-only",
                "--no-autostash",
                "--no-edit",
                "--no-verify-signatures",
                "--no-overwrite-ignore",
                &prepared,
            ])
            .await;
        state.sync_only_passes.last_mut().unwrap().head_after =
            checkout.git(&["rev-parse", "HEAD"]).await.ok();
        let updated = updated?;
        ensure!(
            updated.ok()
                && state.sync_only_passes.last().unwrap().head_after.as_ref() == Some(&prepared),
            "could not install the synchronized HEAD: {}",
            updated.stderr
        );
    }
    let head = checkout.git(&["rev-parse", "HEAD"]).await?;
    state.sync_only_passes.last_mut().unwrap().head_after = Some(head.clone());
    ensure!(
        git::is_ancestor(&checkout.path, &tip, &head).await,
        "winner does not contain the fetched base"
    );
    if head != state.base_commit
        && (head == tip
            || saved
                && checkout
                    .git(&["rev-parse", &format!("{tip}^{{tree}}")])
                    .await?
                    == checkout
                        .git(&["rev-parse", &format!("{head}^{{tree}}")])
                        .await?)
    {
        return Ok(already_integrated(state, tip, behind));
    }
    state.sync_only_passes.last_mut().unwrap().prepared_head = None;
    if saved || behind != 0 {
        state.finish_base_operation(Some(&head));
    } else {
        state.reconcile_verification_head(&head);
    }
    state.base_sync = Some(BaseSync {
        tip,
        behind: 0,
        attempts: state.base_sync.as_ref().map_or(0, |sync| sync.attempts),
        conflict: None,
        already_in: None,
    });
    state.base_sync_recovery.stop = None;
    Ok(if !saved && behind == 0 {
        Outcome::AlreadyCurrent
    } else {
        Outcome::Synchronized
    })
}

fn already_integrated(state: &mut RunState, tip: String, behind: usize) -> Outcome {
    state.sync_only_passes.last_mut().unwrap().prepared_head = None;
    state.status = RunStatus::AlreadyInBase;
    state.base_sync_recovery.stop = None;
    state.base_sync = Some(BaseSync {
        already_in: Some(crate::already::Evidence {
            proof: crate::already::Proof::Tree,
            tip: tip.clone(),
            commits: Vec::new(),
        }),
        tip,
        behind,
        attempts: state.base_sync.as_ref().map_or(0, |sync| sync.attempts),
        conflict: None,
    });
    Outcome::AlreadyInBase
}

fn stop(
    state: &mut RunState,
    tip: String,
    behind: usize,
    attempts: usize,
    head: &str,
    why: &str,
    kind: crate::base_sync_recovery::StopKind,
) {
    state.base_sync = Some(BaseSync {
        tip,
        behind,
        attempts,
        conflict: Some(why.into()),
        already_in: None,
    });
    state.event("land", why);
    state.record_base_sync_stop_as(head, Some(kind));
}
