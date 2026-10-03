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
//! A branch held by a run of *another* task is released on the same terms when
//! magi can prove the worktree is its own: the path is a candidate worktree
//! one run's record names, laid under that record's own worktree root, and the
//! run is not being driven. Anything else - a path magi did not make, a live
//! run, uncommitted files - is left alone and the refusal names which.
//!
//! [`decide`] is pure; [`release`] is the only function here that touches git
//! or disk. A hand-run `magi review` has no task, so it carries a [`Takeover`]
//! with no earlier attempts and can only release a worktree of a run magi
//! itself recorded.

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
    /// The owner's answer to an earlier divergence question about the branch,
    /// applied once the earlier worktree is released (git refuses to move a
    /// branch that is checked out).
    pub choice: Option<crate::reconcile::Choice>,
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
    /// What was released and why it was safe, for the new run's events.
    pub audit: String,
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
    /// The first few changed paths, for the refusal text.
    pub dirty_files: Vec<String>,
    /// The recorded driver pid, for the refusal text.
    pub driver_pid: Option<u32>,
    /// HEAD of the worktree.
    pub head: String,
    /// The branch tip in the repository.
    pub tip: String,
}

/// Whose worktree holds the branch, as far as magi can prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A candidate worktree of an earlier attempt at the same task.
    EarlierAttempt,
    /// A candidate worktree some run's record names, laid under that run's
    /// own worktree root: magi made it.
    MagiRun,
    /// Not provably magi's; the text says why.
    Foreign(String),
}

