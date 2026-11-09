//! One owner-authorized merge resolution, with a reservation before any Git work.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::{agent, git, repair_budget::Phase, run::RunState};

/// Everything needed to finish an interrupted operation without dispatching again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub paths: Vec<String>,
    pub outside: String,
    pub tree: Option<String>,
    pub result: Option<String>,
}

/// The graph has already saved Phase::Fixing and spent the one attempt.
/// A restart may validate and deliver saved work, but never invokes another seat.
pub(crate) async fn repair(state: &mut RunState, dispatch: bool) -> Result<()> {
    let index = state.repair_grants.len() - 1;
    let request = state.repair_grants[index].receipt.request.clone();
    let tip = request.tip.as_deref().context("repair base tip missing")?;
    let winner = state.winner().cloned().context("repair winner missing")?;
    let scratch = state
        .worktree_root()
        .join(format!("base-conflict-{}", request.request));
    let wait = crate::repo_lock::WaitPolicy::current();
    if dispatch {
        crate::repair_budget::recheck_conflict_delivery(state, &request).await?;
        ensure!(
            !scratch.exists(),
            "base-conflict scratch already exists; inspect the interrupted reservation; no seat restarted"
        );
        let _lease = crate::repo_lock::SeatLease::operation_with(
            &state.repo,
            state.config.coordination.as_ref(),
            &wait,
        )
        .await?;
        git::git(
            &state.repo,
            &[
                "worktree",
                "add",
                "--detach",
                scratch.to_str().context("scratch path")?,
                &request.head,
            ],
        )
        .await?;
        crate::worktree_setup::prepare(&state.config, &state.repo, &scratch).await?;
        let merged = git::git_raw(
            &scratch,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "merge",
                "--no-commit",
                "--no-ff",
                "--no-autostash",
                tip,
            ],
        )
        .await?;
        let paths = git::unmerged_paths(&scratch).await?;
        ensure!(
            !merged.ok() && !paths.is_empty(),
            "recorded textual conflict did not reproduce; inspect the reserved scratch tree; no seat dispatched"
        );
        let outside = crate::rebase::index_outside(&scratch, &paths).await?;
        state.repair_grants[index].conflict = Some(Operation {
            paths,
            outside,
            tree: None,
            result: None,
        });
        state.save()?;
        drop(_lease);
        let roles = state.config.resolve_roles()?;
        let (spec, seat_key) = crate::fixer::attempts(state, &roles, &winner)
            .into_iter()
            .next()
            .context("no fixer configured")?;
        let mut seat = crate::land::seat_of(state, &seat_key, &spec.id);
        let prompt = format!(
            "Resolve the owner-authorized retained base merge conflict.\n\nOriginal review criteria (remain in force):\n{}\n\nOwner-approved narrow repair instruction:\n{}\n\nEdit only the contents of these conflicted paths: {:?}. Preserve both histories. Do not commit, stage, abort, rebase, switch branches or change Git metadata; the launcher validates and commits the merge. No broader implementation is authorized.",
            state.instruction,
            request
                .instruction
                .as_deref()
                .context("repair instruction missing")?,
            state.repair_grants[index].conflict.as_ref().unwrap().paths
        );
        let artifacts = agent::artifacts_dir(&state.dir());
        let out = agent::invoke(
            &spec,
            &mut seat,
            &agent::Invocation {
                coordination: state.config.coordination.as_ref(),
                fetched_base: state.fetched_base.as_ref(),
                cwd: &scratch,
                prompt: &prompt,
                timeout: Duration::from_secs(state.config.graph.timeout_fix),
                allow_write: true,
                sessions: false,
                artifacts: &artifacts,
                stem: &format!("base-conflict-{}", request.request),
                run: &state.id,
                node: "base-conflict",
                cache_dir: state.config.cache_dir().as_deref(),
                attachments: &[],
                writable: &[],
            },
        )
        .await;
        state.seats.insert(seat.key.clone(), seat);
        state.save()?;
        let out = out?;
        ensure!(
            out.usable() && !out.quota_exhausted(),
            "base-conflict fixer did not finish; reservation remains consumed; inspect retained scratch contents"
        );
    }
    let operation = state.repair_grants[index].conflict.clone().context("interrupted before conflict checkpoint; inspect reserved scratch; no seat will be restarted")?;
    let _scratch_output = crate::retention::OutputLease::take(&scratch, true)
        .context("base-conflict scratch still has a live seat or descendant; wait before resuming this reservation")?;
    let _winner_output = crate::retention::OutputLease::take(&winner.worktree, true)
        .context("base-conflict winner still has a live seat or descendant; wait before resuming this reservation")?;
    let _lease = crate::repo_lock::SeatLease::operation_with(
        &state.repo,
        state.config.coordination.as_ref(),
        &wait,
    )
    .await?;
    if operation.result.is_none() {
        ensure!(
            git::rev_parse(&scratch, "HEAD").await? == request.head
                && git::rev_parse(&scratch, "MERGE_HEAD").await? == tip,
            "base-conflict merge identity changed; inspect retained scratch; no seat restarted"
        );
        ensure!(
            crate::rebase::index_outside(&scratch, &operation.paths).await? == operation.outside,
            "base-conflict repair changed staged paths outside the approved conflict"
        );
        for (xy, path) in crate::rebase::status_entries(&scratch).await? {
            ensure!(
                operation.paths.contains(&path) || (xy != "??" && xy.ends_with(' ')),
                "base-conflict repair changed unapproved path {path}"
            );
        }
        for path in &operation.paths {
            match std::fs::symlink_metadata(scratch.join(path)) {
                Ok(metadata) => ensure!(
                    metadata.is_file(),
                    "base-conflict path {path} is not a regular file"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            ensure!(
                !crate::rebase::has_markers(&scratch, path, &[&request.head, tip]).await?,
                "base-conflict repair incomplete: markers remain in {path}; inspect retained scratch; no seat restarted"
            );
        }
        let mut args = vec!["--literal-pathspecs", "add", "-A", "--"];
        args.extend(operation.paths.iter().map(String::as_str));
        git::git(&scratch, &args).await?;
        ensure!(
            git::unmerged_paths(&scratch).await?.is_empty(),
            "base-conflict repair has unmerged paths"
        );
        let tree = git::git(&scratch, &["write-tree"]).await?.trim().to_owned();
        if let Some(saved) = &operation.tree {
            ensure!(*saved == tree, "base-conflict validated tree changed");
        }
        state.repair_grants[index].conflict.as_mut().unwrap().tree = Some(tree.clone());
        state.save()?;
        let result = git::git(
            &scratch,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "commit-tree",
                &tree,
                "-p",
                &request.head,
                "-p",
                tip,
                "-m",
                "Merge base with owner-authorized conflict repair",
            ],
        )
        .await?
        .trim()
        .to_owned();
        state.repair_grants[index].conflict.as_mut().unwrap().result = Some(result);
        state.save()?;
    }
    let operation = state.repair_grants[index].conflict.as_ref().unwrap();
    let result = operation.result.as_ref().unwrap();
    ensure!(
        git::git(&state.repo, &["show", "-s", "--format=%P", result])
            .await?
            .trim()
            == format!("{} {tip}", request.head)
            && git::rev_parse(&state.repo, &format!("{result}^{{tree}}")).await?
                == *operation.tree.as_ref().context("repair tree missing")?,
        "base-conflict result identity changed"
    );
    let head = git::rev_parse(&winner.worktree, "HEAD").await?;
    ensure!(
        (head == request.head || &head == result)
            && git::current_branch(&winner.worktree).await?.as_ref() == Some(&winner.branch),
        "base-conflict winner ownership or head changed before delivery"
    );
    let clean = git::is_clean(&winner.worktree).await?;
    // A crash after compare-update may leave the index/worktree projecting
    // exactly the old HEAD. Only that provable projection may be completed.
    if !clean {
        ensure!(
            &head == result
                && git::git(&winner.worktree, &["write-tree"]).await?.trim()
                    == git::rev_parse(&winner.worktree, &format!("{}^{{tree}}", request.head))
                        .await?
                && git::git_raw(&winner.worktree, &["diff-files", "--quiet"])
                    .await?
                    .ok()
                && git::git(
                    &winner.worktree,
                    &["ls-files", "--others", "--exclude-standard"]
                )
                .await?
                .is_empty(),
            "base-conflict winner has uncommitted changes; inspect delivery checkpoint before continuing"
        );
    }
    crate::repair_budget::recheck_conflict_delivery(state, &request).await?;
    if &head != result {
        git::git(
            &winner.worktree,
            &[
                "update-ref",
                &format!("refs/heads/{}", winner.branch),
                result,
                &request.head,
            ],
        )
        .await?;
    }
    if &head != result || !clean {
        git::git(&winner.worktree, &["read-tree", "--reset", "-u", result]).await?;
    }
    ensure!(
        git::is_clean(&winner.worktree).await?,
        "base-conflict delivery did not produce a clean checkout"
    );
    ensure!(
        state.repair_grants[index].phase == Phase::Fixing,
        "repair phase changed"
    );
    Ok(())
}
