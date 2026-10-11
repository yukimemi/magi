//! Task-scoped publication, with a lease pinned to the last successful push.

use std::path::Path;

use crate::{
    git, land,
    run::{self, Candidate, Liveness, PrRecord, RunState},
};
use anyhow::{Context, Result, bail};

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
fn lease_for(remote: Option<&str>, expected: Option<&str>, local: &str) -> Result<String> {
    match remote {
        None => Ok(String::new()),
        Some(head) if Some(head) == expected || head == local => Ok(head.to_owned()),
        Some(_) => bail!("public head moved or has no recorded lease; refusing to overwrite it"),
    }
}

async fn predecessors(state: &RunState, home: &Path, head: &str) -> Result<Vec<RunState>> {
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
        let old: RunState =
            serde_json::from_value(view).context("read previous public head owner")?;
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
        previous.push(old);
    }
    previous.sort_by_key(|s| s.updated_at);
    Ok(previous)
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
    let expected = state
        .pr_pushed_head
        .as_deref()
        .or_else(|| previous.last().and_then(|s| s.pr_pushed_head.as_deref()));
    let posted_title = state
        .pr_posted_title
        .clone()
        .or_else(|| previous.last().and_then(|s| s.pr_posted_title.clone()));
    // Query before any mutation: ambiguous or unreadable PR lists cannot create duplicates.
    let found = land::find_open_pr(&winner.worktree, &head, &state.base_branch).await?;
    if let land::OpenPr::Many(_) = found {
        bail!("several open pull requests name the task head");
    }
    let remote = state.config.merge.remote.clone();
    let remote_head = git::remote_head(&winner.worktree, &remote, &head).await?;
    let local_tip = git::rev_parse(&winner.worktree, &winner.branch).await?;
    let expected = lease_for(remote_head.as_deref(), expected, &local_tip)?;
    if let land::OpenPr::One { ref url, .. } = found {
        let (ok, raw) = land::gh(
            &winner.worktree,
            &[
                "pr".into(),
                "view".into(),
                url.clone(),
                "--json".into(),
                "headRefName,baseRefName,isCrossRepository,headRefOid,state".into(),
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
        // A predecessor's approval must not arm a merge of the replacement head.
        checked_gh(&winner.worktree, &["pr", "merge", url, "--disable-auto"]).await?;
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
    for old in previous {
        // Re-read before writing, using the same terminal/driver guard.
        let mut old = RunState::load_under(&old.id, &home)?;
        let claimed = crate::daemon::is_working_on(&home, &old.id, jiff::Timestamp::now());
        if !old.status.done() || old.liveness(claimed) != Liveness::Dead {
            bail!("previous owner became active during publication");
        }
        if old.pr_taken_over_by.as_deref() == Some(state.id.as_str()) {
            continue;
        }
        if old.pr.as_ref().is_some_and(|p| p.url == url) {
            crate::followup::supersede_deputy_tasks(
                &mut old,
                &url,
                &crate::queue::Queue::at(home.join("queue")),
                &state.id,
            );
        }
        old.pr_taken_over_by = Some(state.id.clone());
        old.event(
            "merge",
            format!("pull request superseded by run {}", state.short()),
        );
        old.save_under(&home)?;
        if old.pr.as_ref().is_some_and(|p| p.url == url) {
            let comment = format!(
                "Run {} replaced the head with {tip}, superseding run {}. Review and CI history remain on this pull request.",
                state.short(),
                old.short()
            );
            checked_gh(
                &winner.worktree,
                &["pr", "comment", &url, "--body", &comment],
            )
            .await?;
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
        assert_eq!(lease_for(None, None, "l").unwrap(), "");
        assert_eq!(lease_for(Some("a"), Some("a"), "l").unwrap(), "a");
        assert!(lease_for(Some("b"), Some("a"), "l").is_err());
        assert!(lease_for(Some("a"), None, "l").is_err());
        // Already at the tip being pushed: allowed even with no record.
        assert_eq!(lease_for(Some("l"), None, "l").unwrap(), "l");
    }
}
