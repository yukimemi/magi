//! Task-scoped publication, with a lease pinned to the last successful push.

use std::path::Path;

use crate::{
    git, land,
    run::{self, Candidate, Liveness, PrRecord, RunState},
};
use anyhow::{Context, Result, bail};

/// Serializes driver claims with publication takeover. The file stays in place:
/// unlinking a locked inode would let a second caller lock a different one.
/// Contention fails closed rather than blocking an async executor.
pub(crate) fn owner_lock(home: &Path, id: &str) -> Result<std::fs::File> {
    let path = home.join("runs").join(id).join("pr-owner.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .context("open public head ownership lock")?;
    try_lock_file(&file).context("public head ownership is being updated; retry later")?;
    Ok(file)
}

// std::fs::File locking requires Rust 1.89; keep the declared 1.85 MSRV.
#[cfg(unix)]
pub(crate) fn try_lock_file(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor belongs to a live File. flock does not retain
    // pointers; closing the File releases the lock, including on process exit.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn try_lock_file(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFile(
            file: *mut std::ffi::c_void,
            offset_low: u32,
            offset_high: u32,
            length_low: u32,
            length_high: u32,
        ) -> i32;
    }
    // SAFETY: this is a live file handle and the Win32 LockFile ABI. LockFile
    // fails immediately on contention; closing the handle releases the lock.
    if unsafe { LockFile(file.as_raw_handle(), 0, 0, 1, 0) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn ensure_owner(state: &RunState) -> Result<()> {
    let disk = run::try_home().and_then(|home| RunState::load_under(&state.id, &home).ok());
    ensure_owner_record(state, disk.as_ref())
}

/// The same ownership check with an explicitly loaded disk record.
pub(crate) fn ensure_owner_record(state: &RunState, disk: Option<&RunState>) -> Result<()> {
    if state
        .merge_decision
        .as_ref()
        .is_some_and(|d| d.closed || d.choice == crate::merge_decision::DISMISS)
        || disk.is_some_and(|s| {
            s.merge_decision
                .as_ref()
                .is_some_and(|d| d.closed || d.choice == crate::merge_decision::DISMISS)
        })
    {
        bail!("this execution was retired by an owner decision; resume the current task instead");
    }
    if let Some(next) = state
        .pr_taken_over_by
        .as_ref()
        .or_else(|| disk.and_then(|s| s.pr_taken_over_by.as_ref()))
    {
        bail!(
            "pull request ownership was superseded by run {}",
            run::short_of(next)
        );
    }
    Ok(())
}

/// The remote is absent, exactly the head we last published, or already at the
/// tip being pushed (an idempotent re-push).
fn lease_for(remote: Option<&str>, expected: &[&str], local: &str) -> Result<String> {
    match remote {
        None => Ok(String::new()),
        Some(head) if expected.contains(&head) || head == local => Ok(head.to_owned()),
        Some(_) => bail!("public head moved or has no recorded lease; refusing to overwrite it"),
    }
}

fn selected_lease(remote: Option<&str>, expected: &str) -> Result<String> {
    if remote == Some(expected) {
        Ok(expected.to_owned())
    } else {
        bail!("selected public head moved or disappeared; refusing to overwrite it")
    }
}

async fn predecessors(
    state: &RunState,
    home: &Path,
    head: &str,
) -> Result<Vec<(RunState, std::fs::File)>> {
    let common = git::git(
        &state.repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let mut previous = Vec::new();
    for id in run::list_ids_in(&home.join("runs")) {
        if id == state.id {
            continue;
        }
        let path = home.join("runs").join(&id).join("run.json");
        let Ok(body) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(view) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        let selected_url = state.pr_recovery.as_ref().map(|s| s.url.as_str());
        let names_selected_pr =
            selected_url.is_some() && view["pr"]["url"].as_str() == selected_url;
        if view["pr_head"].as_str() != Some(head) && !names_selected_pr {
            continue;
        }
        let _lock = owner_lock(home, &id)?;
        // The scan only selects possible owners; validate a fresh record under
        // the same lock the driver claim uses, and hold it through takeover.
        let mut old = RunState::load_under(&id, home)?;
        let other = git::git(
            &old.repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        if other != common {
            continue;
        }
        if old
            .pr_taken_over_by
            .as_deref()
            .is_some_and(|next| next != state.id)
        {
            continue;
        }
        let claimed = crate::daemon::is_working_on(home, &id, jiff::Timestamp::now());
        validate_predecessor(&mut old, state, home, claimed)?;
        previous.push((old, _lock));
    }
    previous.sort_by_key(|(s, _)| s.updated_at);
    Ok(previous)
}

/// A released execution has no remaining graph to resume. Only an explicit
/// completed handover licenses retiring a non-terminal predecessor, even if
/// its immediate successor handed the work on again;
/// all other interrupted runs must resume their existing graph first.
fn validate_predecessor(
    old: &mut RunState,
    next: &RunState,
    home: &Path,
    claimed: bool,
) -> Result<()> {
    match old.liveness_under(home, claimed) {
        Liveness::Live => bail!(
            "previous public head owner {} still has a live driver",
            old.short()
        ),
        Liveness::Unknown => bail!(
            "previous public head owner {} driver state unknown; recheck and retry, or hold the task; an owner answer cannot prove driver death",
            old.short()
        ),
        Liveness::Dead => {}
    }
    if crate::handover::has_resume_checkpoint(old, home) {
        bail!(
            "previous public head owner {} has resumable work; resume that run instead of publishing a successor",
            old.short()
        );
    }
    if !old.status.done() {
        if old.released_to.is_none()
            || old.candidates.iter().any(|c| {
                old.released_branches.contains(&c.branch) && c.worktree.try_exists().unwrap_or(true)
            })
        {
            bail!(
                "previous public head owner {} has resumable work; resume that run instead of publishing a successor",
                old.short()
            );
        }
        old.status = crate::run::RunStatus::Superseded;
        old.event(
            "publish",
            format!(
                "execution retired after completed worktree handover to {}; current publisher {}",
                old.released_to.as_deref().expect("checked handover"),
                next.id
            ),
        );
        old.save_under(home)?;
    }
    Ok(())
}

/// Refuse startup while a historical publisher is still active or ambiguous.
pub(crate) async fn check_predecessors(state: &RunState, head: &str) -> Result<()> {
    predecessors(state, &run::home(), head).await?;
    Ok(())
}

/// Whether `old` is handing over ownership of the open pull request `url`.
/// Merged or closed records, other pull requests and records with no pull
/// request (ownership unknown) stay history and are never superseded.
fn transfers_ownership(old: &RunState, url: &str) -> bool {
    old.pr
        .as_ref()
        .is_some_and(|p| p.url == url && p.state != "merged" && p.state != "closed")
}

pub(crate) async fn publish(
    state: &mut RunState,
    winner: &Candidate,
    title: &str,
    body: &str,
) -> Result<String> {
    ensure_owner(state)?;
    let head = state.task_pr_branch().context("missing task public head")?;
    let replacement_body;
    let body = if let Some(previous) = &state.replaces {
        replacement_body = format!(
            "{body}\n\nReimplements {} in a new attempt (`{}`), following owner decision `{}`. The previous pull request remains closed; its approvals do not apply here.",
            previous.pr, state.id, previous.question
        );
        replacement_body.as_str()
    } else {
        body
    };
    let home = run::home();
    let previous = predecessors(state, &home, &head).await?;
    // The lease is the head this run pushed, else any head a live-checked
    // predecessor pushed: merged runs are not superseded, so "latest by
    // updated_at" is not a reliable pusher.
    let expected: Vec<&str> = match state.pr_pushed_head.as_deref() {
        Some(h) => vec![h],
        None if state.pr_recovery.is_some() => {
            vec![state.pr_recovery.as_ref().expect("selected").head.as_str()]
        }
        None => previous
            .iter()
            .filter_map(|(s, _)| s.pr_pushed_head.as_deref())
            .collect(),
    };
    // Query before any mutation: ambiguous or unreadable PR lists cannot create duplicates.
    let found = land::find_open_pr(&winner.worktree, &head, &state.base_branch).await?;
    if let land::OpenPr::Many(_) = found {
        bail!("several open pull requests name the task head");
    }
    if let Some(selected) = &state.pr_recovery {
        selected
            .verify(
                &winner.worktree,
                &state.config.merge.remote,
                &state.base_branch,
                state.pr_pushed_head.as_deref().unwrap_or(&selected.head),
            )
            .await?;
    }
    let remote = state.config.merge.remote.clone();
    let remote_head = git::remote_head(&winner.worktree, &remote, &head).await?;
    let local_tip = git::rev_parse(&winner.worktree, &winner.branch).await?;
    let expected = if let Some(selected) = &state.pr_recovery {
        selected_lease(
            remote_head.as_deref(),
            state.pr_pushed_head.as_deref().unwrap_or(&selected.head),
        )?
    } else {
        lease_for(remote_head.as_deref(), &expected, &local_tip)?
    };
    let posted_title = state.pr_posted_title.clone().or_else(|| match &found {
        land::OpenPr::One { url, .. } => previous
            .iter()
            .rev()
            .find(|(s, _)| transfers_ownership(s, url))
            .and_then(|(s, _)| s.pr_posted_title.clone()),
        _ => None,
    });
    if let land::OpenPr::One { ref url, .. } = found {
        let (ok, raw) = land::gh(
            &winner.worktree,
            &[
                "pr".into(),
                "view".into(),
                url.clone(),
                "--json".into(),
                "headRefName,baseRefName,isCrossRepository,headRefOid,state,autoMergeRequest"
                    .into(),
            ],
        )
        .await?;
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Head {
            head_ref_name: String,
            base_ref_name: String,
            is_cross_repository: bool,
            head_ref_oid: String,
            state: String,
            #[serde(default)]
            auto_merge_request: Option<serde_json::Value>,
        }
        if !ok {
            bail!("could not verify task PR head: {raw}");
        }
        let pr_head: Head = serde_json::from_str(&raw).context("verify task PR head")?;
        if pr_head.state != "OPEN"
            || pr_head.is_cross_repository
            || pr_head.head_ref_name != head
            || pr_head.base_ref_name != state.base_branch
            || remote_head
                .as_deref()
                .is_some_and(|sha| sha != pr_head.head_ref_oid)
        {
            bail!("task pull request does not name the expected repository head");
        }
        // A predecessor's approval must not arm a merge of the replacement
        // head. Only a pull request that actually has auto-merge armed is
        // touched: the forge may refuse to disable what is not enabled.
        if pr_head.auto_merge_request.is_some_and(|v| !v.is_null()) {
            checked_gh(&winner.worktree, &["pr", "merge", url, "--disable-auto"]).await?;
        }
    }
    let tip = git::rev_parse(&winner.worktree, &format!("refs/heads/{}", winner.branch)).await?;
    let push = git::push_as(&winner.worktree, &remote, &winner.branch, &head, &expected).await?;
    if !push.ok() {
        bail!("public head push refused: {}", push.stderr);
    }
    state.pr_head = Some(head.clone());
    state.pr_pushed_head = Some(tip.clone());
    // Durable before metadata calls: an interrupted publication can retry its own lease.
    state.save()?;
    let url = match found {
        land::OpenPr::None => {
            let url = crate::graph::gh_pr_create(
                &winner.worktree,
                &state.base_branch,
                &head,
                title,
                body,
            )
            .await?;
            state.pr_posted_title = Some(title.to_owned());
            url
        }
        land::OpenPr::One {
            url,
            title: current,
        } => {
            let retitle = posted_title.as_deref() == Some(current.as_str())
                || crate::graph::should_retitle(&current, title, &[]);
            let mut args = vec!["pr", "edit", url.as_str(), "--body", body];
            if retitle {
                args.extend(["--title", title]);
            }
            checked_gh(&winner.worktree, &args).await?;
            state.pr_posted_title = if retitle {
                Some(title.to_owned())
            } else {
                posted_title
            };
            url
        }
        land::OpenPr::Many(_) => unreachable!(),
    };
    let number = url.rsplit('/').next().context("PR number")?.parse()?;
    state.pr = Some(PrRecord {
        url: url.clone(),
        number,
        state: "open".into(),
        checks: "unknown".into(),
        round: 0,
        rounds: state.config.graph.land_rounds,
        red_at_merge: Vec::new(),
    });
    state.save()?;
    for (old, _lock) in previous {
        // Re-read before writing, using the same terminal/driver guard.
        let mut old = RunState::load_under(&old.id, &home)?;
        let claimed = crate::daemon::is_working_on(&home, &old.id, jiff::Timestamp::now());
        validate_predecessor(&mut old, state, &home, claimed)?;
        if old.pr_taken_over_by.as_deref() == Some(state.id.as_str()) {
            continue;
        }
        if !transfers_ownership(&old, &url) {
            continue;
        }
        crate::followup::supersede_deputy_tasks(
            &mut old,
            &url,
            &crate::queue::Queue::at(home.join("queue")),
            &state.id,
        );
        old.pr_taken_over_by = Some(state.id.clone());
        old.event(
            "merge",
            format!("pull request superseded by run {}", state.short()),
        );
        old.save_under(&home)?;
        let comment = format!(
            "Run {} replaced the head with {tip}, superseding run {}. Review and CI history remain on this pull request.",
            state.short(),
            old.short()
        );
        // Best effort: the head is already published and recorded, and a
        // failed note must not turn a landed update into a failed step.
        if let Err(e) = checked_gh(
            &winner.worktree,
            &["pr", "comment", &url, "--body", &comment],
        )
        .await
        {
            tracing::warn!("takeover comment on {url} failed: {e:#}");
        }
    }
    if let Some(previous) = &state.replaces {
        if let Ok(mut old) = RunState::load(&previous.run)
            && let Some(decision) = &mut old.merge_decision
        {
            decision.replacement_pr = Some(url.clone());
            old.save()?;
        }
        let comment = format!(
            "Replacement attempt `{}` opened {url}, following owner decision `{}`. This PR remains closed; its approvals do not authorize the replacement.",
            state.id, previous.question
        );
        if let Err(e) = checked_gh(
            &winner.worktree,
            &["pr", "comment", &previous.pr, "--body", &comment],
        )
        .await
        {
            state.event(
                "replacement",
                format!("could not link the old PR to {url}: {e:#}"),
            );
        }
    }
    state.event("merge", format!("published task head {head}: {url}"));
    Ok(url)
}

async fn checked_gh(repo: &Path, args: &[&str]) -> Result<()> {
    let args = args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let (ok, output) = land::gh(repo, &args).await?;
    if !ok {
        bail!("gh publication failed: {output}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn predecessor(home: &Path) -> RunState {
        let mut s = RunState::new(
            "repo".into(),
            "main".into(),
            "base".into(),
            "task".into(),
            crate::config::Config::default(),
        );
        s.status = crate::run::RunStatus::Ready;
        s.save_under(home).unwrap();
        s
    }

    #[test]
    fn takeover_needs_proof_and_preserves_live_or_unknown_drivers() {
        let home = tempfile::tempdir().unwrap();
        let mut old = predecessor(home.path());
        let next = predecessor(home.path());
        assert!(validate_predecessor(&mut old, &next, home.path(), false).is_err());
        old.driver_exited = true;
        assert!(validate_predecessor(&mut old, &next, home.path(), false).is_ok());
        assert!(validate_predecessor(&mut old, &next, home.path(), true).is_err());
        old.driver_exited = false;
        old.driver_pid = Some(std::process::id());
        old.driver_started_at = crate::proc::process_started_at(std::process::id());
        assert!(validate_predecessor(&mut old, &next, home.path(), false).is_err());
        old.driver_started_at = Some("1".into());
        assert!(validate_predecessor(&mut old, &next, home.path(), false).is_ok());
    }

    #[test]
    fn a_stopped_gating_predecessor_retires_only_after_explicit_handover() {
        let home = tempfile::tempdir().unwrap();
        let mut old = predecessor(home.path());
        let next = predecessor(home.path());
        old.status = crate::run::RunStatus::Gating;
        old.driver_exited = true;
        assert!(validate_predecessor(&mut old, &next, home.path(), false).is_err());
        assert_eq!(old.status, crate::run::RunStatus::Gating);
        old.released_to = Some(next.id.clone());
        old.pr_publication_pending = true;
        assert!(validate_predecessor(&mut old, &next, home.path(), false).is_err());
        old.pr_publication_pending = false;
        assert!(validate_predecessor(&mut old, &next, home.path(), false).is_ok());
        let disk = RunState::load_under(&old.id, home.path()).unwrap();
        assert_eq!(disk.status, crate::run::RunStatus::Superseded);
        assert_eq!(disk.released_to, Some(next.id));
    }

    #[test]
    fn successive_handovers_do_not_strand_a_stopped_gating_publisher() {
        let home = tempfile::tempdir().unwrap();
        let mut a = predecessor(home.path());
        let mut b = predecessor(home.path());
        let c = predecessor(home.path());
        for state in [&mut a, &mut b] {
            state.status = crate::run::RunStatus::Gating;
            state.driver_exited = true;
            state.pr_head = Some("task-head".into());
        }
        a.released_to = Some(b.id.clone());
        b.released_to = Some(c.id.clone());
        let worktree = home.path().join("released-worktree");
        std::fs::create_dir(&worktree).unwrap();
        a.released_branches.push("branch".into());
        a.candidates.push(Candidate {
            index: 0,
            label: 'A',
            agent: "existing".into(),
            branch: "branch".into(),
            worktree: worktree.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: true,
        });
        a.pr_publication_pending = true;
        assert!(validate_predecessor(&mut a, &c, home.path(), false).is_err());
        a.pr_publication_pending = false;
        // Saving the handover marker precedes removal. A crash in that gap
        // must not be mistaken for a completed transfer.
        assert!(validate_predecessor(&mut a, &c, home.path(), false).is_err());
        std::fs::remove_dir(worktree).unwrap();
        assert!(validate_predecessor(&mut a, &c, home.path(), false).is_ok());
        assert_eq!(a.status, crate::run::RunStatus::Superseded);
        assert_eq!(a.released_to.as_deref(), Some(b.id.as_str()));
        assert!(validate_predecessor(&mut b, &c, home.path(), true).is_err());
        assert!(validate_predecessor(&mut b, &c, home.path(), false).is_ok());
        assert_eq!(b.status, crate::run::RunStatus::Superseded);
    }

    #[test]
    fn selected_public_lease_refuses_moves_and_deletion_even_after_validation() {
        assert_eq!(
            selected_lease(Some("approved"), "approved").unwrap(),
            "approved"
        );
        assert!(selected_lease(Some("historical-candidate"), "approved").is_err());
        assert!(selected_lease(Some("repaired-but-unrecorded"), "approved").is_err());
        assert!(selected_lease(None, "approved").is_err());
    }

    #[test]
    fn public_head_lease_refuses_unknown_or_moved_remote() {
        assert_eq!(lease_for(None, &[], "l").unwrap(), "");
        assert_eq!(lease_for(Some("a"), &["a"], "l").unwrap(), "a");
        assert_eq!(lease_for(Some("b"), &["a", "b"], "l").unwrap(), "b");
        assert!(lease_for(Some("b"), &["a"], "l").is_err());
        assert!(lease_for(Some("a"), &[], "l").is_err());
        // Already at the tip being pushed: allowed even with no record.
        assert_eq!(lease_for(Some("l"), &[], "l").unwrap(), "l");
    }
}
