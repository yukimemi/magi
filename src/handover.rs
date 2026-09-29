//! Taking a branch over from an earlier attempt at the same task.
//!
//! A retried task can reopen a branch (`magi/f82f/A`) that the *previous*
//! run still has checked out in its own worktree. Git holds a branch in one
//! worktree at a time, so the new run's review cannot start, and the only way
//! forward used to be an agent asking the operator whether the old worktree
//! may be deleted. Auto-fold cannot help: it only folds terminal runs, and an
//! unfinished run keeps its worktrees on purpose so it can be resumed.
//!
//! When the run holding the branch is an earlier attempt at the same task
//! ([`crate::queue::Task::earlier_attempts`]) - superseded or not: blocked,
//! failed, stale or parked alike - and nothing is driving it, its worktree
//! (and only its worktree) is released before the review starts. The
//! branch, every commit and any pull request stay exactly where they were.
//!
//! [`decide`] is pure; [`release`] is the only function here that touches git
//! or disk. Only a queue-driven review takes over anything: a hand-run
//! `magi review` has no task, so it carries no [`Takeover`] and behaves as it
//! always did.

use std::path::{Path, PathBuf};

use anyhow::Result;

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

/// The operator has to decide: the takeover was refused, and the text says why.
///
/// A distinct type so the queue loop can hold the task for a person instead
/// of spending an attempt on it (see `daemon::attempt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// A worktree that was released, with what is needed to put it back if the
/// review that took the branch over never starts.
#[derive(Debug, Clone)]
pub struct Released {
    /// The run whose worktree was released.
    pub old_id: String,
    index: usize,
    path: PathBuf,
    home: PathBuf,
    /// The branch tip when the worktree was released; the branch sync may
    /// have moved it since.
    tip: String,
}

impl Released {
    /// Undo the release after the new run failed to start: check the branch
    /// out again at the old path and unmark the old run. Best-effort - if the
    /// worktree cannot be re-added the old run stays marked, which is true.
    pub async fn restore(&self, repo: &Path, branch: &str) {
        let path = self.path.to_string_lossy().to_string();
        // Nothing holds the branch now, so it can be put back where the old
        // run left it before its worktree is recreated at that commit.
        let refname = format!("refs/heads/{branch}");
        if git::rev_parse(repo, &refname).await.ok().as_deref() != Some(self.tip.as_str())
            && let Err(e) = git::git(repo, &["branch", "-f", branch, &self.tip]).await
        {
            tracing::warn!("could not put `{branch}` back at {}: {e:#}", self.tip);
            return;
        }
        if let Err(e) = git::git(repo, &["worktree", "add", &path, branch]).await {
            tracing::warn!(
                "could not put run {}'s worktree back at {path}: {e:#}",
                self.old_id
            );
            return;
        }
        let put_back = RunState::load_under(&self.old_id, &self.home).and_then(|mut s| {
            if let Some(c) = s.candidates.get_mut(self.index) {
                c.folded = false;
            }
            s.released_to = None;
            s.released_branches.retain(|b| b != branch);
            s.events.pop();
            s.save_under(&self.home)
        });
        if let Err(e) = put_back {
            tracing::warn!("could not unmark run {}: {e:#}", self.old_id);
        }
    }
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
    /// A driver pid is recorded but could not be shown dead: it may well be
    /// running, so "unknown" must not read as "gone".
    pub driver_unproven: bool,
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
/// [`Liveness::Unknown`] releases only when no driver pid was ever recorded
/// (nothing that could still be running); a recorded pid that could not be
/// shown dead is refused like a live one.
pub fn decide(superseded: bool, holder: &Holder) -> Decision {
    if !superseded {
        return Decision::NotOurs;
    }
    let mut why = Vec::new();
    if holder.liveness == Liveness::Live {
        why.push("that run is being worked on right now".to_owned());
    } else if holder.driver_unproven {
        why.push("its driver process could not be shown to be gone".to_owned());
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
    let liveness = state.liveness(claimed);
    Ok(Holder {
        run: state.id.clone(),
        status: state.status,
        liveness,
        driver_unproven: liveness == Liveness::Unknown && state.driver_pid.is_some(),
        // Spelled out: `status.showUntrackedFiles=no` in a user's config would
        // otherwise hide new files from `git status` and let them be deleted.
        dirty: !git::git(path, &["status", "--porcelain", "--untracked-files=normal"])
            .await?
            .trim()
            .is_empty(),
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
/// was released (see [`Released`]), or `None` when nothing needed (or was allowed) to happen.
///
/// An `Err` is a refusal or a failed removal; the message is what the
/// operator reads. Nothing is left half-done: the old run's record is written
/// before the removal and rolled back if git refuses it.
pub async fn release(
    repo: &Path,
    branch: &str,
    new_run: &str,
    takeover: &Takeover,
) -> Result<Option<Released>> {
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
        return Err(Refused(format!(
            "branch `{branch}` is checked out in {}, which is not a worktree of an earlier \
             attempt at this task (a run of another task, or one made by hand), so it was \
             not touched. Remove that worktree (`git worktree remove`) if it is not needed, \
             and try again.",
            path.display()
        ))
        .into());
    };

    let holder = inspect(repo, branch, &path, &state, &takeover.home).await?;
    match decide(true, &holder) {
        Decision::NotOurs => return Ok(None),
        Decision::Refuse(why) => {
            return Err(Refused(format!(
                "branch `{branch}` is checked out in {}: {why}. Commit or discard the work \
                 there and remove that worktree (`git worktree remove`), or say the run may be \
                 discarded, and try again.",
                path.display()
            ))
            .into());
        }
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

    // Re-read right before the removal for the driver and the tip, and let git
    // itself refuse a dirty worktree: the removal is *not* `--force`, so an
    // edit made after the first look is refused rather than thrown away.
    // Ignored files such as `target/` go with the worktree by design.
    // Against the record as it is on disk now, not the copy read earlier: a
    // resume that started in between has saved its driver there. (The resume
    // side refuses a released run too - see `Runner::execute_graph`.)
    let again = match RunState::load_under(&old_id, &takeover.home) {
        Ok(fresh) => inspect(repo, branch, &path, &fresh, &takeover.home).await,
        Err(e) => Err(e),
    };
    let safe = matches!(&again, Ok(h) if decide(true, h) == Decision::Release);
    let removed = safe
        && git::worktree_remove_clean(repo, &path)
            .await
            .unwrap_or(false);
    if !removed || path.exists() {
        state = RunState::load_under(&old_id, &takeover.home)?;
        state.candidates[index].folded = false;
        state.released_to = None;
        state.released_branches.retain(|b| b != branch);
        state.events.pop();
        state.save_under(&takeover.home)?;
        return Err(Refused(format!(
            "branch `{branch}` is checked out in {} by run {}, and releasing that worktree \
             failed or found it changed (git refuses to remove a worktree with uncommitted \
             changes); it was left as it was",
            path.display(),
            crate::run::short_of(&old_id)
        ))
        .into());
    }
    Ok(Some(Released {
        old_id,
        index,
        path,
        home: takeover.home.clone(),
        tip: holder.tip,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder() -> Holder {
        Holder {
            run: "20260901-000000-f82f".to_owned(),
            status: RunStatus::Gating,
            liveness: Liveness::Unknown,
            driver_unproven: false,
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
    fn a_driver_that_could_not_be_shown_dead_is_refused() {
        let unproven = Holder {
            driver_unproven: true,
            ..holder()
        };
        let Decision::Refuse(why) = decide(true, &unproven) else {
            panic!("an unproven driver must be refused");
        };
        assert!(why.contains("driver"), "{why}");
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
