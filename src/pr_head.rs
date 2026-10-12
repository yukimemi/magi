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
fn try_lock_file(file: &std::fs::File) -> std::io::Result<()> {
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
fn try_lock_file(file: &std::fs::File) -> std::io::Result<()> {
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
    if let Some(next) = state
        .pr_taken_over_by
        .as_ref()
        .or_else(|| disk.as_ref().and_then(|s| s.pr_taken_over_by.as_ref()))
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
        if view["pr_head"].as_str() != Some(head) {
            continue;
        }
        let _lock = owner_lock(home, &id)?;
        // The scan only selects possible owners; validate a fresh record under
        // the same lock the driver claim uses, and hold it through takeover.
        let old = RunState::load_under(&id, home)?;
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
        if !old.status.done() || old.liveness(claimed) != Liveness::Dead {
            bail!(
                "previous public head owner {} is active or its driver is unknown",
                old.short()
            );
        }
        previous.push((old, _lock));
    }
    previous.sort_by_key(|(s, _)| s.updated_at);
    Ok(previous)
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
    let home = run::home();
    let previous = predecessors(state, &home, &head).await?;
    // The lease is the head this run pushed, else any head a live-checked
    // predecessor pushed: merged runs are not superseded, so "latest by
    // updated_at" is not a reliable pusher.
    let expected: Vec<&str> = match state.pr_pushed_head.as_deref() {
        Some(h) => vec![h],
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
    let remote = state.config.merge.remote.clone();
    let remote_head = git::remote_head(&winner.worktree, &remote, &head).await?;
    let local_tip = git::rev_parse(&winner.worktree, &winner.branch).await?;
    let expected = lease_for(remote_head.as_deref(), &expected, &local_tip)?;
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
        if !old.status.done() || old.liveness(claimed) != Liveness::Dead {
            bail!("previous owner became active during publication");
        }
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
