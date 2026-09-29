//! Taking a branch over from an earlier attempt at the same task.
//!
//! A retried task can reopen a branch (`magi/f82f/A`) that the *previous*
//! run still has checked out in its own worktree. Git holds a branch in one
//! worktree at a time, so the new run's review cannot start, and the only way
//! forward used to be an agent asking the operator whether the old worktree
//! may be deleted. Auto-fold cannot help: it only folds terminal runs, and an
//! unfinished run keeps its worktrees on purpose so it can be resumed.
//!
//! When the run holding the branch is one this attempt supersedes
//! ([`crate::queue::Task::earlier_attempts`]), nothing is left to resume it
//! into, so its worktree - and only its worktree - is released before the
//! review starts. The branch, every commit and any pull request stay
//! exactly where they were.
//!
//! [`decide`] is pure; [`release`] is the only function here that touches git
//! or disk. Only a queue-driven review takes over anything: a hand-run
//! `magi review` has no task, so it carries no [`Takeover`] and behaves as it
//! always did.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::git;
use crate::run::{Liveness, RunState, RunStatus};

/// The earlier attempts at a task a new run may take a branch over from.
#[derive(Debug, Clone)]
pub struct Takeover {
    /// Ids of runs the new attempt supersedes, from
    /// [`crate::queue::Task::earlier_attempts`].
    pub earlier: Vec<String>,
    /// The magi home their records live under.
    pub home: PathBuf,
}

/// What is known about the run and worktree holding the branch.
#[derive(Debug, Clone)]
pub struct Holder {
    /// The run's id.
    pub run: String,
    /// Where the run got to.
    pub status: RunStatus,
    /// Whether anything is driving it.
    pub liveness: Liveness,
    /// Any uncommitted change, untracked files included.
    pub dirty: bool,
    /// HEAD of the worktree.
    pub head: String,
    /// The branch tip in the repository.
    pub tip: String,
}

/// The verdict of [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Safe to release the worktree.
    Release,
    /// Ours to decide about, but not safe; the text says why, for the
    /// operator.
    Refuse(String),
    /// Not a superseded run's worktree: leave it alone and let git's own
    /// refusal stand.
    NotOurs,
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Whether `holder`'s worktree may be released to a new run.
///
/// [`Liveness::Unknown`] releases: it means no daemon claims the run and its
/// driver could not be proven alive, which is exactly what a stale run whose
/// driver died looks like. Only a *proven* live run is refused.
pub fn decide(superseded: bool, holder: &Holder) -> Decision {
    if !superseded {
        return Decision::NotOurs;
    }
    let mut why = Vec::new();
    if holder.liveness == Liveness::Live {
        why.push("that run is being worked on right now".to_owned());
    }
    if holder.dirty {
        why.push("its worktree has uncommitted changes".to_owned());
    }
    if holder.head != holder.tip {
        why.push("its HEAD is not at the branch tip".to_owned());
    }
    if why.is_empty() {
        return Decision::Release;
    }
    Decision::Refuse(format!(
        "run {} (status `{}`, worktree {}, HEAD {}, branch tip {}) is an earlier attempt \
         at this task and still has the branch checked out, so it was not released \
         automatically: {}",
        crate::run::short_of(&holder.run),
        holder.status.as_str(),
        if holder.dirty { "dirty" } else { "clean" },
        short(&holder.head),
        short(&holder.tip),
        why.join("; ")
    ))
}