/// The verdict of [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Safe to release the worktree.
    Release,
    /// Not safe to release; the text says exactly why, for the operator.
    Refuse(String),
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Whether `holder`'s worktree may be released to a new run.
///
/// [`Liveness::Unknown`] releases only when no driver pid was ever recorded
/// (nothing that could still be running); a recorded pid that could not be
/// shown dead is refused like a live one. A run of another task that is in
/// `landing` (possibly waiting on the owner's approval) is refused as well.
pub fn decide(owner: &Owner, holder: &Holder) -> Decision {
    if let Owner::Foreign(why) = owner {
        return Decision::Refuse(why.clone());
    }
    let mut why = Vec::new();
    if holder.liveness == Liveness::Live {
        why.push(format!(
            "run {} is being worked on right now{}",
            crate::run::short_of(&holder.run),
            holder
                .driver_pid
                .map(|p| format!(" (driver pid {p})"))
                .unwrap_or_default()
        ));
    } else if holder.driver_unproven {
        why.push("its driver process could not be shown to be gone".to_owned());
    }
    if *owner == Owner::MagiRun && holder.status == RunStatus::Landing {
        why.push("that run is in `landing`, possibly waiting on an approval".to_owned());
    }
    if holder.dirty {
        let files = holder.dirty_files.join(", ");
        why.push(format!("its worktree has uncommitted changes ({files})"));
    }
    if holder.head != holder.tip {
        why.push("its HEAD is not at the branch tip".to_owned());
    }
    if why.is_empty() {
        return Decision::Release;
    }
    let kind = match owner {
        Owner::EarlierAttempt => "an earlier attempt at this task",
        _ => "a magi run of another task",
    };
    Decision::Refuse(format!(
        "run {} (status `{}`, worktree {}, HEAD {}, branch tip {}) is {kind} and still has the \
         branch checked out, so it was not released automatically: {}",
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
    // Spelled out: `status.showUntrackedFiles=no` in a user's config would
    // otherwise hide new files from `git status` and let them be deleted.
    let porcelain = git::git(path, &["status", "--porcelain", "--untracked-files=normal"]).await?;
    Ok(Holder {
        run: state.id.clone(),
        status: state.status,
        liveness,
        driver_unproven: liveness == Liveness::Unknown && state.driver_pid.is_some(),
        dirty: !porcelain.trim().is_empty(),
        dirty_files: porcelain
            .lines()
            .take(5)
            .map(|l| l.get(3..).unwrap_or(l).trim().to_owned())
            .collect(),
        driver_pid: state.driver_pid,
        head: git::rev_parse(path, "HEAD").await?,
        tip: git::rev_parse(repo, &format!("refs/heads/{branch}")).await?,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

fn foreign(branch: &str, path: &Path, why: &str) -> anyhow::Error {
    Refused(format!(
        "branch `{branch}` is checked out in {}, which magi will not remove by itself: {why}. \
         Remove that worktree (`git worktree remove`) if it is not needed, and try again.",
        path.display()
    ))
    .into()
}

/// The run and candidate whose recorded worktree is `path`, when magi
/// provably made it: the record names the path, the path sits directly in the
/// record's own worktree root (`<root>/<short>/<dir>`), and exactly one run
/// says so. `Err` is the reason a path under a root could not be proven.
fn find_magi_owner(
    path: &Path,
    home: &Path,
) -> std::result::Result<Option<(RunState, usize)>, String> {
    let Some(bay) = path.parent() else {
        return Ok(None);
    };
    let Some(bay_name) = bay.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    let mut found = Vec::new();
    for id in crate::run::list_ids_in(&home.join("runs")) {
        if crate::run::short_of(&id) != bay_name {
            continue;
        }
        let Ok(state) = RunState::load_under(&id, home) else {
            return Err(format!("run {bay_name}'s record could not be read"));
        };
        if !same_path(&state.worktree_root(), bay) {
            continue;
        }
        if let Some(i) = state
            .candidates
            .iter()
            .position(|c| !c.folded && same_path(&c.worktree, path))
        {
            found.push((state, i));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(format!("more than one run record claims it ({bay_name})")),
    }
}

/// Release the worktree holding `branch` to `new_run`, when an earlier attempt
/// at the same task, or a run magi itself recorded, holds it and it is safe. Returns the run whose worktree
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
    // The holder has to be an unfolded candidate worktree of a run on record.
    // One nobody recorded (made by hand, say) is not ours to touch.
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
            owner = Some((state, i, Owner::EarlierAttempt));
            break;
        }
    }
    if owner.is_none() {
        owner = match find_magi_owner(&path, &takeover.home) {
            Ok(found) => found.map(|(s, i)| (s, i, Owner::MagiRun)),
            Err(why) => return Err(foreign(branch, &path, &why)),
        };
    }
    let Some((mut state, index, kind)) = owner else {
        return Err(foreign(
            branch,
            &path,
            &format!(
                "{} is not a candidate worktree any magi run recorded (made by hand, or by \
                 something other than magi)",
                path.display()
            ),
        ));
    };

    let holder = inspect(repo, branch, &path, &state, &takeover.home).await?;
    if let Decision::Refuse(why) = decide(&kind, &holder) {
        return Err(Refused(format!(
            "branch `{branch}` is checked out in {}: {why}. Commit or discard the work \
             there and remove that worktree (`git worktree remove`), or say the run may be \
             discarded, and try again.",
            path.display()
        ))
        .into());
    }
    let audit = format!(
        "run {} (status `{}`, no driver, clean, HEAD {} = branch tip, branch `{branch}` kept)",
        crate::run::short_of(&holder.run),
        holder.status.as_str(),
        short(&holder.head)
    );

    let old_id = state.id.clone();
    state.candidates[index].folded = true;
    state.released_to = Some(new_run.to_owned());
    if !state.released_branches.iter().any(|b| b == branch) {
        state.released_branches.push(branch.to_owned());
    }
    state.event(
        "release",
        format!(
            "worktree of `{branch}` released to run {}: {audit}; this run can no longer be \
             resumed from here",
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
    let safe = matches!(&again, Ok(h) if decide(&kind, h) == Decision::Release);
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
        state.event(
            "release",
            format!(
                "release of `{branch}` to run {} was undone: {audit}",
                crate::run::short_of(new_run)
            ),
        );
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
        audit,
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
            dirty_files: Vec::new(),
            driver_pid: None,
            head: "a".repeat(40),
            tip: "a".repeat(40),
        }
    }

    #[test]
    fn a_clean_superseded_stale_run_is_released() {
        assert_eq!(decide(&Owner::EarlierAttempt, &holder()), Decision::Release);
        let dead = Holder {
            liveness: Liveness::Dead,
            ..holder()
        };
        assert_eq!(decide(&Owner::EarlierAttempt, &dead), Decision::Release);
    }

    #[test]
    fn a_foreign_worktree_is_refused_with_its_reason() {
        let owner = Owner::Foreign("it is a foreign path".to_owned());
        assert_eq!(
            decide(&owner, &holder()),
            Decision::Refuse("it is a foreign path".to_owned())
        );
    }

    #[test]
    fn a_landing_run_of_another_task_is_refused_but_an_earlier_attempt_is_not() {
        let landing = Holder {
            status: RunStatus::Landing,
            ..holder()
        };
        assert!(matches!(
            decide(&Owner::MagiRun, &landing),
            Decision::Refuse(_)
        ));
        assert_eq!(decide(&Owner::EarlierAttempt, &landing), Decision::Release);
    }

    #[test]
    fn a_dirty_worktree_is_refused_and_says_why() {
        let dirty = Holder {
            dirty: true,
            dirty_files: vec!["scratch.txt".to_owned()],
            ..holder()
        };
        let Decision::Refuse(why) = decide(&Owner::EarlierAttempt, &dirty) else {
            panic!("dirty must be refused");
        };
        assert!(
            why.contains("uncommitted") && why.contains("scratch.txt"),
            "{why}"
        );
        assert!(why.contains("f82f") && why.contains("gating") && why.contains("dirty"));
    }

    #[test]
    fn a_live_run_is_refused() {
        let live = Holder {
            liveness: Liveness::Live,
            driver_pid: Some(4242),
            ..holder()
        };
        let Decision::Refuse(why) = decide(&Owner::EarlierAttempt, &live) else {
            panic!("live must be refused");
        };
        assert!(
            why.contains("right now") && why.contains("f82f") && why.contains("4242"),
            "{why}"
        );
    }

    #[test]
    fn a_driver_that_could_not_be_shown_dead_is_refused() {
        let unproven = Holder {
            driver_unproven: true,
            ..holder()
        };
        let Decision::Refuse(why) = decide(&Owner::EarlierAttempt, &unproven) else {
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
        assert!(matches!(
            decide(&Owner::EarlierAttempt, &off),
            Decision::Refuse(_)
        ));
    }
}