/// Read what [`decide`] needs from disk and git.
async fn inspect(
    repo: &Path,
    branch: &str,
    path: &Path,
    state: &RunState,
    home: &Path,
) -> Result<Holder> {
    let claimed = crate::daemon::is_working_on(home, &state.id, jiff::Timestamp::now());
    Ok(Holder {
        run: state.id.clone(),
        status: state.status,
        liveness: state.liveness(claimed),
        dirty: !git::status_porcelain(path).await?.trim().is_empty(),
        head: git::rev_parse(path, "HEAD").await?,
        tip: git::rev_parse(repo, &format!("refs/heads/{branch}")).await?,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Release the worktree holding `branch` to `new_run`, when an earlier attempt
/// at the same task holds it and it is safe. Returns the run whose worktree
/// was released, or `None` when nothing needed (or was allowed) to happen.
///
/// An `Err` is a refusal or a failed removal; the message is what the
/// operator reads. Nothing is left half-done: the old run's record is written
/// before the removal and rolled back if git refuses it.
pub async fn release(
    repo: &Path,
    branch: &str,
    new_run: &str,
    takeover: &Takeover,
) -> Result<Option<String>> {
    let Some(path) = git::worktree_holding(repo, branch).await? else {
        return Ok(None);
    };
    // The holder has to be an unfolded candidate worktree of a run in the
    // list. One nobody recorded (made by hand, say) is not ours to touch.
    let mut owner = None;
    for id in &takeover.earlier {
        let Ok(state) = RunState::load_under(id, &takeover.home) else {
            continue;
        };
        if let Some(i) = state
            .candidates
            .iter()
            .position(|c| !c.folded && same_path(&c.worktree, &path))
        {
            owner = Some((state, i));
            break;
        }
    }
    let Some((mut state, index)) = owner else {
        return Ok(None);
    };

    let holder = inspect(repo, branch, &path, &state, &takeover.home).await?;
    match decide(true, &holder) {
        Decision::NotOurs => return Ok(None),
        Decision::Refuse(why) => bail!(
            "branch `{branch}` is checked out in {}: {why}. Commit or discard the work there \
             and remove that worktree (`git worktree remove`), or say the run may be \
             discarded, and try again.",
            path.display()
        ),
        Decision::Release => {}
    }

    let old_id = state.id.clone();
    state.candidates[index].folded = true;
    state.released_to = Some(new_run.to_owned());
    if !state.released_branches.iter().any(|b| b == branch) {
        state.released_branches.push(branch.to_owned());
    }
    state.event(
        "release",
        format!(
            "worktree of `{branch}` released to run {}; this run can no longer be resumed \
             from here",
            crate::run::short_of(new_run)
        ),
    );
    state.save_under(&takeover.home)?;

    // Re-read right before the removal: `worktree_remove` is `--force`, so
    // anything that changed since the first look (a driver coming back, an
    // edit) would be thrown away. Ignored files such as `target/` go with the
    // worktree by design.
    let again = inspect(repo, branch, &path, &state, &takeover.home).await;
    let safe = matches!(&again, Ok(h) if decide(true, h) == Decision::Release);
    let removed = safe && git::worktree_remove(repo, &path).await.unwrap_or(false);
    if !removed || path.exists() {
        state = RunState::load_under(&old_id, &takeover.home)?;
        state.candidates[index].folded = false;
        state.released_to = None;
        state.released_branches.retain(|b| b != branch);
        state.events.pop();
        state.save_under(&takeover.home)?;
        bail!(
            "branch `{branch}` is checked out in {} by run {}, and releasing that worktree \
             failed or found it changed; it was left as it was",
            path.display(),
            crate::run::short_of(&old_id)
        );
    }
    Ok(Some(old_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder() -> Holder {
        Holder {
            run: "20260901-000000-f82f".to_owned(),
            status: RunStatus::Gating,
            liveness: Liveness::Unknown,
            dirty: false,
            head: "a".repeat(40),
            tip: "a".repeat(40),
        }
    }

    #[test]
    fn a_clean_superseded_stale_run_is_released() {
        assert_eq!(decide(true, &holder()), Decision::Release);
        let dead = Holder {
            liveness: Liveness::Dead,
            ..holder()
        };
        assert_eq!(decide(true, &dead), Decision::Release);
    }

    #[test]
    fn a_run_that_is_not_superseded_is_not_ours() {
        assert_eq!(decide(false, &holder()), Decision::NotOurs);
    }

    #[test]
    fn a_dirty_worktree_is_refused_and_says_why() {
        let dirty = Holder {
            dirty: true,
            ..holder()
        };
        let Decision::Refuse(why) = decide(true, &dirty) else {
            panic!("dirty must be refused");
        };
        assert!(why.contains("uncommitted"), "{why}");
        assert!(why.contains("f82f") && why.contains("gating") && why.contains("dirty"));
    }

    #[test]
    fn a_live_run_is_refused() {
        let live = Holder {
            liveness: Liveness::Live,
            ..holder()
        };
        let Decision::Refuse(why) = decide(true, &live) else {
            panic!("live must be refused");
        };
        assert!(why.contains("right now"), "{why}");
    }

    #[test]
    fn a_head_off_the_tip_is_refused() {
        let off = Holder {
            head: "b".repeat(40),
            ..holder()
        };
        assert!(matches!(decide(true, &off), Decision::Refuse(_)));
    }
}
