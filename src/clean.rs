//! The disk janitor: finished runs get their worktrees folded, worktrees whose
//! run record is already gone get reclaimed too, and the shared build cache is
//! pruned to its cap.
//!
//! A run's state being written by an older schema is not the same thing as it
//! being unreadable, and this module used to conflate the two: [`fold_due`]
//! treated any `run.json` its version check rejected exactly like one that
//! failed to parse at all, so a single schema bump silently stopped every
//! automatic fold in the fleet the moment it shipped, and did so with no
//! counter and no log line to say so. A record magi genuinely cannot parse —
//! missing fields, broken JSON, a schema newer than this build has ever heard
//! of — is still left alone here, still counted in
//! [`Housekeeping::unreadable`], and still only ever removed by an explicit
//! operator action (`magi fold`, or the equivalent phone route). One written
//! by a schema this build merely disagrees with the *meaning* of is not that:
//! as long as it still parses, folding proceeds regardless of the number in
//! its `schema` field.
//!
//! Everything policy-shaped — which statuses are foldable, how long a finished
//! run is left alone, whether the cache is over its limit — is a pure function
//! injected with numbers, so nothing here has to ask the operating system to
//! be testable. The only I/O is the removal itself.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::ask::Questions;
use crate::config::Disk;
use crate::run::{RunState, RunStatus, SCHEMA, short_of};

use crate::disk::{Prune, dir_size};

/// What one janitor pass did, for the caller's log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Housekeeping {
    /// Runs folded (worktrees dropped).
    pub folded: usize,
    /// Runs [`fold_due`] left alone because their `run.json` genuinely could
    /// not be read - missing, broken JSON, a schema this build has never
    /// heard of - as opposed to one merely written by a different schema
    /// number, which is folded like any other (see the module docs). This was
    /// defined but never incremented for a long stretch of this module's
    /// history, which is exactly how 90 of 93 runs sat unfolded on one
    /// operator's machine with nothing anywhere saying why: every one of them
    /// was misclassified as unreadable by a schema check that has since been
    /// narrowed to only the runs that actually are.
    pub unreadable: usize,
    /// Worktrees under the worktree bay reclaimed because no run record in
    /// `runs/` claims them anymore (see [`fold_orphaned_worktrees`]).
    pub orphaned_worktrees: usize,
    /// Files dropped from the shared cache.
    pub cache_files: usize,
    /// Bytes freed from the shared cache.
    pub cache_freed: u64,
    /// Open questions abandoned because the run that asked them has already
    /// settled where nothing is coming back to read an answer.
    pub questions_abandoned: usize,
    /// Runs found blocked on a merge magi never recorded — a pull request the
    /// operator merged by hand while `land::land` never got as far as
    /// opening one itself — and corrected automatically. See
    /// [`reconcile_external_merges`].
    pub external_merges_recorded: usize,
    /// Run records that still called their pull request open after it was
    /// merged or closed, and were rewritten (see
    /// [`crate::land::repair_stale_pr_states`]).
    pub stale_pr_states_repaired: usize,
}

/// Run the janitor: fold due runs, reclaim orphaned worktrees, prune stale
/// worktree registrations, then prune the cache if it is over its cap.
///
/// Every part is best-effort; a jammed cache lock or a run whose worktree
/// another borrower holds must not stop the rest. Errors are reported through
/// `tracing::warn` - this is housekeeping, and the daemon keeps serving
/// either way.
pub async fn housekeep(
    cfg: &crate::config::Config,
    home: &Path,
    worktrees_root: &Path,
    repo: &Path,
    now: Timestamp,
) -> Housekeeping {
    let mut out = Housekeeping::default();
    crate::retention::sweep(home, &cfg.disk, now).await;
    if cfg.disk.auto_fold {
        let runs = home.join("runs");
        match fold_due(&runs, home, worktrees_root, &cfg.disk, now).await {
            Ok((folded, unreadable)) => {
                out.folded = folded;
                out.unreadable = unreadable;
            }
            Err(e) => {
                tracing::warn!("housekeep: fold due runs: {e:#}");
                // Stable wording: the reason varies per pass, and a changed
                // message would relight the bell on every retry.
                crate::notices::raise_in(
                    home,
                    crate::notices::Notice::warn(
                        "housekeep:fold",
                        "Automatic cleanup of finished runs failed; disk usage may keep growing.",
                    ),
                );
            }
        }
        out.orphaned_worktrees =
            fold_orphaned_worktrees(&runs, worktrees_root, home, cfg.disk.fold_grace_secs, now)
                .await;
        // Best-effort in the same sense as everything else here: a repository
        // this janitor pass has nothing to do with (or none at all, in a unit
        // test) must not turn a `warn` into a reason to skip the rest.
        if let Err(e) = crate::git::worktree_prune(repo).await {
            tracing::warn!("housekeep: prune worktree registrations: {e:#}");
        }
        out.external_merges_recorded = reconcile_external_merges(&runs, home, &cfg.disk, now).await;
        // Bounded: a handful of forge lookups a pass keeps this cheap, and the
        // pass is idempotent, so a backlog drains over several passes.
        out.stale_pr_states_repaired = crate::land::repair_stale_pr_states(home, 5).await.0;
    }
    match prune_cache_if_over_limit(cfg, home) {
        Ok(Some(pruned)) => {
            out.cache_files = pruned.files;
            out.cache_freed = pruned.freed;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("housekeep: prune cache: {e:#}"),
    }
    match crate::build_target::prune_registered(home, &cfg.disk) {
        Ok(pruned) => {
            out.cache_files = out.cache_files.saturating_add(pruned.files);
            out.cache_freed = out.cache_freed.saturating_add(pruned.freed);
        }
        Err(e) => tracing::warn!("housekeep: prune isolated builds: {e:#}"),
    }
    // Unconditional, unlike the two passes above: this is not a disk policy
    // with a cap or an opt-out, it is closing a gap `graph::Runner` itself
    // cannot - a run that reached `Merged`/`Ready`/`Failed` before this
    // cleanup existed, or whose process died between saving that status and
    // abandoning the question it leaves behind (see `Runner::settle_questions`).
    // Left alone, that question sits `open` forever: the owner's badge,
    // banner and title all keep counting a decision nobody is left to read.
    out.questions_abandoned =
        abandon_settled_questions(&Questions::at(home.join("questions")), &home.join("runs"));
    out
}

/// Abandon every open question whose run has already settled into a status
/// nothing comes back from, worded with what the run became - the same
/// cleanup `graph::Runner::settle_questions` runs the moment `status` lands
/// there, for questions that missed it.
///
/// Scans questions rather than runs: the open list is normally short, and a
/// run that never asked anything costs nothing here. A run this cannot read,
/// deleted or written by a schema this build does not speak, is left alone
/// the same as everywhere else in this module; the question stays open
/// rather than guessed at.
pub fn abandon_settled_questions(store: &Questions, runs: &Path) -> usize {
    let waiting_on: BTreeSet<String> = store
        .list()
        .into_iter()
        .filter(|q| q.status.open())
        .map(|q| q.run)
        .collect();
    let mut abandoned = 0;
    for run in waiting_on {
        let Ok(meta) = read_meta(runs, &run) else {
            continue;
        };
        match store.settle_run(&run, meta.status) {
            Ok(n) => abandoned += n,
            Err(e) => tracing::warn!("housekeep: abandon questions for {run}: {e:#}"),
        }
    }
    abandoned
}

/// Fold every run that is finished, older than the grace period, and not being
/// worked on; return `(folded, unreadable)`.
///
/// A run whose `run.json` genuinely cannot be parsed — missing fields, broken
/// JSON, a schema newer than this build has ever heard of — is left exactly
/// as it is. Automatic housekeeping cannot tell a mid-write file from one that
/// will never parse again, and `<home>/runs/<id>/` is the evidence `magi
/// stats` and the deck read; when unsure whether it is safe to touch, the
/// janitor keeps rather than deletes (see the module docs). Discarding a
/// record this unreadable is an explicit operator action (`magi fold`, or the
/// equivalent phone route), never something that happens unattended. Every
/// such skip is counted in the returned `unreadable` and logged through
/// `tracing::warn` with the parse failure that caused it - silence here is
/// exactly the failure mode that let 90 of 93 runs sit unfolded with nothing
/// to show for it.
///
/// A run merely written by a *different* schema number is not unreadable: as
/// long as `run.json` still parses, it folds like any other terminal run (see
/// the module docs for why the two are different questions).
///
/// Runnable statuses and runs newer than the grace period are also left
/// alone; folding them would throw away work that is still the answer to
/// somebody's question. `Merged` runs forget their winner's worktree (the
/// merge already landed it); `Ready` and `Failed` runs keep it.
pub async fn fold_due(
    runs: &Path,
    home: &Path,
    _worktrees_root: &Path,
    disk: &Disk,
    now: Timestamp,
) -> Result<(usize, usize)> {
    let mut folded = 0usize;
    let mut unreadable = 0usize;
    for id in fold_ids(runs) {
        match fold_one(runs, home, disk, now, &id, false, &mut || {})
            .await
            .outcome
        {
            FoldOutcome::Folded | FoldOutcome::NothingLeft => folded += 1,
            FoldOutcome::Unreadable => unreadable += 1,
            _ => {}
        }
    }
    Ok((folded, unreadable))
}

/// Every run directory [`fold_due`] and [`fold_turn`] consider, oldest id first.
fn fold_ids(runs: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("run.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort_unstable();
    ids
}

/// What one run's turn through [`fold_one`] came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoldOutcome {
    /// `graph::fold_run_when` ran and removed something.
    Folded,
    /// `graph::fold_run_when` ran and found nothing left to remove.
    NothingLeft,
    /// The run is `settled`: a fold recorded that it found nothing left and
    /// the bay is confirmed absent, so no fold was started (only the busy-time
    /// turn asks for this).
    AlreadyFolded,
    /// A live claim, or a held execution/ownership/output lease, protects the run.
    Live,
    /// A resumable status: someone may still continue this run.
    Resumable,
    /// Finished less than `fold_grace_secs` ago.
    WithinGrace,
    /// Explicitly retained recovery source (`pr_recovery::source_retention_reason`).
    Retained,
    /// The proof that nothing is lost is incomplete: startup or candidate
    /// preservation unfinished, or the confirmed-merge source proof.
    ProofIncomplete,
    /// `run.json` could not be parsed, or became unreadable once the fold
    /// held the run's locks.
    Unreadable,
    /// The record was deleted while the fold waited for its locks.
    RecordGone,
    /// `graph::fold_run_when` failed (git, filesystem).
    Failed,
}

/// One run's [`FoldOutcome`], with the text of what stopped it where there is
/// any worth keeping (`ProofIncomplete`, `Failed`).
struct Judged {
    outcome: FoldOutcome,
    note: Option<String>,
}

impl From<FoldOutcome> for Judged {
    fn from(outcome: FoldOutcome) -> Self {
        Self {
            outcome,
            note: None,
        }
    }
}

fn refused(refusal: &crate::graph::FoldRefusal) -> Judged {
    use crate::graph::FoldRefusal;
    match refusal {
        FoldRefusal::Live(_) => FoldOutcome::Live.into(),
        FoldRefusal::Resumable => FoldOutcome::Resumable.into(),
        FoldRefusal::WithinGrace => FoldOutcome::WithinGrace.into(),
        FoldRefusal::Retained(_) => FoldOutcome::Retained.into(),
        FoldRefusal::ProofIncomplete(detail) => Judged {
            outcome: FoldOutcome::ProofIncomplete,
            note: Some(detail.clone()),
        },
        FoldRefusal::RecordGone => FoldOutcome::RecordGone.into(),
        FoldRefusal::Unreadable(_) => FoldOutcome::Unreadable.into(),
    }
}

/// May this record be folded now, and does its winner go too? The one
/// decision, shared by the idle janitor and the busy-time turn, and asked
/// twice: on the record read before any lock, and again by
/// `graph::fold_run_when` on the record it reloads under the run's owner lock
/// and execution lease. See [`fold_due`] for why every refusal is a skip and
/// not an error.
fn eligibility(
    state: &RunState,
    home: &Path,
    disk: &Disk,
    now: Timestamp,
) -> Result<bool, crate::graph::FoldRefusal> {
    use crate::graph::FoldRefusal;
    if crate::daemon::is_working_on(home, &state.id, now) {
        return Err(FoldRefusal::Live(
            "claimed by a live driver or worker".into(),
        ));
    }
    if state.status.resumable() {
        return Err(FoldRefusal::Resumable);
    }
    if !due(now, state.updated_at, disk.fold_grace_secs) {
        return Err(FoldRefusal::WithinGrace);
    }
    if state.startup.as_ref().is_some_and(|s| !s.complete) {
        return Err(FoldRefusal::ProofIncomplete("startup incomplete".into()));
    }
    if let Some(reason) = crate::pr_recovery::source_retention_reason(home, state) {
        tracing::info!(id = %state.id, %reason, "housekeep: source retained; fold refused");
        return Err(FoldRefusal::Retained(reason));
    }
    // The same predicate `graph::fold_run_when` refuses on before it takes any lock.
    if !state.candidate_preservation.is_empty() || !state.interrupted_implementations.is_empty() {
        return Err(FoldRefusal::ProofIncomplete(
            "candidate preservation incomplete; refusing cleanup".into(),
        ));
    }
    // `Merged`'s winner branch is safe to drop because it already landed
    // in the real target branch; `Superseded`'s winner branch is safe to
    // drop for a different reason - a later attempt at the same task is
    // what actually landed (or didn't), so *this* run's own winner is
    // never going to be merged by anyone. Every other terminal status
    // keeps the winner: `Ready`/`Blocked`/`Stalled`/`Failed`/`VerifiedNoop`
    // may still have a human's decision pending on that exact branch.
    // `AlreadyInBase` is the same: the change is on the base under other
    // commit ids, so the branch holds nothing anyone still needs.
    Ok(matches!(
        state.status,
        RunStatus::Merged | RunStatus::Superseded | RunStatus::AlreadyInBase
    ))
}

/// May the busy-time turn answer a run without starting a fold?
///
/// Only on a fact a fold itself recorded: `graph::fold_run_when` completed under
/// the run's locks, found nothing left to remove and saw the worktree bay gone
/// (`RunState::fold_settled_at`), *and* the bay is confirmed absent now -
/// `bay` is the answer of `Path::try_exists`, so a failing check and a bay that
/// exists (even an empty one, which a fold removes) both mean "judge normally and
/// invoke the fold". Nothing is inferred from candidate flags, directory listings
/// or Git: a stale registration or a leftover branch without a bay is not disk
/// and is left to the ordinary paths. A pending worktree-config release is work,
/// so it is not skipped either. This spares a run that was folded long ago the
/// fold budget every cycle; the idle pass never skips.
fn settled(state: &RunState, drop_winner: bool, bay: std::io::Result<bool>) -> bool {
    state.fold_settled_at.is_some()
        && !(state.enabled_worktree_config && drop_winner)
        && matches!(bay, Ok(false))
}

/// Judge one run and, when [`eligibility`] allows it, fold it.
///
/// Shared by the idle janitor ([`fold_due`]) and the bounded busy-time turn
/// ([`fold_turn`]); neither may decide differently, so the checks live here
/// and nowhere else. `skip_finished` (the turn only) answers a run that is
/// [`settled`] without starting a fold. `begin_fold` is called right
/// before `graph::fold_run_when` is entered, whatever it then returns, so the
/// caller's budget is charged for every invocation, not only for the ones that
/// removed something.
async fn fold_one(
    runs: &Path,
    home: &Path,
    disk: &Disk,
    now: Timestamp,
    id: &str,
    skip_finished: bool,
    begin_fold: &mut (dyn FnMut() + Send),
) -> Judged {
    if crate::daemon::is_working_on(home, id, now) {
        return FoldOutcome::Live.into();
    }
    let meta = match read_meta(runs, id) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::warn!("housekeep: run {id} unreadable, left alone: {e:#}");
            return FoldOutcome::Unreadable.into();
        }
    };
    if meta.status.resumable() {
        return FoldOutcome::Resumable.into();
    }
    if !due(now, meta.updated_at, disk.fold_grace_secs) {
        return FoldOutcome::WithinGrace.into();
    }
    // `read_meta` already proved the file parses; `read_state` asks for
    // the rest of the fields `graph::fold_run` needs (worktree paths,
    // candidates, tally). A schema mismatch alone does not fail this -
    // see the module docs - so reaching `Err` here means the JSON itself
    // is broken in a way `read_meta` did not exercise, which is rare but
    // not impossible (a body truncated between the two fields it reads
    // and the rest). That must not cost every other run its turn through
    // the caller's loop, so it is a skip, not a `?`.
    let mut state = match read_state(runs, id) {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!("housekeep: run {id} unreadable, left alone: {e:#}");
            return FoldOutcome::Unreadable.into();
        }
    };
    let decide = |state: &RunState| eligibility(state, home, disk, now);
    let drop_winner = match decide(&state) {
        Ok(drop_winner) => drop_winner,
        Err(refusal) => return refused(&refusal),
    };
    if state.schema != SCHEMA {
        tracing::info!(
            "housekeep: run {id} was written by schema {} (this build speaks {SCHEMA}); \
             folding it anyway",
            state.schema
        );
    }
    if skip_finished && settled(&state, drop_winner, state.worktree_root().try_exists()) {
        return FoldOutcome::AlreadyFolded.into();
    }
    // The window a handover can fall into: after the read above, before the
    // fold takes the run's locks. Tests run their handover here, deterministically.
    #[cfg(test)]
    if let Some(hook) = tests::AFTER_FIRST_READ.with(|hook| hook.borrow_mut().take()) {
        hook(home, id);
    }
    // One run's fold must not cost every later run its turn. A worktree
    // another borrower holds, a branch git refuses to delete, a repository
    // that has since moved: each is a reason this run cannot be folded
    // now, and none is a reason to stop the pass. Left unfolded, it is
    // simply due again next time; a `?` here stopped automatic folding
    // permanently at the first such run (finding R3-1-1 of run 51a3).
    begin_fold();
    match crate::graph::fold_run_when(&mut state, home, &decide).await {
        Ok(removed) if removed.is_empty() => FoldOutcome::NothingLeft.into(),
        Ok(_) => FoldOutcome::Folded.into(),
        Err(e) => match e.downcast_ref::<crate::graph::FoldRefusal>() {
            Some(refusal) => {
                tracing::info!("housekeep: fold {id}: {refusal}");
                refused(refusal)
            }
            None => {
                tracing::warn!("housekeep: fold {id}: {e:#}");
                Judged {
                    outcome: FoldOutcome::Failed,
                    note: Some(format!("{e:#}")),
                }
            }
        },
    }
}

/// How much one [`fold_turn`] may do. Whichever bound is reached first ends the
/// pass; the cycle state says where the next one continues.
#[derive(Debug, Clone, Copy)]
pub struct FoldBounds {
    /// Run records read and judged (cheap: one JSON each).
    pub inspect: usize,
    /// `graph::fold_run_when` invocations (expensive: git, locks, deleting
    /// build output), charged when the invocation begins, whatever it returns.
    pub fold: usize,
    /// Wall clock, inventory included, checked between runs only; a started
    /// fold always finishes.
    pub budget: std::time::Duration,
}

/// What the scheduler's busy-time turn may do per pass: judging at most 64
/// records (a 218-run history is four passes), starting at most four folds and
/// starting none after five minutes, whichever bound comes first.
///
/// The costs are not alike, and none is "milliseconds" in general. Judging a
/// record reads its `run.json` and asks for a live claim (`daemon.json`, the
/// worker store); one that is eligible is read in full and its decision
/// evaluated, which reads the queue for recovery-source retention. Those are
/// local file reads and are bounded by `inspect`, but their wall time depends
/// on the disk and on how loaded the machine is, which is why the time budget
/// also applies to them. A fold can take a minute (Git plus deleting
/// gigabytes of build output) and may wait out repository coordination, so
/// the fold bound, not the inspect bound, is what limits how long a pass holds
/// run ownership locks.
///
/// The inventory (listing `runs/`, sorting the ids) is not bounded by
/// `inspect`: it costs one directory listing plus one `is_file` per record on
/// every pass, i.e. O(history) syscalls. It is counted in the pass's elapsed
/// time and reported (`FoldTurn::records`, `FoldTurn::elapsed_ms`).
///
/// **The bound on revisiting.** A cycle covers the run ids that are not newer
/// than the persisted high-water mark, the newest id present when it began,
/// in id order, and then ends; the next pass begins a new cycle. With N records
/// at the start of a cycle, every one of them is judged within
/// `ceil(N / inspect)` passes plus the passes its fold invocations consume
/// (`ceil(F / fold)` for F invocations, and one more for each pass the time
/// budget cuts short, which itself began at least one fold). Ids start with the
/// local wall-clock time of creation, so runs created later sort after the mark
/// and wait for the next cycle: a growing history cannot postpone the
/// wraparound. Two things can still put a later run inside the range - one
/// created in the same second as the mark's, or after the clock stepped
/// backwards - and each such run adds at most its own judgement to the cycle;
/// there is no cohort list, so that is the whole bound on them. A run that
/// left its grace period after being judged is folded within the next cycle:
/// at most one cycle plus one cycle later.
pub const BUSY_FOLD_BOUNDS: FoldBounds = FoldBounds {
    inspect: 64,
    fold: 4,
    budget: std::time::Duration::from_secs(300),
};

/// Why a [`fold_turn`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FoldStop {
    /// Every run of the cycle's range was judged; the next pass starts a new cycle.
    EndOfCycle,
    /// The bound on run records judged was reached.
    InspectLimit,
    /// The bound on folds started was reached.
    FoldLimit,
    /// The wall-clock budget was spent (checked between runs).
    TimeBudget,
}

/// What stopped one run, for the receipt: the run, the outcome, and the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoldNote {
    /// The run id.
    pub run: String,
    /// `proof_incomplete` or `failed`.
    pub outcome: &'static str,
    /// Scrubbed and clipped to [`NOTE_CHARS`].
    pub text: String,
}

/// Notes kept per pass, and characters kept per note.
const MAX_NOTES: usize = 8;
const NOTE_CHARS: usize = 300;

/// What one [`fold_turn`] did, per outcome, for the service receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoldTurn {
    /// Run records in the history when the pass began (the inventory's size).
    pub records: usize,
    /// Run records judged this pass.
    pub inspected: usize,
    /// `graph::fold_run_when` invocations this pass (what `bounds.fold` bounds).
    pub invoked: usize,
    /// Folds that ran and removed something.
    pub folded: usize,
    /// Folds that ran and found nothing left to remove.
    pub nothing_left: usize,
    /// Runs the record shows to be folded already; no fold was started.
    pub already_folded: usize,
    /// Claimed by a live driver or worker, or a lease/lock of it is held.
    pub live: usize,
    /// A resumable status: someone may still continue the run.
    pub resumable: usize,
    /// Finished less than `fold_grace_secs` ago.
    pub within_grace: usize,
    /// Explicitly retained recovery sources.
    pub recovery_retained: usize,
    /// Startup, candidate preservation or the confirmed-merge source proof incomplete.
    pub proof_incomplete: usize,
    /// `run.json` could not be parsed, or became unreadable once the run's
    /// locks were held.
    pub unreadable: usize,
    /// The record was deleted while the fold waited for its locks.
    pub record_gone: usize,
    /// `graph::fold_run_when` failed (git, filesystem).
    pub failed: usize,
    /// Run id the next pass continues after; `None`: it starts a new cycle.
    pub cursor: Option<String>,
    /// Last run id of the cycle's range, fixed when the cycle began.
    pub high_water: Option<String>,
    /// Why this pass ended.
    pub stopped: FoldStop,
    /// Why a new cycle was started although one was saved, when it was
    /// (unreadable cursor file, high-water mark below the cursor, a cursor
    /// naming a run that no longer exists).
    pub cycle_restart: Option<String>,
    /// The cycle file could not be saved: the next pass repeats this slice.
    pub cursor_error: Option<String>,
    /// Wall clock of the pass, inventory included.
    pub elapsed_ms: u64,
    /// What stopped the first few `proof_incomplete` and `failed` runs.
    pub notes: Vec<FoldNote>,
}

impl FoldTurn {
    fn begin(records: usize) -> Self {
        Self {
            records,
            inspected: 0,
            invoked: 0,
            folded: 0,
            nothing_left: 0,
            already_folded: 0,
            live: 0,
            resumable: 0,
            within_grace: 0,
            recovery_retained: 0,
            proof_incomplete: 0,
            unreadable: 0,
            record_gone: 0,
            failed: 0,
            cursor: None,
            high_water: None,
            stopped: FoldStop::EndOfCycle,
            cycle_restart: None,
            cursor_error: None,
            elapsed_ms: 0,
            notes: Vec::new(),
        }
    }

    fn record(&mut self, id: &str, judged: Judged) {
        match judged.outcome {
            FoldOutcome::Folded => self.folded += 1,
            FoldOutcome::NothingLeft => self.nothing_left += 1,
            FoldOutcome::AlreadyFolded => self.already_folded += 1,
            FoldOutcome::Live => self.live += 1,
            FoldOutcome::Resumable => self.resumable += 1,
            FoldOutcome::WithinGrace => self.within_grace += 1,
            FoldOutcome::Retained => self.recovery_retained += 1,
            FoldOutcome::ProofIncomplete => self.proof_incomplete += 1,
            FoldOutcome::Unreadable => self.unreadable += 1,
            FoldOutcome::RecordGone => self.record_gone += 1,
            FoldOutcome::Failed => self.failed += 1,
        }
        let outcome = match judged.outcome {
            FoldOutcome::ProofIncomplete => "proof_incomplete",
            FoldOutcome::Failed => "failed",
            _ => return,
        };
        if let (Some(text), true) = (judged.note, self.notes.len() < MAX_NOTES) {
            let text = crate::scrub::scrub(&text, &crate::scrub::Identity::current());
            self.notes.push(FoldNote {
                run: id.to_owned(),
                outcome,
                text: text.chars().take(NOTE_CHARS).collect(),
            });
        }
    }
}

/// Where one cycle through the history stands, saved between passes.
///
/// `high_water` is the last run id that existed when the cycle began; the
/// cycle covers `(cursor, high_water]` and nothing created later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cycle {
    cursor: Option<String>,
    high_water: String,
}

/// What the cycle file held.
enum Saved {
    /// No cycle in progress: the next pass begins one.
    Absent,
    Cycle(Cycle),
    /// The file exists and cannot be used (read failure, bad JSON).
    Unreadable(String),
}

/// One bounded, resumable step through finished-run folding.
///
/// Continues the saved cycle (or begins one), judges each run with
/// [`fold_one`] - the same decision, grace, liveness and source-retention
/// checks as [`fold_due`] - and stops at the first bound in `bounds`. At least
/// one run is always judged, so a zero budget still makes progress. Returns
/// the cycle to save: `None` when the cycle's range is exhausted.
///
/// A saved cycle that cannot be trusted starts a fresh one, and the receipt
/// says why (`cycle_restart`): an unreadable file, a high-water mark below the
/// cursor, a cursor naming a run that no longer exists, or a range with
/// nothing left in it. Starting over only costs judging runs again.
async fn fold_turn_from(
    runs: &Path,
    home: &Path,
    disk: &Disk,
    now: Timestamp,
    saved: Saved,
    bounds: FoldBounds,
) -> (FoldTurn, Option<Cycle>) {
    let began = std::time::Instant::now();
    let ids = fold_ids(runs);
    let mut turn = FoldTurn::begin(ids.len());
    let mut cycle = match saved {
        Saved::Absent => None,
        Saved::Unreadable(why) => {
            turn.cycle_restart = Some(format!("cursor file unreadable: {why}"));
            None
        }
        Saved::Cycle(cycle) => {
            let restart = if !crate::run::is_run_id(&cycle.high_water)
                || cycle
                    .cursor
                    .as_deref()
                    .is_some_and(|cursor| !crate::run::is_run_id(cursor))
            {
                Some("cycle file names an id that is not a run id")
            } else if ids.last().is_none_or(|newest| cycle.high_water > *newest) {
                Some("high-water mark is newer than every run")
            } else {
                match &cycle.cursor {
                    Some(cursor) if *cursor > cycle.high_water => {
                        Some("high-water mark is below the cursor")
                    }
                    Some(cursor) if ids.binary_search(cursor).is_err() => {
                        Some("cursor names a run that no longer exists")
                    }
                    _ => None,
                }
            };
            match restart {
                Some(why) => {
                    turn.cycle_restart = Some(why.into());
                    None
                }
                None => Some(cycle),
            }
        }
    };
    let range = |cycle: &Cycle| {
        let start = cycle
            .cursor
            .as_deref()
            .map_or(0, |c| ids.partition_point(|id| id.as_str() <= c));
        let end = ids.partition_point(|id| id.as_str() <= cycle.high_water.as_str());
        (start, end)
    };
    if cycle.as_ref().is_some_and(|c| {
        let (start, end) = range(c);
        start >= end
    }) {
        turn.cycle_restart = Some("the saved cycle had nothing left in its range".into());
        cycle = None;
    }
    let Some(cycle) = cycle.or_else(|| {
        ids.last().map(|last| Cycle {
            cursor: None,
            high_water: last.clone(),
        })
    }) else {
        turn.elapsed_ms = began.elapsed().as_millis() as u64;
        return (turn, None);
    };
    turn.high_water = Some(cycle.high_water.clone());
    let (start, end) = range(&cycle);
    let mut begun = 0usize;
    let mut last: Option<&String> = None;
    let mut next = None;
    for id in &ids[start..end] {
        let stop = if turn.inspected == 0 {
            None
        } else if turn.inspected >= bounds.inspect {
            Some(FoldStop::InspectLimit)
        } else if begun >= bounds.fold {
            Some(FoldStop::FoldLimit)
        } else if began.elapsed() >= bounds.budget {
            Some(FoldStop::TimeBudget)
        } else {
            None
        };
        if let Some(stop) = stop {
            turn.stopped = stop;
            turn.cursor = last.cloned();
            next = Some(Cycle {
                cursor: last.cloned(),
                high_water: cycle.high_water.clone(),
            });
            break;
        }
        turn.inspected += 1;
        last = Some(id);
        let judged = fold_one(runs, home, disk, now, id, true, &mut || begun += 1).await;
        turn.record(id, judged);
    }
    turn.invoked = begun;
    turn.elapsed_ms = began.elapsed().as_millis() as u64;
    (turn, next)
}

/// Where [`fold_turn`] remembers its cycle: JSON `{cursor, high_water}`. Absent
/// means "no cycle in progress", an unusable file starts a new one and is
/// reported; either only costs judging runs again.
const FOLD_TURN_CURSOR_FILE: &str = "fold-turn-cursor";

fn read_cycle(home: &Path) -> Saved {
    match std::fs::read(home.join(FOLD_TURN_CURSOR_FILE)) {
        Ok(body) => match serde_json::from_slice(&body) {
            Ok(cycle) => Saved::Cycle(cycle),
            Err(e) => Saved::Unreadable(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::Absent,
        Err(e) => Saved::Unreadable(e.to_string()),
    }
}

fn write_cycle(home: &Path, cycle: Option<&Cycle>) -> Result<()> {
    let path = home.join(FOLD_TURN_CURSOR_FILE);
    match cycle {
        Some(cycle) => crate::fold_evidence::atomic_write(&path, &serde_json::to_vec(cycle)?),
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    }
}

/// [`fold_turn_from`] continuing from, and saving, the persisted cycle.
///
/// The cycle is saved after every pass, before the caller reports it, so a
/// restart resumes where this pass ended. A save failure is reported in
/// [`FoldTurn::cursor_error`] rather than hidden: the next pass would then
/// repeat the same slice, and the receipt must say so. Passes never overlap:
/// the scheduler admits one `fold` service worker at a time.
pub async fn fold_turn(
    runs: &Path,
    home: &Path,
    disk: &Disk,
    now: Timestamp,
    bounds: FoldBounds,
) -> FoldTurn {
    let (mut turn, next) = fold_turn_from(runs, home, disk, now, read_cycle(home), bounds).await;
    if let Err(e) = write_cycle(home, next.as_ref()) {
        tracing::warn!("housekeep: persist the fold turn's cycle: {e:#}");
        turn.cursor_error = Some(format!("{e:#}"));
    }
    turn
}

/// What one [`fetch_origins`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// Repos whose fetch succeeded.
    pub fetched: usize,
    /// Repos with no `origin` remote, skipped.
    pub no_origin: usize,
    /// Repos whose fetch failed or timed out.
    pub failed: usize,
}

/// Run `git fetch origin` in every checkout under `roots` (the set `magi
/// repos` lists), so `origin/main` stays current.
///
/// Fetch only: never a checkout, merge or fast-forward, so no HEAD, branch or
/// working tree moves. Best-effort like [`fold_due`]: a repo with no `origin`
/// is skipped silently, a failure or timeout is a warning, and the pass never
/// errors. `stop` is checked between repos.
pub async fn fetch_origins(
    roots: &[PathBuf],
    per_repo: std::time::Duration,
    stop: impl Fn() -> bool,
) -> FetchReport {
    let mut report = FetchReport::default();
    for repo in crate::repos::scan(roots) {
        if stop() {
            break;
        }
        match crate::git::git_raw(&repo.path, &["remote", "get-url", "origin"]).await {
            Ok(out) if out.ok() => {}
            _ => {
                report.no_origin += 1;
                continue;
            }
        }
        match crate::git::fetch_origin(&repo.path, per_repo).await {
            Ok(()) => report.fetched += 1,
            Err(e) => {
                tracing::warn!("fetch origin in {}: {e:#}", repo.name);
                report.failed += 1;
            }
        }
    }
    report
}

/// Is `updated` old enough, measured against `now`, that the run may fold?
///
/// Pure; the janitor compares against wallclock, tests inject both sides. The
/// comparison is strict, so a run exactly at the edge of its grace period is
/// left alone one more pass — the same convention as [`crate::disk::over_limit`].
pub fn due(now: Timestamp, updated: Timestamp, grace_secs: u64) -> bool {
    now.duration_since(updated) > SignedDuration::new(grace_secs as i64, 0)
}

/// The two fields the janitor decides on, read with a serde that tolerates
/// everything else about the run being unreadable.
#[derive(Deserialize)]
struct Meta {
    status: RunStatus,
    updated_at: Timestamp,
}

/// Read `status` and `updated_at` straight off the state file, asking for
/// nothing else. `Err` when the file is missing, not parseable, or a status in
/// a version this build does not speak - all of which mean "unreadable".
fn read_meta(runs: &Path, id: &str) -> Result<Meta> {
    let path = runs.join(id).join("run.json");
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let meta: Meta =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    Ok(meta)
}

/// Runs checked against GitHub in one janitor pass, at most.
///
/// A fleet with many stuck runs must not turn every idle tick into a burst of
/// `gh pr list` calls; a run left unchecked this pass is checked again next
/// pass, same as an unfolded due run is.
const MAX_EXTERNAL_MERGE_CHECKS_PER_PASS: usize = 5;

/// The fields [`reconcile_external_merges`] filters on before paying for a
/// full [`RunState`] parse or a `gh` round trip.
#[derive(Deserialize)]
struct ExternalMergeMeta {
    status: RunStatus,
    updated_at: Timestamp,
    #[serde(default)]
    merge: Option<crate::run::MergeOutcome>,
}

fn read_external_merge_meta(runs: &Path, id: &str) -> Result<ExternalMergeMeta> {
    let path = runs.join(id).join("run.json");
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let meta: ExternalMergeMeta =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    Ok(meta)
}

/// Is a run worth asking GitHub about?
///
/// Pure, so the filter itself is assertable without a fixture that writes
/// `run.json` files or spawns `gh`. Narrower than "blocked": `merge_recorded`
/// excludes a run `land::land` already finished landing (successfully or
/// not) - that path already carries its own record of what happened - and
/// `due` gives an operator who is about to fix a run by hand a grace window
/// before the janitor starts asking the same question about it.
pub fn eligible_for_external_merge_check(
    status: RunStatus,
    merge_recorded: bool,
    updated_at: Timestamp,
    now: Timestamp,
    grace_secs: u64,
) -> bool {
    status == RunStatus::Blocked && !merge_recorded && due(now, updated_at, grace_secs)
}

/// Rotate `ids` so a fixed per-pass cap does not always land on the same
/// prefix of the list, starting at `start` (already reduced modulo nothing -
/// callers pass whatever a persisted cursor last recorded, and this takes it
/// modulo `ids.len()` itself).
///
/// Pure: the offset is the caller's problem, not wall-clock time. An earlier
/// version derived `start` from `now.as_second() % ids.len()`, which looked
/// independent of any state to persist but is not actually independent of
/// how often the janitor runs - a poll interval and a pass's own duration
/// that alias onto the same handful of `now.as_second()` values (say, a
/// ten-second poll and a pass that finishes in under a second) revisit only
/// those same few residues forever and never advance into the rest of the
/// list at all. [`reconcile_external_merges`] instead persists a cursor
/// across passes ([`read_external_merge_cursor`] /
/// [`write_external_merge_cursor`]) and advances it by exactly how many ids
/// this pass actually looked at, which is the only quantity that is
/// guaranteed to move forward pass over pass regardless of timing.
fn rotate_from(ids: &[String], start: usize) -> Vec<String> {
    if ids.is_empty() {
        return Vec::new();
    }
    let start = start % ids.len();
    ids[start..]
        .iter()
        .chain(ids[..start].iter())
        .cloned()
        .collect()
}

/// Where [`reconcile_external_merges`] remembers how far it got, so a fixed
/// per-pass cap advances through a fleet's eligible runs pass over pass
/// instead of camping on whichever ones happen to sort first.
const EXTERNAL_MERGE_CURSOR_FILE: &str = "external-merge-cursor";

/// `0` for anything this cannot read as a plain number - a first run, a
/// corrupt or missing file, a build that has never written one - which is
/// exactly as good a starting point as any: the cursor's whole job is to
/// keep moving, not to encode any particular position as meaningful.
fn read_external_merge_cursor(home: &Path) -> usize {
    std::fs::read_to_string(home.join(EXTERNAL_MERGE_CURSOR_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Best-effort, like every other write in this module's sweep: a failure to
/// persist the cursor costs the fleet one pass's worth of forward progress
/// on this sweep, never the run whose housekeeping it is racing to keep up
/// with.
fn write_external_merge_cursor(home: &Path, cursor: usize) {
    if let Err(e) = std::fs::write(home.join(EXTERNAL_MERGE_CURSOR_FILE), cursor.to_string()) {
        tracing::warn!("housekeep: persist the external-merge sweep's cursor: {e:#}");
    }
}

/// Close the gap `magi fold --merged` exists for, without waiting for an
/// operator to notice and go find the pull request URL: a run that stopped
/// `Blocked` with no `merge` recorded may have been merged anyway, by hand,
/// on a pull request `land::land` never opened or never got to observe as
/// merged. `land::find_external_merge` asks GitHub about the run's own
/// winning branch, and only a pull request it can uniquely tie back to this
/// run is ever acted on - see that function's own doc for what "uniquely"
/// excludes.
///
/// A run this finds and fixes goes through
/// [`crate::land::correct_confirmed_external_merge`] - the same correction
/// `magi fold --merged` performs, minus the same-repo guard that command
/// needs and this loop doesn't (see that function's own doc: the URL here
/// was never operator-supplied, it came from asking `state.repo`'s own
/// remote) - then through [`crate::graph::fold_run`] so it stops holding
/// worktrees the moment it stops needing them. A run this cannot decide
/// about - `gh` unreachable, more than one candidate pull
/// request, nothing found at all - is left exactly as it is; only an error
/// asking GitHub raises a notice, since "nothing found" is the ordinary,
/// expected shape of a run that really is just blocked.
async fn reconcile_external_merges(runs: &Path, home: &Path, disk: &Disk, now: Timestamp) -> usize {
    reconcile_external_merges_with(runs, home, disk, now, |state| async move {
        crate::land::find_external_merge(&state).await
    })
    .await
}

async fn reconcile_external_merges_with<F, Fut>(
    runs: &Path,
    home: &Path,
    disk: &Disk,
    now: Timestamp,
    check: F,
) -> usize
where
    F: Fn(RunState) -> Fut,
    Fut: std::future::Future<Output = Result<Option<crate::land::ExternalMerge>>>,
{
    let mut ids: Vec<String> = std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("run.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort_unstable();

    // Every eligible id first, cheaply (a `Meta` read, no `gh`), then rotated
    // before the cap is applied. Capping the sorted order directly would
    // always land on the same lexicographic prefix - a fleet's oldest run ids
    // - so a handful of long-blocked runs that never turn out to be merged
    // would starve every id after them of a `gh` check, forever.
    let mut eligible: Vec<String> = Vec::new();
    for id in ids {
        if crate::daemon::is_working_on(home, &id, now) {
            continue;
        }
        let meta = match read_external_merge_meta(runs, &id) {
            Ok(meta) => meta,
            // Already counted as unreadable by `fold_due`'s own pass; not
            // worth a second warning for the same file.
            Err(_) => continue,
        };
        if eligible_for_external_merge_check(
            meta.status,
            meta.merge.is_some(),
            meta.updated_at,
            now,
            disk.fold_grace_secs,
        ) {
            eligible.push(id);
        }
    }

    if eligible.is_empty() {
        return 0;
    }
    let cursor = read_external_merge_cursor(home);
    let window: Vec<String> = rotate_from(&eligible, cursor)
        .into_iter()
        .take(MAX_EXTERNAL_MERGE_CHECKS_PER_PASS)
        .collect();
    // Advances by exactly how many ids this pass looked at, regardless of
    // what came of looking - a run that turned out not to be merged after
    // all must not be revisited before every other eligible run has had its
    // own turn.
    write_external_merge_cursor(home, (cursor + window.len()) % eligible.len());

    let mut reconciled = 0usize;
    for id in window {
        let mut state = match read_state(runs, &id) {
            Ok(state) => state,
            Err(_) => continue,
        };
        if let Ok(record) = serde_json::to_value(&state)
            && let Some(reason) =
                crate::external_merge_attention::historical_reason(home, &id, &record)
        {
            tracing::info!("housekeep: external merge for {id}: {reason}");
            continue;
        }
        match check(state.clone()).await {
            Ok(Some(found)) => {
                match crate::land::correct_confirmed_external_merge(&mut state, &found.url).await {
                    Ok(_) => {
                        if let Err(e) = crate::graph::fold_run(&mut state, true, home).await {
                            tracing::warn!(
                                "housekeep: fold {id} after recording its external merge: {e:#}"
                            );
                        }
                        reconciled += 1;
                    }
                    Err(e) => tracing::warn!(
                        "housekeep: record external merge {} for {id}: {e:#}",
                        found.url
                    ),
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("housekeep: check external merge for {id}: {e:#}");
                crate::notices::raise_in(
                    home,
                    crate::notices::Notice::warn(
                        &format!("merged-unrecorded:{id}"),
                        format!(
                            "Run {id} is blocked with no recorded merge, and checking GitHub \
                             for a merge failed; check by hand."
                        ),
                    )
                    .link(crate::notices::Link::Run { id: id.clone() }),
                );
            }
        }
    }
    reconciled
}

/// Read a whole run state from a runs directory, for folding only.
///
/// Deliberately more permissive than [`RunState::load`], which this does not
/// call: `load` backs `--resume` and every hand-driven command, where a
/// schema this build disagrees with the *meaning* of must refuse outright
/// rather than resume a review round or a tally against stale semantics
/// (`RunState::SCHEMA`'s own docs list what has changed meaning at each
/// bump). Folding recomputes nothing - it only reads worktree paths, branch
/// names and a tally winner off the struct to remove them - so an old
/// schema's values are exactly as good here as a current one's; every schema
/// bump so far has only ever added a field or a variant, never repurposed an
/// existing one, and serde already fills an added field's default when an
/// older record has nothing to say about it. What this cannot tolerate, and
/// what still surfaces as an `Err`, is `run.json` failing to parse at all.
fn read_state(runs: &Path, id: &str) -> Result<RunState> {
    RunState::read_unmigrated(&runs.join(id).join("run.json"))
}

/// Remove only verified clean Git worktrees and empty directory scaffolding.
/// Unknown files or unreadable Git state require recovery, never recursive deletion.
async fn remove_clean_bay(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut directories = Vec::new();
    let mut worktrees = Vec::new();
    while let Some(path) = pending.pop() {
        if path.join(".git").exists() {
            if !crate::git::is_clean(&path).await? {
                anyhow::bail!("uncommitted work in {}; recovery required", path.display());
            }
            worktrees.push(path);
        } else {
            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    anyhow::bail!(
                        "unknown worktree content in {}; recovery required",
                        path.display()
                    );
                }
                pending.push(entry.path());
            }
            directories.push(path);
        }
    }
    for path in worktrees {
        if !crate::git::worktree_remove_clean(&path, &path).await? {
            anyhow::bail!("clean worktree removal refused: {}", path.display());
        }
    }
    for path in directories.into_iter().rev() {
        std::fs::remove_dir(path)?;
    }
    Ok(())
}

/// A run's lock is held by somebody: a [`DeleteRefused`] that says so. Any other
/// failure of the lock call stays what it was.
fn busy_run(id: &str) -> impl Fn(anyhow::Error) -> anyhow::Error + '_ {
    move |e| {
        if crate::graph::held_by_live_owner(&e) {
            DeleteRefused(format!(
                "run {id} is being worked on or folded right now ({e:#}); try again later"
            ))
            .into()
        } else {
            e
        }
    }
}

/// Remove a run that cannot be read: its state directory under `runs` and its
/// worktree directory under `worktrees_root`.
///
/// Inspect worktrees before deleting any state or artifacts. Only clean registered
/// worktrees and empty directory scaffolding can be reclaimed; unknown or dirty
/// content remains available for recovery. Nonstandard worktree roots remain the
/// operator's responsibility when the state file cannot identify them.
pub async fn fold_unreadable(runs: &Path, worktrees_root: &Path, id: &str) -> Result<Vec<String>> {
    let resolved = resolve_id_path(runs, id)?;
    let mut removed = Vec::new();
    let run_dir = runs.join(&resolved);
    // The two locks every writer of the record takes, held until the directory
    // is gone: a fold in progress makes this a refusal, not a race.
    let _locks = if run_dir.is_dir()
        && let Some(home) = runs.parent()
    {
        let busy = busy_run(&resolved);
        Some((
            crate::pr_head::owner_lock(home, &resolved).map_err(&busy)?,
            crate::driver::maintenance(home, &resolved).map_err(&busy)?,
        ))
    } else {
        None
    };
    let wt = worktrees_root.join(short_of(&resolved));
    if wt.exists() {
        remove_clean_bay(&wt).await?;
        removed.push(wt.to_string_lossy().into_owned());
    }
    // Keep the recovery artifacts if any worktree could not be removed safely.
    if run_dir.exists() {
        std::fs::remove_dir_all(&run_dir)
            .with_context(|| format!("remove {}", run_dir.display()))?;
        removed.push(format!("runs/{resolved}"));
    }
    Ok(removed)
}

/// Reclaim worktrees under `worktrees_root` that no run record in `runs`
/// claims anymore, and return how many were removed.
///
/// [`fold_due`] only ever sees a worktree by walking `runs/` first, so a
/// worktree whose run record is already gone — `magi run rm`, or a record
/// deleted before its worktree — never enters that loop at all: nothing there
/// is looking for it. This walks the worktree bay directly instead, and
/// removes any `<short>` directory that no run id maps to.
///
/// Two things must never happen, and this checks both before ever touching a
/// directory:
///
/// - **A worktree bay is never the only kind of thing under `worktrees_root`,
///   and this must not assume it is.** A hand-placed scratch directory, or
///   anything else an operator or another tool left in the same bay, has the
///   same "no run claims it" shape as a genuine orphan but is not one -
///   [`looks_like_a_worktree_bay`] is the same tag shape [`crate::run::is_run_id`]
///   already requires of a real run's short id, and anything else is left
///   alone regardless of what else is true about it.
/// - **A worktree that was only just created might not have a `run.json` yet
///   for a reason that has nothing to do with being orphaned.** `Runner::start`
///   and `Runner::review` both create the worktree before the first
///   `RunState::save` lands, and that gap - several `git` subprocesses wide -
///   is invisible to [`crate::daemon::is_working_on_short`] whenever the run
///   is not being driven through this daemon's own `poll` loop at all (a
///   `magi review` invocation, for one). A directory whose own modification
///   time is within `grace_secs` of `now` is left alone on that basis alone,
///   the same margin [`fold_due`] gives a run before treating it as truly
///   finished - long enough that no realistic gap between a `worktree add`
///   and its `run.json` could ever be mistaken for one.
///
/// The one failure this must never cause is deleting the worktree of a run
/// that is genuinely in flight. [`crate::daemon::is_working_on_short`] is the
/// same liveness check [`fold_due`] trusts everywhere else in this module,
/// checked by short id because there is no full id to compare here; when it
/// cannot tell, this leaves the directory alone. Best-effort like the rest of
/// housekeeping: one directory git or the filesystem refuses to give up is a
/// `tracing::warn`, not a reason to abandon the rest of the pass.
pub async fn fold_orphaned_worktrees(
    runs: &Path,
    worktrees_root: &Path,
    home: &Path,
    grace_secs: u64,
    now: Timestamp,
) -> usize {
    let known: std::collections::HashSet<String> = std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| crate::run::is_run_id(name))
        .map(|id| short_of(&id).to_owned())
        .collect();

    let mut folded = 0usize;
    for entry in std::fs::read_dir(worktrees_root)
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.path().is_dir() {
            continue;
        }
        let short = entry.file_name().to_string_lossy().into_owned();
        if !looks_like_a_worktree_bay(&short) {
            continue;
        }
        if known.contains(&short) || crate::daemon::is_working_on_short(home, &short, now) {
            continue;
        }
        let wt = entry.path();
        if !stale_enough(&wt, grace_secs, now) {
            continue;
        }
        match remove_clean_bay(&wt).await {
            Ok(()) => folded += 1,
            Err(e) => tracing::warn!(
                "housekeep: remove orphaned worktree {}: {e:#}",
                wt.display()
            ),
        }
    }
    folded
}

/// Does `name` have the shape a run's own worktree bay is named with: the
/// same 4-character alphanumeric tag [`crate::run::is_run_id`] requires of a
/// full id's trailing block (see [`short_of`])?
///
/// Anything else under `worktrees_root` is not a bay this function reclaims
/// at all, claimed or not - answering "does a run claim this?" about a
/// directory that was never a run's worktree in the first place is exactly
/// the wrong question to ask before deleting it.
fn looks_like_a_worktree_bay(name: &str) -> bool {
    name.len() == 4 && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Is `dir`'s own modification time old enough, against `grace_secs` and
/// `now`, that its emptiness of a run record can be trusted rather than
/// caught mid-creation?
///
/// A directory this pass cannot stat at all - a race with its own removal, a
/// permission error - is treated as not yet stale: unreadable metadata is not
/// evidence of anything, and the janitor already keeps rather than deletes
/// whenever it cannot tell (see the module docs).
///
/// `grace_secs` is floored at [`MIN_ORPHAN_AGE_SECS`] regardless of what the
/// caller passes: `0` is a documented, legitimate value for
/// [`crate::config::Disk::fold_grace_secs`] (`due`'s own "always due" case),
/// because that grace answers a policy question the operator owns - how long
/// a *known, finished* run's worktree lingers before cleanup. Whether an
/// orphan worktree is actually a race with `Runner::review`'s `git worktree
/// add` landing before its `run.json` is not a policy question, and must not
/// collapse to zero just because the operator turned the other grace off -
/// that would defeat the very check meant to catch it.
fn stale_enough(dir: &Path, grace_secs: u64, now: Timestamp) -> bool {
    let Ok(modified) = std::fs::metadata(dir).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(ts) = Timestamp::try_from(modified) else {
        return false;
    };
    due(now, ts, grace_secs.max(MIN_ORPHAN_AGE_SECS))
}

/// The floor under [`stale_enough`]'s grace, independent of
/// [`crate::config::Disk::fold_grace_secs`].
///
/// Ample next to the race it guards: the gap between `Runner::start` or
/// `Runner::review` creating a worktree and the first `RunState::save`
/// landing is a handful of `git` subprocess calls, not minutes - but the
/// janitor cannot tell "still mid-setup" from "orphaned" by any other signal
/// for a run that never registers with `daemon::Status` at all (a `magi
/// review` invocation, for one), so this is generous on purpose rather than
/// tuned to the observed case.
const MIN_ORPHAN_AGE_SECS: u64 = 5 * 60;

/// Resolve an id or prefix against an explicit runs directory, exactly the way
/// [`crate::run::resolve_id`] does against the global home.
fn resolve_id_path(runs: &Path, prefix: &str) -> Result<String> {
    // Keyed on the directory, not on a readable state file: the record this
    // route exists to remove may be a lone `run.json.tmp` from a save that
    // ran out of disk, and that is precisely the one a human needs a way to
    // clear (see `crate::run::list_ids`).
    if runs.join(prefix).is_dir() && crate::run::is_run_id(prefix) {
        return Ok(prefix.to_owned());
    }
    let mut hits: Vec<String> = Vec::new();
    for e in std::fs::read_dir(runs).into_iter().flatten().flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let id = e.file_name().to_string_lossy().into_owned();
        if crate::run::is_run_id(&id) && (id.starts_with(prefix) || id.ends_with(prefix)) {
            hits.push(id);
        }
    }
    match hits.len() {
        1 => Ok(hits.into_iter().next().expect("exactly one hit")),
        0 => bail!("no run matches `{prefix}`"),
        _ => bail!(
            "`{prefix}` matches {} runs: {}",
            hits.len(),
            hits.join(", ")
        ),
    }
}

/// `magi fold`'s recovery path for a run whose worktrees are already gone —
/// so [`crate::graph::fold_run`] removed nothing — but whose `run.json` still
/// lists active seats nobody is left to answer for: no live daemon claims the
/// run, and every one of those seats has overrun its own timeout (see
/// [`RunState::active_all_overrun`]). Clearing them and failing the run is
/// what lets it be deleted afterward — [`RunState::ensure_can_delete`] only
/// ever checks whether a live daemon is working on the run and whether its
/// candidates are folded, not `status`, but a run stuck `implementing`
/// forever with an empty worktree still reads as unresolved everywhere else
/// (`magi show`, the deck, the phone) until this runs.
///
/// Returns `false` without changing anything when a live daemon still claims
/// the run, or when some active seat has not actually overrun its budget yet
/// — a run that is merely between waves must never be guessed at.
///
/// `state` is only the caller's first look. The record is written under the
/// run's owner lock and execution lease (the locks `graph::fold_run` and a
/// handover take), read again once they are held, and the conditions above are
/// judged on that record; `state` is replaced by it. Saving the caller's copy
/// would erase a handover (`released_to` / `released_branches`) that landed
/// after the fold released its own locks. A lock held by somebody else, or a
/// record that is gone, changes nothing and answers `false`.
pub fn clear_abandoned_active(state: &mut RunState, home: &Path, now: Timestamp) -> Result<bool> {
    let id = state.id.clone();
    if !home.join("runs").join(&id).join("run.json").try_exists()? {
        return Ok(false);
    }
    let Some(_owner) = uncontended(crate::pr_head::owner_lock(home, &id))? else {
        return Ok(false);
    };
    let Some(_driver) = uncontended(crate::driver::maintenance(home, &id))? else {
        return Ok(false);
    };
    match RunState::load_under(&id, home) {
        Ok(fresh) if fresh.id == id => *state = fresh,
        Err(e)
            if e.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            return Ok(false);
        }
        Ok(_) => return Ok(false),
        Err(e) => return Err(e),
    }
    if crate::daemon::is_working_on(home, &id, now) || !state.active_all_overrun(now) {
        return Ok(false);
    }
    state.abandon("fold");
    state.save_under(home)?;
    // The seat that asked is gone for good now - the same door
    // `graph::Runner::settle_questions` closes the moment `status` lands
    // somewhere non-resumable, see that method's own doc. Without this, an
    // open question the abandoned seat left behind would keep badging the
    // operator until the next daemon startup's `abandon_settled_questions`
    // pass happened to notice it, or forever if nothing is running `magi
    // serve` at all.
    if let Err(e) = Questions::at(home.join("questions")).settle_run(&state.id, state.status) {
        tracing::warn!("abandon questions for {}: {e:#}", state.id);
    }
    Ok(true)
}

/// `Some(lock)`, or `None` when somebody else holds it; any other failure of
/// the lock call stays an error.
fn uncontended<T>(locked: Result<T>) -> Result<Option<T>> {
    match locked {
        Err(e) if crate::graph::held_by_live_owner(&e) => Ok(None),
        other => other.map(Some),
    }
}

/// Why [`delete_run`] left a run record alone: somebody holds the run, it is
/// not finished with its worktrees, or it is gone. Callers answer a refusal
/// with a conflict and anything else with a failure.
#[derive(Debug)]
pub struct DeleteRefused(pub String);

impl std::fmt::Display for DeleteRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeleteRefused {}

/// Delete a run's directory (`magi run rm`, the phone's delete button) under
/// the protocol every writer of the run record follows.
///
/// A fold (`graph::fold_run*`) holds the run's owner lock and execution lease
/// from the moment it locks the record until it has saved it, and acts on what
/// it reads once they are held. A deletion that took no part in that let the
/// record vanish under a fold - which then recreated it from a stale copy - and
/// let the run's four-character worktree suffix be reserved by a new run while
/// the fold was still deleting that suffix's directories. So the deletion takes
/// both locks without waiting (a held lock means the run is busy, and the
/// answer is a refusal, not a wait), reads the record under them, and judges
/// [`RunState::ensure_can_delete`] on that read, not on the caller's.
pub fn delete_run(home: &Path, id: &str, now: Timestamp) -> Result<()> {
    let dir = home.join("runs").join(id);
    if !dir.join("run.json").try_exists()? {
        return Err(DeleteRefused(format!("run {id}: record gone")).into());
    }
    let busy = busy_run(id);
    let _owner = crate::pr_head::owner_lock(home, id).map_err(&busy)?;
    let _driver = crate::driver::maintenance(home, id).map_err(&busy)?;
    let state = RunState::load_under(id, home)?;
    state
        .ensure_can_delete(crate::daemon::is_working_on(home, id, now))
        .map_err(|e| DeleteRefused(format!("{e:#}")))?;
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("remove run directory {}", dir.display()))?;
    Ok(())
}

/// Delete files from the shared build cache until it fits its cap — but only
/// while nobody live is registered as using it. [`crate::cache::maintenance_prune`]
/// takes out the same lease a build would, so a prune can never race a
/// compile in flight (this run's own, another run's, or a human's `magi
/// review`) into deleting a file that build still needs. `Ok(None)` when the
/// cache is in use right now; the next pass catches it once the borrower
/// releases it, the same way a cap of `0` or a missing `CARGO_TARGET_DIR`
/// already meant "nothing to do this time" here.
pub fn prune_cache(home: &Path, cache: &Path, limit_bytes: u64) -> Result<Option<Prune>> {
    crate::cache::maintenance_prune(home, cache, limit_bytes)
}

/// [`prune_cache`], but resolving the operator's opt-out and missing
/// `CARGO_TARGET_DIR` first — the same two checks [`housekeep`]'s idle pass
/// makes before ever measuring the cache, factored out so
/// [`crate::daemon`]'s between-runs check (see the module's own doc for why
/// congestion can make "idle" arrive too rarely to matter) makes them
/// identically rather than growing its own copy that could drift. `Ok(None)`
/// covers a cap of `0` (see the module docs on `cache_limit_bytes`), a config
/// that renders no `CARGO_TARGET_DIR` to aggregate at all, and a cache
/// currently in use (see [`prune_cache`]).
pub fn prune_cache_if_over_limit(
    cfg: &crate::config::Config,
    home: &Path,
) -> Result<Option<Prune>> {
    if cfg.disk.cache_limit_bytes == 0 {
        return Ok(None);
    }
    let Some(cache) = cfg.cache_dir() else {
        return Ok(None);
    };
    let limit = crate::disk::free_bytes(&cache)
        .ok()
        .map_or(cfg.disk.cache_limit_bytes, |free| {
            crate::disk::pressure_limit(
                dir_size(&cache),
                cfg.disk.cache_limit_bytes,
                cfg.disk.min_free_bytes,
                free,
            )
        });
    prune_cache(home, &cache, limit)
}

/// The cache's path, size and cap, for `magi cache show` and the health view.
/// `None` when the config declares no `CARGO_TARGET_DIR` to aggregate.
///
/// A cap of `0` means the operator opted out of pruning; the size is then
/// reported but never acted on.
pub fn cache_report(cfg: &crate::config::Config) -> Option<(PathBuf, u64, u64)> {
    let cache = cfg.cache_dir()?;
    Some((
        cache.clone(),
        cache_size(&cache),
        cfg.disk.cache_limit_bytes,
    ))
}

/// Size in bytes of the shared build cache.
pub fn cache_size(cache: &Path) -> u64 {
    dir_size(cache)
}

#[cfg(test)]
pub(crate) mod tests {

    async fn sh(cwd: &Path, args: &[&str]) {
        let out = crate::git::git_raw(cwd, args).await.expect("spawn git");
        assert!(out.ok(), "git {args:?}: {}", out.stderr);
    }

    async fn commit(cwd: &Path, file: &str, body: &str) {
        std::fs::write(cwd.join(file), body).unwrap();
        sh(cwd, &["add", file]).await;
        sh(
            cwd,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                body,
            ],
        )
        .await;
    }

    const LONG: std::time::Duration = std::time::Duration::from_secs(30);

    #[tokio::test]
    async fn fetch_advances_origin_main_and_leaves_the_checkout_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let bare = t.join("remote.git");
        sh(
            t,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        )
        .await;
        let dir = t.join("root/h/o/r");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        sh(
            t,
            &["clone", "-q", bare.to_str().unwrap(), dir.to_str().unwrap()],
        )
        .await;
        sh(&dir, &["checkout", "-q", "-b", "main"]).await;
        commit(&dir, "a.txt", "one").await;
        sh(&dir, &["push", "-q", "origin", "main"]).await;
        sh(&dir, &["checkout", "-q", "--detach"]).await;
        std::fs::write(dir.join("a.txt"), "dirty").unwrap();

        let other = t.join("other");
        sh(
            t,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        )
        .await;
        commit(&other, "b.txt", "two").await;
        sh(&other, &["push", "-q", "origin", "HEAD:main"]).await;

        let rev = |d: PathBuf, r: &'static str| async move {
            crate::git::git(&d, &["rev-parse", r]).await.unwrap()
        };
        let head = rev(dir.clone(), "HEAD").await;
        let before = rev(dir.clone(), "origin/main").await;
        let status = crate::git::git(&dir, &["status", "--porcelain"])
            .await
            .unwrap();

        let r = fetch_origins(&[t.join("root")], LONG, || false).await;
        assert_eq!(
            r,
            FetchReport {
                fetched: 1,
                no_origin: 0,
                failed: 0
            }
        );

        assert_ne!(rev(dir.clone(), "origin/main").await, before);
        assert_eq!(rev(dir.clone(), "HEAD").await, head);
        assert_eq!(
            crate::git::git(&dir, &["status", "--porcelain"])
                .await
                .unwrap(),
            status
        );
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "dirty");
    }

    #[tokio::test]
    async fn fetch_never_writes_a_local_branch_whatever_the_configured_refspec() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let bare = t.join("remote.git");
        sh(
            t,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        )
        .await;
        let dir = t.join("root/h/o/r");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        sh(
            t,
            &["clone", "-q", bare.to_str().unwrap(), dir.to_str().unwrap()],
        )
        .await;
        sh(&dir, &["checkout", "-q", "-b", "main"]).await;
        commit(&dir, "a.txt", "one").await;
        sh(&dir, &["push", "-q", "origin", "main"]).await;
        sh(&dir, &["checkout", "-q", "--detach"]).await;
        // A hostile mapping onto the local branch, and no origin/* at all.
        sh(
            &dir,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/main:refs/heads/main",
            ],
        )
        .await;
        let _ = crate::git::git_raw(&dir, &["update-ref", "-d", "refs/remotes/origin/main"]).await;

        let other = t.join("other");
        sh(
            t,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        )
        .await;
        commit(&other, "b.txt", "two").await;
        sh(&other, &["push", "-q", "origin", "HEAD:main"]).await;
        let remote_tip = crate::git::git(&other, &["rev-parse", "HEAD"])
            .await
            .unwrap();
        let local_main = crate::git::git(&dir, &["rev-parse", "refs/heads/main"])
            .await
            .unwrap();

        let r = fetch_origins(&[t.join("root")], LONG, || false).await;
        assert_eq!(r.fetched, 1);
        assert_eq!(
            crate::git::git(&dir, &["rev-parse", "refs/remotes/origin/main"])
                .await
                .unwrap(),
            remote_tip
        );
        assert_eq!(
            crate::git::git(&dir, &["rev-parse", "refs/heads/main"])
                .await
                .unwrap(),
            local_main
        );
    }

    #[tokio::test]
    async fn fetch_skips_no_origin_and_survives_an_unreachable_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let root = t.join("root");
        let bare = t.join("remote.git");
        sh(t, &["init", "-q", "--bare", bare.to_str().unwrap()]).await;

        let none = root.join("h/o/none");
        let dead = root.join("h/o/dead");
        let good = root.join("h/o/good");
        for d in [&none, &dead, &good] {
            std::fs::create_dir_all(d).unwrap();
            sh(d, &["init", "-q"]).await;
        }
        sh(
            &dead,
            &[
                "remote",
                "add",
                "origin",
                t.join("missing").to_str().unwrap(),
            ],
        )
        .await;
        sh(&good, &["remote", "add", "origin", bare.to_str().unwrap()]).await;

        let r = fetch_origins(&[root], LONG, || false).await;
        assert_eq!(
            r,
            FetchReport {
                fetched: 1,
                no_origin: 1,
                failed: 1
            }
        );
    }
    use super::*;
    use crate::config::Disk;
    use std::fs;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("rfc3339")
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().expect("runtime").block_on(f)
    }

    #[test]
    fn a_run_is_due_after_its_grace_and_not_before() {
        let now = ts("2026-09-05T00:00:00Z");
        let grace = 600;
        let old = now - SignedDuration::new(601, 0);
        let fresh = now - SignedDuration::new(599, 0);
        assert!(due(now, old, grace));
        assert!(!due(now, fresh, grace));
        // Exactly at the edge: not yet due.
        let edge = now - SignedDuration::new(600, 0);
        assert!(!due(now, edge, grace));
        // A zero grace folds everything, ever.
        assert!(due(now, old, 0));
    }

    #[test]
    fn rotate_from_moves_the_starting_point_as_the_cursor_advances() {
        let ids: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // A cursor of 0 starts at the front, same as an unrotated list.
        assert_eq!(rotate_from(&ids, 0), ids);

        // A cursor of 2 starts two ids further along, with the skipped
        // front wrapping to the tail rather than being dropped.
        let at_2 = rotate_from(&ids, 2);
        assert_eq!(at_2, vec!["c", "d", "e", "a", "b"]);
        for id in &ids {
            assert!(at_2.contains(id));
        }

        // A cursor past the list's own length wraps rather than panicking -
        // the caller never has to reduce it modulo anything itself.
        assert_eq!(rotate_from(&ids, 7), rotate_from(&ids, 2));

        // An empty list has no start point to compute and must not panic.
        assert_eq!(rotate_from(&[], 0), Vec::<String>::new());
    }

    /// A fleet with more eligible runs than one pass can check must not park
    /// the same handful at the front of the window forever: advancing the
    /// cursor by exactly how many ids one pass actually looked at - the
    /// arithmetic [`reconcile_external_merges`] itself does - walks every id
    /// to the front within one full rotation, with no reliance on how often
    /// or how regularly passes happen to run.
    #[test]
    fn advancing_the_cursor_by_the_window_size_gives_every_id_a_turn() {
        let ids: Vec<String> = (0..12).map(|n| format!("run-{n}")).collect();
        let cap = 5usize;
        let mut cursor = 0usize;
        let mut ever_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..ids.len() {
            let window: Vec<String> = rotate_from(&ids, cursor).into_iter().take(cap).collect();
            ever_seen.extend(window.iter().cloned());
            cursor = (cursor + window.len()) % ids.len();
        }
        assert_eq!(
            ever_seen.len(),
            ids.len(),
            "every id must be checked at least once across a full rotation, whatever \
             the cadence between passes"
        );
    }

    #[test]
    fn the_external_merge_cursor_round_trips_through_a_files_absence() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Nothing written yet: starts at 0, not an error.
        assert_eq!(read_external_merge_cursor(home), 0);

        write_external_merge_cursor(home, 7);
        assert_eq!(read_external_merge_cursor(home), 7);

        // Anything unreadable as a plain number falls back to 0 rather than
        // wedging the sweep on a corrupt file.
        std::fs::write(home.join(EXTERNAL_MERGE_CURSOR_FILE), "not a number").unwrap();
        assert_eq!(read_external_merge_cursor(home), 0);
    }

    #[test]
    fn a_run_qualifies_for_an_external_merge_check_only_when_blocked_unmerged_and_due() {
        let now = ts("2026-09-05T00:00:00Z");
        let grace = 600;
        let old = now - SignedDuration::new(601, 0);
        let fresh = now - SignedDuration::new(599, 0);

        assert!(
            eligible_for_external_merge_check(RunStatus::Blocked, false, old, now, grace),
            "blocked, unmerged, and past its grace period is exactly the run this exists for"
        );
        assert!(
            !eligible_for_external_merge_check(RunStatus::Blocked, false, fresh, now, grace),
            "an operator mid-fix deserves the same grace window `fold_due` gives before \
             the janitor starts asking GitHub about it"
        );
        assert!(
            !eligible_for_external_merge_check(RunStatus::Blocked, true, old, now, grace),
            "a run `land::land` already recorded a merge outcome for has its own answer \
             already; this check is only for a run with nothing recorded at all"
        );
        assert!(
            !eligible_for_external_merge_check(RunStatus::Stalled, false, old, now, grace),
            "stalled is not blocked - it means the tally never reached quorum, which a \
             pull request cannot fix"
        );
        assert!(
            !eligible_for_external_merge_check(RunStatus::Ready, false, old, now, grace),
            "ready has nothing to correct - it was never landed by design"
        );
        assert!(
            !eligible_for_external_merge_check(RunStatus::Superseded, false, old, now, grace),
            "a superseded run's task already has its answer from a later attempt; \
             there is nothing left for GitHub to confirm here"
        );
    }

    /// A blocked run with a winner in an isolated repository directory.
    /// Consumer tests inject forge results without invoking a real CLI.
    fn write_blocked_run(runs: &Path, id: &str, updated_at: Timestamp) {
        use crate::run::{Candidate, Tally};
        use std::collections::BTreeMap;

        let mut state = RunState::new(
            runs.parent().unwrap().to_path_buf(),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            String::new(),
            crate::config::Config::default(),
        );
        state.id = id.to_owned();
        state.status = RunStatus::Blocked;
        state.updated_at = updated_at;
        state.candidates.push(Candidate {
            index: 0,
            label: 'A',
            agent: "agent".to_owned(),
            branch: format!("magi/{id}/A"),
            worktree: runs.parent().unwrap().join("candidate"),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        });
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 1,
            present: 1,
            quorum: 1,
            met_quorum: true,
            uncontested: None,
        });
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(
            runs.join(id).join("run.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
    }

    /// No run here can be confirmed merged - one has a forge error,
    /// one has not sat blocked long enough, and one is
    /// claimed by a live daemon - so the sweep must leave every one of them
    /// exactly as it found them and never panic on the failures in between.
    #[tokio::test]
    async fn reconcile_external_merges_leaves_ineligible_and_unconfirmable_runs_alone() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let home = dir.path().to_path_buf();
        std::fs::create_dir_all(&runs).unwrap();

        let now = ts("2026-09-05T00:00:00Z");
        let disk = Disk {
            fold_grace_secs: 600,
            ..Disk::default()
        };

        let due_id = "20260905-000000-blkd";
        write_blocked_run(&runs, due_id, ts("2026-08-01T00:00:00Z"));

        let fresh_id = "20260905-000000-fres";
        write_blocked_run(&runs, fresh_id, now);

        let live_id = "20260905-000000-live";
        write_blocked_run(&runs, live_id, ts("2026-08-01T00:00:00Z"));
        let mut status = crate::daemon::Status::new();
        status.current = vec![crate::daemon::Current {
            worker: None,
            task: "20260905-000000-task".to_owned(),
            run: live_id.to_owned(),
        }];
        status.updated_at = now;
        crate::daemon::write_status_to(&home.join("daemon.json"), &status).unwrap();

        let reconciled = reconcile_external_merges_with(&runs, &home, &disk, now, |_| async {
            anyhow::bail!("injected forge authentication failure")
        })
        .await;
        assert_eq!(reconciled, 0, "a forge failure can never confirm a merge");
        for id in [due_id, fresh_id, live_id] {
            assert_eq!(
                read_meta(&runs, id).unwrap().status,
                RunStatus::Blocked,
                "{id} must be left exactly as it was found"
            );
        }
        let notices = crate::notices::Notices::at(home.join("notifications")).list();
        let due_notice = notices
            .iter()
            .find(|n| n.key == format!("merged-unrecorded:{due_id}"))
            .expect("notice for due_id must be raised");
        assert_eq!(
            due_notice.link,
            Some(crate::notices::Link::Run {
                id: due_id.to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn historical_external_merge_check_preserves_unknown_history() {
        let home = tempfile::tempdir().unwrap();
        let runs = home.path().join("runs");
        let id = "legacy";
        write_blocked_run(&runs, id, ts("2026-08-01T00:00:00Z"));
        let path = runs.join(id).join("run.json");
        let mut state = read_state(&runs, id).unwrap();
        state.repo = home.path().join("removed");
        state.candidates[0].folded = true;
        let original = serde_json::to_vec(&state).unwrap();
        std::fs::write(&path, &original).unwrap();
        let count = reconcile_external_merges_with(
            &runs,
            home.path(),
            &Disk::default(),
            ts("2026-11-14T00:00:00Z"),
            |_| async { panic!("historical unavailable checkout must not invoke the forge") },
        )
        .await;
        assert_eq!(count, 0);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(
            crate::notices::Notices::at(home.path().join("notifications"))
                .all()
                .is_empty()
        );

        // The same folded run is actionable when a current task owns it.
        let queue = crate::queue::Queue::at(home.path().join("queue"));
        let mut task = crate::queue::Task::new(
            "current".into(),
            "current".into(),
            state.repo.clone(),
            crate::queue::Source::Human,
        );
        task.runs.push(id.into());
        queue.put(&mut task).unwrap();
        reconcile_external_merges_with(
            &runs,
            home.path(),
            &Disk::default(),
            ts("2026-11-14T00:00:00Z"),
            |_| async { anyhow::bail!("current checkout unavailable") },
        )
        .await;
        assert_eq!(
            crate::notices::Notices::at(home.path().join("notifications"))
                .list()
                .len(),
            1
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// The bug this exists to pin: a first version rotated on
    /// `now.as_second() % len`, which is fully determined by wall-clock time
    /// and nothing else. Two passes seconds apart - as they would be when a
    /// short poll interval and a fast pass alias onto the same handful of
    /// `now.as_second()` residues - landed on the *same* starting point and
    /// never covered a fleet bigger than the per-pass cap, however many
    /// times housekeeping ran. Persisting a cursor makes forward progress a
    /// function of how many ids got looked at, not of when the clock reads
    /// this pass happened to run.
    #[tokio::test]
    async fn reconcile_external_merges_covers_a_larger_fleet_across_repeated_passes_at_one_instant()
    {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let home = dir.path().to_path_buf();
        std::fs::create_dir_all(&runs).unwrap();

        let now = ts("2026-09-05T00:00:00Z");
        let disk = Disk {
            fold_grace_secs: 600,
            ..Disk::default()
        };
        let old = ts("2026-08-01T00:00:00Z");

        let ids: Vec<String> = (0..8).map(|n| format!("20260905-000000-r{n:03}")).collect();
        for id in &ids {
            write_blocked_run(&runs, id, old);
        }

        let notices = crate::notices::Notices::at(home.join("notifications"));

        // Two passes at the exact same `now`, exactly what a fast pass on a
        // short poll interval looks like. Each of the 8 injected forge-failure
        // runs raises its own notice the first time it is looked at, so the
        // count of distinct notices is a direct readout of how many distinct
        // ids have been checked so far.
        reconcile_external_merges_with(&runs, &home, &disk, now, |_| async {
            anyhow::bail!("injected forge authentication failure")
        })
        .await;
        let after_first = notices.list().len();
        assert_eq!(
            after_first, MAX_EXTERNAL_MERGE_CHECKS_PER_PASS,
            "the first pass checks exactly one cap's worth"
        );

        reconcile_external_merges_with(&runs, &home, &disk, now, |_| async {
            anyhow::bail!("injected forge authentication failure")
        })
        .await;
        let after_second = notices.list().len();
        assert_eq!(
            after_second,
            ids.len(),
            "a second pass at the same instant must still reach every id the \
             first pass had no room for, not repeat the same cap's worth"
        );
    }

    #[test]
    fn the_meta_reader_is_tolerant_of_everything_except_the_deciders() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let id = "20260905-000000-abcd";
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(
            runs.join(id).join("run.json"),
            r#"{"schema": 99, "id": "20260905-000000-abcd", "updated_at": "2026-09-05T00:00:00Z", "status": "ready", "junk_from_another_build": [1, 2, 3]}"#,
        )
        .unwrap();
        let meta = read_meta(&runs, id).expect("readable");
        assert_eq!(meta.status, RunStatus::Ready);
        assert_eq!(meta.updated_at, ts("2026-09-05T00:00:00Z"));
        assert!(read_meta(&runs, "nope").is_err(), "missing file unreadable");
        std::fs::write(runs.join(id).join("run.json"), "not json at all").unwrap();
        assert!(read_meta(&runs, id).is_err(), "garbage unreadable");
    }

    #[test]
    fn fold_unreadable_preserves_unknown_content_and_recovery_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let id = "20260905-000000-abcd";
        std::fs::create_dir_all(runs.join(id).join("artifacts")).unwrap();
        std::fs::write(runs.join(id).join("run.json"), "garbage").unwrap();
        std::fs::create_dir_all(wt.join("abcd")).unwrap();
        std::fs::write(wt.join("abcd/leftover"), "owned edits").unwrap();
        assert!(block_on(fold_unreadable(&runs, &wt, id)).is_err());
        assert!(runs.join(id).join("artifacts").exists());
        assert_eq!(
            std::fs::read_to_string(wt.join("abcd/leftover")).unwrap(),
            "owned edits"
        );
    }

    #[test]
    fn fold_unreadable_releases_run_dir_and_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let id = "20260905-000000-abcd";
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(runs.join(id).join("run.json"), "garbage").unwrap();
        std::fs::create_dir_all(wt.join("abcd")).unwrap();

        let removed = block_on(fold_unreadable(&runs, &wt, id)).expect("fold");
        assert_eq!(removed.len(), 2);
        assert!(!runs.join(id).exists(), "run dir gone");
        assert!(!wt.join("abcd").exists(), "worktrees gone");

        // A prefix resolves like `run::resolve_id` does.
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(runs.join(id).join("run.json"), "garbage").unwrap();
        std::fs::create_dir_all(wt.join("abcd")).unwrap();
        let removed = block_on(fold_unreadable(&runs, &wt, "20260905")).expect("by prefix");
        assert_eq!(removed.len(), 2);
        // Once gone, `id` cannot be resolved at all - same as `run::resolve_id`
        // on an id nothing on disk matches - so a repeat pass errors rather
        // than silently reporting nothing removed.
        assert!(
            block_on(fold_unreadable(&runs, &wt, id)).is_err(),
            "a run already gone cannot be resolved again"
        );
    }

    #[test]
    fn prune_cache_sheds_the_oldest_generation_until_it_fits() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Same size, different age: only the age decides, and the newest
        // generation - the one the next build reuses - is what survives.
        fs::write(dir.path().join("old"), b"xx").unwrap();
        fs::write(dir.path().join("new"), b"yy").unwrap();
        touch(&dir.path().join("old"), 1_000_000);
        touch(&dir.path().join("new"), 2_000_000);

        let out = prune_cache(home.path(), dir.path(), 2)
            .expect("prune")
            .expect("the cache is free");
        assert_eq!(out.files, 1, "one deletion is enough to reach the cap");
        assert_eq!(out.remaining, 2);
        assert!(!dir.path().join("old").exists(), "the older file went");
        assert!(dir.path().join("new").exists(), "the newer one stayed");

        // A whole generation shares one timestamp tick, so the tie has to be
        // decided too: largest first, which reaches the cap in the fewest
        // deletions. Left to `read_dir` and an unstable sort this deleted
        // both files on Linux and one on Windows.
        let tied = tempfile::tempdir().unwrap();
        fs::write(tied.path().join("big"), b"xxxx").unwrap();
        fs::write(tied.path().join("small"), b"yy").unwrap();
        touch(&tied.path().join("big"), 1_000_000);
        touch(&tied.path().join("small"), 1_000_000);
        let out = prune_cache(home.path(), tied.path(), 2)
            .expect("prune")
            .expect("the cache is free");
        assert_eq!(out.files, 1, "the big one alone gets under the cap");
        assert_eq!(out.remaining, 2);
        assert!(tied.path().join("small").exists());
    }

    #[test]
    fn prune_cache_if_over_limit_resolves_the_opt_outs_before_ever_measuring() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big"), vec![0u8; 10]).unwrap();

        let mut cfg = crate::config::Config::default();
        cfg.verify.gate = vec![format!(
            "CARGO_TARGET_DIR={} cargo make check",
            dir.path().display()
        )];

        // A cap of `0` is the operator's opt-out: never measured, never
        // pruned, regardless of what is actually on disk.
        cfg.disk.cache_limit_bytes = 0;
        assert_eq!(
            prune_cache_if_over_limit(&cfg, home.path()).unwrap(),
            None,
            "a zero cap must not even look at the directory"
        );
        assert!(dir.path().join("big").exists());

        // No `CARGO_TARGET_DIR` in either verify command: nothing to
        // aggregate, so there is nothing to prune either.
        let mut no_cache = crate::config::Config::default();
        no_cache.disk.cache_limit_bytes = 1;
        assert_eq!(
            prune_cache_if_over_limit(&no_cache, home.path()).unwrap(),
            None
        );

        // Over the cap and configured: pruned exactly like `prune_cache`
        // itself would.
        cfg.disk.cache_limit_bytes = 1;
        let pruned = prune_cache_if_over_limit(&cfg, home.path())
            .unwrap()
            .expect("a real cache dir over its cap prunes");
        assert_eq!(pruned.files, 1);
        assert!(!dir.path().join("big").exists());
    }

    /// Pin a file's mtime, so a test asserts the policy and not the runner's
    /// timestamp granularity.
    fn touch(path: &Path, secs: u64) {
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        ))
        .unwrap();
    }

    /// The disk-full casualty: a run whose first save left `run.json.tmp` and
    /// nothing else. It has to be clearable, or the record is permanent.
    #[test]
    fn fold_unreadable_clears_a_run_whose_state_never_landed() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let id = "20260904-014540-88c0";
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(runs.join(id).join("run.json.tmp"), b"").unwrap();

        let removed = block_on(fold_unreadable(&runs, &wt, id)).expect("fold by id");
        assert_eq!(removed, vec![format!("runs/{id}")]);
        assert!(!runs.join(id).exists(), "record gone");

        // And by prefix, the way the deck and the phone address a run.
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(runs.join(id).join("run.json.tmp"), b"").unwrap();
        assert!(
            block_on(fold_unreadable(&runs, &wt, "88c0")).is_ok(),
            "by prefix"
        );

        // A directory under `runs` that is not a run is never a fold target.
        std::fs::create_dir_all(runs.join("scratch")).unwrap();
        assert!(
            block_on(fold_unreadable(&runs, &wt, "scratch")).is_err(),
            "a stray directory is not a run"
        );
    }

    #[test]
    fn fold_due_folds_terminal_runs_of_any_schema_but_leaves_genuinely_unreadable_ones() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        let disk = Disk::default();
        let now = ts("2026-09-05T00:00:00Z");

        // 1. Runnable (judging): never folded, however old.
        let judging = "20260801-000000-0001";
        write_meta(&runs, judging, "judging", "2026-08-01T00:00:00Z");

        // 2. Finished but fresh: grace not elapsed. Within the default 6h
        //    grace of `now`, so `fold_due` must stop at the freshness check
        //    and never even reach `read_state` - `write_meta`'s minimal JSON
        //    would fail that full parse anyway, and this case exists to
        //    prove freshness is why the run survives, not an accident of the
        //    fixture being unparseable as a whole `RunState`.
        let ready_fresh = "20260904-220000-0002";
        write_meta(&runs, ready_fresh, "ready", "2026-09-04T22:00:00Z");

        // 3. Genuinely unreadable: broken JSON, not merely an unfamiliar
        //    schema number. Left alone and counted - this is the one case
        //    automatic housekeeping must never touch (see `fold_due`'s docs);
        //    discarding it is an explicit operator action, not something a
        //    background pass does.
        let garbage = "20260901-000000-0004";
        std::fs::create_dir_all(runs.join(garbage)).unwrap();
        std::fs::write(runs.join(garbage).join("run.json"), "not json").unwrap();
        std::fs::create_dir_all(wt.join("0004")).unwrap();

        // 4. Finished, well past grace, current schema: the ordinary case
        //    `fold_due` has always acted on.
        let due_ready = due_run(&runs, &wt, "20260801-000000-ffff", SCHEMA);

        // 5. Finished, well past grace, but written by a schema number this
        //    build no longer matches - the defect this task exists to fix.
        //    It still parses cleanly, so only the version number differs, and
        //    that alone must not block folding.
        let due_old_schema = due_run(&runs, &wt, "20260801-000000-eeee", SCHEMA - 1);

        let (folded, unreadable) =
            block_on(fold_due(&runs, &home, &wt, &disk, now)).expect("fold_due");
        assert_eq!(
            folded, 2,
            "both due, parseable runs fold regardless of their schema number"
        );
        assert_eq!(
            unreadable, 1,
            "only the run with broken JSON counts as unreadable"
        );
        assert!(runs.join(judging).exists(), "runnable never folded");
        assert!(runs.join(ready_fresh).exists(), "fresh never folded");
        assert!(runs.join(garbage).exists(), "unreadable record kept");
        assert!(wt.join("0004").exists(), "unreadable worktree kept");
        assert!(
            runs.join(&due_ready).exists(),
            "folding drops worktrees, not the record"
        );
        assert!(
            runs.join(&due_old_schema).exists(),
            "an old-schema record survives its fold exactly like a current one"
        );
        // `graph::fold_run` saves the state it just folded back to disk. That
        // write must land under this test's own `runs` - the argument it
        // passed to `fold_due`, not the process-global `run::home` - or a
        // fold that landed somewhere else entirely would still be counted
        // above as one of the two `folded` runs.
        for id in [&due_ready, &due_old_schema] {
            let saved = read_meta(&runs, id).expect("folded run still parses");
            assert_ne!(
                saved.updated_at,
                ts("2026-08-01T00:00:00Z"),
                "fold_run must have saved the updated state back through the \
                 `runs` directory this test passed to fold_due"
            );
        }
    }

    /// `[disk] auto_fold = false` must leave the janitor's fold-and-reclaim
    /// passes completely inert - a due run's worktree and record both
    /// survive exactly as if `housekeep` had never run at all. Cache pruning
    /// is a separate opt-out (`cache_limit_bytes`) and stays disabled here
    /// too, so this test is only ever about `auto_fold`.
    #[tokio::test]
    async fn housekeep_leaves_everything_alone_when_auto_fold_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        crate::run::set_home(dir.path().to_path_buf());

        let due_id = due_run(&runs, &wt, "20260801-000000-abcd", SCHEMA);
        std::fs::create_dir_all(wt.join("orphan").join("cand-A")).unwrap();

        let mut cfg = crate::config::Config::default();
        cfg.disk.auto_fold = false;
        cfg.disk.cache_limit_bytes = 0;

        let out = housekeep(&cfg, &home, &wt, &dir.path().join("repo"), Timestamp::now()).await;

        assert_eq!(out.folded, 0);
        assert_eq!(out.unreadable, 0);
        assert_eq!(out.orphaned_worktrees, 0);
        assert!(
            runs.join(&due_id).exists(),
            "a due run's record survives untouched"
        );
        assert!(
            wt.join("orphan").exists(),
            "an orphaned worktree survives untouched: the reclaim pass never ran"
        );
    }

    #[test]
    fn fold_orphaned_worktrees_removes_only_worktrees_no_run_claims_and_none_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();

        // A run record exists for this one: its worktree is claimed, not
        // orphaned, however old the record.
        write_meta(
            &runs,
            "20260801-000000-aaaa",
            "ready",
            "2026-08-01T00:00:00Z",
        );
        std::fs::create_dir_all(wt.join("aaaa").join("cand-A")).unwrap();

        // No run record at all, and nobody is working on it: this is the
        // leftover `fold_due` can never see, because it only ever walks
        // `runs/`.
        std::fs::create_dir_all(wt.join("bbbb").join("cand-A")).unwrap();

        // No run record either, but a live daemon status names a run with
        // this short id - the save-timing gap between the daemon claiming a
        // task and `RunState::new` writing its first `run.json`. Must survive
        // untouched.
        std::fs::create_dir_all(wt.join("cccc")).unwrap();

        // Not shaped like a run's short id at all - a scratch directory an
        // operator or another tool left in the same bay - so it is never a
        // reclaim target regardless of what runs claim it or not.
        std::fs::create_dir_all(wt.join("scratch")).unwrap();

        // `now` pushed comfortably past `MIN_ORPHAN_AGE_SECS`, so a zero
        // grace - the same "always due" escape hatch `due` itself documents
        // - still reclaims once a worktree is genuinely old, without faking
        // an mtime: real directory creation just above is already in the
        // past relative to this `now`, by design rather than by timing.
        let now = Timestamp::now() + SignedDuration::new((MIN_ORPHAN_AGE_SECS + 1) as i64, 0);
        let mut status = crate::daemon::Status::new();
        status.current = vec![crate::daemon::Current {
            worker: None,
            task: "20260905-000000-t111".to_owned(),
            run: "20260905-000000-cccc".to_owned(),
        }];
        status.updated_at = now;
        crate::daemon::write_status_to(&home.join("daemon.json"), &status).unwrap();

        let folded = block_on(fold_orphaned_worktrees(&runs, &wt, &home, 0, now));
        assert_eq!(
            folded, 1,
            "only the truly orphaned, idle, bay-shaped worktree is removed"
        );
        assert!(wt.join("aaaa").exists(), "claimed by a run record");
        assert!(!wt.join("bbbb").exists(), "orphaned and idle: reclaimed");
        assert!(wt.join("cccc").exists(), "a run in flight is never touched");
        assert!(
            wt.join("scratch").exists(),
            "not shaped like a worktree bay, so never a reclaim target"
        );
    }

    /// The gap this closes: `Runner::review` (`magi review`) creates the
    /// worktree with `git worktree add` before `RunState::save` ever writes a
    /// `run.json`, and that path never runs through the daemon's own `poll`
    /// loop at all, so `daemon::Status` never names it either. Without a
    /// grace window, a janitor pass landing in that gap would read the
    /// worktree as an orphan nothing is waiting on and delete a review still
    /// being set up.
    #[test]
    fn fold_orphaned_worktrees_leaves_a_freshly_created_bay_alone() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();

        std::fs::create_dir_all(wt.join("dddd").join("under-review")).unwrap();

        let now = Timestamp::now();
        let folded = block_on(fold_orphaned_worktrees(&runs, &wt, &home, 6 * 60 * 60, now));
        assert_eq!(
            folded, 0,
            "too fresh to tell apart from a run still being set up"
        );
        assert!(wt.join("dddd").exists());
    }

    /// A fresh open question on `run`, stored and handed back for assertions.
    fn open_question(store: &Questions, run: &str) -> crate::ask::Question {
        let mut q = crate::ask::Question::new(
            run.to_owned(),
            "implement".to_owned(),
            "impl-A".to_owned(),
            "Which storage backend should the cache use?".to_owned(),
            String::new(),
            vec!["SQLite".to_owned(), "Redis".to_owned()],
        );
        store.put(&mut q).unwrap();
        q
    }

    /// The exact ghost the phone showed: a run that already finished, with a
    /// question its dead seat asked still sitting `open` because it reached
    /// that status before `graph::Runner::settle_questions` existed (or
    /// missed it in the crash window `daemon::reclaim_orphaned_running`
    /// covers). This sweep is the second door to the same fact.
    #[test]
    fn a_finished_runs_open_question_is_swept_up() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let store = Questions::at(dir.path().join("questions"));

        let failed = "20260908-205802-c9eb";
        write_meta(&runs, failed, "failed", "2026-09-08T20:58:02Z");
        let failed_q = open_question(&store, failed);

        let merged = "20260908-205501-ca67";
        write_meta(&runs, merged, "merged", "2026-09-08T20:55:01Z");
        let merged_q = open_question(&store, merged);

        let n = abandon_settled_questions(&store, &runs);
        assert_eq!(n, 2, "both dead runs' questions are swept in one pass");

        for (id, run) in [(&failed_q.id, failed), (&merged_q.id, merged)] {
            let back = store.get(id).unwrap();
            assert!(!back.status.open(), "{run} is done; nobody reads an answer");
            assert!(back.detail.contains(run), "{}", back.detail);
        }
    }

    #[test]
    fn a_still_alive_runs_open_question_survives_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let store = Questions::at(dir.path().join("questions"));

        // `Blocked` and `Stalled` are `RunStatus::resumable`: the run can
        // still be picked back up, so its question may yet get a real
        // answer. A run still mid-competition is even more obviously alive.
        for (id, status) in [
            ("20260908-000000-b10c", "blocked"),
            ("20260908-000000-5ta1", "stalled"),
            ("20260908-000000-jud6", "judging"),
        ] {
            write_meta(&runs, id, status, "2026-09-08T00:00:00Z");
            let q = open_question(&store, id);

            let n = abandon_settled_questions(&store, &runs);
            assert_eq!(n, 0, "{status} run is not done; nothing to sweep");
            assert!(
                store.get(&q.id).unwrap().status.open(),
                "{status} run's question must still be waiting"
            );
        }
    }

    #[test]
    fn the_sweep_leaves_an_answered_question_and_an_unreadable_run_alone() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let store = Questions::at(dir.path().join("questions"));

        // Already decided: a sweep must never revisit it, whatever the run
        // that asked went on to become.
        let done = "20260908-000000-answ";
        write_meta(&runs, done, "failed", "2026-09-08T00:00:00Z");
        let mut answered = open_question(&store, done);
        answered
            .answer(crate::ask::Answer::Choice("SQLite".to_owned()))
            .unwrap();
        store.put(&mut answered).unwrap();

        // No `run.json` at all for this one - deleted, or never landed.
        let gone = "20260908-000000-gone";
        let orphan = open_question(&store, gone);

        assert_eq!(abandon_settled_questions(&store, &runs), 0);
        assert_eq!(
            store.get(&answered.id).unwrap().status,
            crate::ask::QuestionStatus::Answered,
            "a real answer is never overwritten by a sweep"
        );
        assert!(
            store.get(&orphan.id).unwrap().status.open(),
            "a run this sweep cannot read is left exactly as it was, not guessed at"
        );
    }

    /// A grace of `0` is a legitimate, documented value for the operator's
    /// own `Disk::fold_grace_secs` - `due`'s "always due" case - but the
    /// freshness check this guards is not that policy, and must not collapse
    /// to it: a `0` handed straight through would reclaim a worktree the
    /// instant it exists, exactly the race `fold_orphaned_worktrees_leaves_a_
    /// freshly_created_bay_alone` exists to rule out, just with the operator
    /// having turned the other grace off instead of leaving it at its
    /// default.
    #[test]
    fn fold_orphaned_worktrees_floors_a_zero_grace_at_the_race_safe_minimum() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();

        std::fs::create_dir_all(wt.join("eeee").join("under-review")).unwrap();

        // Too fresh, even with the grace argument at zero.
        let now = Timestamp::now();
        let folded = block_on(fold_orphaned_worktrees(&runs, &wt, &home, 0, now));
        assert_eq!(
            folded, 0,
            "a zero grace must not defeat the race-safety floor"
        );
        assert!(wt.join("eeee").exists());

        // Once genuinely past the floor, a zero grace reclaims it - the
        // floor is a minimum, not a replacement policy that never fires.
        let later = now + SignedDuration::new((MIN_ORPHAN_AGE_SECS + 1) as i64, 0);
        let folded = block_on(fold_orphaned_worktrees(&runs, &wt, &home, 0, later));
        assert_eq!(folded, 1, "old enough now, regardless of the zero grace");
        assert!(!wt.join("eeee").exists());
    }

    #[test]
    fn clear_abandoned_active_only_acts_once_dead_and_overrun() {
        let dir = tempfile::tempdir().unwrap();
        // Harmless if another test in this binary already pinned the global
        // home first (see `run::set_home`'s own doc): this test only checks
        // the in-memory mutation `clear_abandoned_active` makes, never a
        // write that landed under this exact directory.
        crate::run::set_home(dir.path().to_path_buf());
        let home = dir.path().to_path_buf();
        let now = ts("2026-09-14T12:00:00Z");
        let overrun_seat = || crate::run::ActiveSeat {
            node: "implement".to_owned(),
            started_at: now - SignedDuration::new(21_000, 0),
            timeout_secs: 3_600,
            attempt: 0,
            task: None,
            command: None,
            index: None,
            total: None,
        };

        let mut state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "fixture".to_owned(),
            crate::config::Config::default(),
        );
        state.status = RunStatus::Implementing;
        state.active.insert("impl-A".to_owned(), overrun_seat());

        // A seat still within its own budget: not provably dead yet, so this
        // must change nothing.
        let mut fresh = state.clone();
        fresh.active.insert(
            "impl-B".to_owned(),
            crate::run::ActiveSeat {
                node: "implement".to_owned(),
                started_at: now,
                timeout_secs: 3_600,
                attempt: 0,
                task: None,
                command: None,
                index: None,
                total: None,
            },
        );
        // The record is judged as saved, under the run's locks, not as the
        // caller holds it: so the records are saved first.
        fresh.save_under(&home).unwrap();
        assert!(!clear_abandoned_active(&mut fresh, &home, now).unwrap());
        assert!(!fresh.active.is_empty());
        assert_eq!(fresh.status, RunStatus::Implementing);

        state.save_under(&home).unwrap();
        let store = Questions::at(home.join("questions"));
        let q = open_question(&store, &state.id);

        assert!(clear_abandoned_active(&mut state, &home, now).unwrap());
        assert!(state.active.is_empty());
        assert_eq!(state.status, RunStatus::Failed);
        assert!(
            !store.get(&q.id).unwrap().status.open(),
            "the abandoned seat's own open question must not keep badging the \
             operator until some later daemon startup notices it"
        );
    }

    fn overrun_run(now: Timestamp) -> RunState {
        let mut state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "fixture".to_owned(),
            crate::config::Config::default(),
        );
        state.status = RunStatus::Implementing;
        state.active.insert(
            "impl-A".to_owned(),
            crate::run::ActiveSeat {
                node: "implement".to_owned(),
                started_at: now - SignedDuration::new(21_000, 0),
                timeout_secs: 3_600,
                attempt: 0,
                task: None,
                command: None,
                index: None,
                total: None,
            },
        );
        state
    }

    /// The cleanup writes the record under the run's locks, from a fresh read:
    /// a handover that landed after the fold released its own locks is neither
    /// erased by the caller's copy nor allowed to go unnoticed.
    #[test]
    fn clearing_abandoned_seats_keeps_a_handover_that_landed_after_the_fold() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let now = ts("2026-09-14T12:00:00Z");
        let mut stale = overrun_run(now);
        stale.save_under(&home).unwrap();
        let mut handed = RunState::load_under(&stale.id, &home).unwrap();
        handed.released_to = Some("20260901-000000-succ".to_owned());
        handed.released_branches.push("magi/branch/A".to_owned());
        handed.save_under(&home).unwrap();

        assert!(clear_abandoned_active(&mut stale, &home, now).unwrap());
        let saved = RunState::load_under(&stale.id, &home).unwrap();
        assert_eq!(saved.released_to.as_deref(), Some("20260901-000000-succ"));
        assert_eq!(saved.released_branches, ["magi/branch/A"]);
        assert_eq!(saved.status, RunStatus::Failed);
        assert!(saved.active.is_empty());
    }

    /// Held locks and vanished records are "nothing done", never a write.
    #[test]
    fn clearing_abandoned_seats_writes_nothing_while_the_run_is_held_or_gone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let now = ts("2026-09-14T12:00:00Z");
        let mut state = overrun_run(now);
        state.save_under(&home).unwrap();
        let record = home.join("runs").join(&state.id).join("run.json");
        let before = std::fs::read(&record).unwrap();
        {
            let _owner = crate::pr_head::owner_lock(&home, &state.id).unwrap();
            assert!(!clear_abandoned_active(&mut state, &home, now).unwrap());
        }
        {
            let _lease = crate::driver::maintenance(&home, &state.id).unwrap();
            assert!(!clear_abandoned_active(&mut state, &home, now).unwrap());
        }
        assert_eq!(std::fs::read(&record).unwrap(), before);

        std::fs::remove_dir_all(home.join("runs").join(&state.id)).unwrap();
        assert!(!clear_abandoned_active(&mut state, &home, now).unwrap());
        assert!(
            !home.join("runs").join(&state.id).exists(),
            "a deleted run is not brought back by its cleanup"
        );
    }

    /// Write a whole `run.json` that magi can read, over the given state.
    fn write_meta(runs: &Path, id: &str, status: &str, updated_at: &str) {
        let day = &updated_at[..10];
        std::fs::create_dir_all(runs.join(id)).unwrap();
        let body = format!(
            r#"{{"schema": {SCHEMA}, "id": "{id}", "repo": "/nonexistent/repo", "base_branch": "main", "base_commit": "0000000000000000000000000000000000000000", "instruction": "", "created_at": "{day}T00:00:00Z", "updated_at": "{updated_at}", "status": "{status}", "seed": 1}}"#
        );
        std::fs::write(runs.join(id).join("run.json"), body).unwrap();
    }

    /// Write a fully-formed, `Ready`, well-past-grace `run.json` tagged with
    /// an arbitrary schema number - so a test can write one this build's own
    /// `RunState::new` could never produce on its own. Returns the id.
    ///
    /// `wt` becomes this run's `graph.worktree_root`: left at the config
    /// default, `RunState::worktree_root` falls through to
    /// `run::default_worktree_root` - the operator's real `~/wt/magi` - and
    /// `graph::fold_run`'s second sweep would then `read_dir` and remove
    /// worktrees there instead of anything this test owns.
    fn due_run(runs: &Path, wt: &Path, id: &str, schema: u32) -> String {
        due_run_with_status(runs, wt, id, schema, RunStatus::Ready)
    }

    /// [`due_run`], with the terminal status a caller wants instead of the
    /// `Ready` every other caller here happens to want.
    fn due_run_with_status(
        runs: &Path,
        wt: &Path,
        id: &str,
        schema: u32,
        status: RunStatus,
    ) -> String {
        let mut config = crate::config::Config::default();
        config.graph.worktree_root = Some(wt.to_path_buf());
        let mut state = RunState::new(
            PathBuf::from("/nonexistent/repo"),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            String::new(),
            config,
        );
        state.id = id.to_owned();
        state.status = status;
        state.updated_at = ts("2026-08-01T00:00:00Z");
        let mut value = serde_json::to_value(&state).unwrap();
        value["schema"] = serde_json::json!(schema);
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(
            runs.join(id).join("run.json"),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
        id.to_owned()
    }

    #[test]
    fn fold_due_folds_a_superseded_run_same_as_any_other_terminal_one() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        let disk = Disk::default();
        let now = ts("2026-09-05T00:00:00Z");

        let superseded = due_run_with_status(
            &runs,
            &wt,
            "20260801-000000-cccc",
            SCHEMA,
            RunStatus::Superseded,
        );

        let (folded, unreadable) =
            block_on(fold_due(&runs, &home, &wt, &disk, now)).expect("fold_due");
        assert_eq!(
            folded, 1,
            "a superseded run has nothing left for a human to check, so it folds \
             exactly like a merged one"
        );
        assert_eq!(unreadable, 0);
        assert!(
            read_meta(&runs, &superseded).is_ok(),
            "folding drops the worktree, not the record"
        );
    }

    /// A throwaway repo with one commit on `main`, for a test that needs a
    /// real winner worktree `fold_run` can actually remove.
    pub(crate) fn init_repo(dir: &Path) {
        use crate::proc::Quiet as _;
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .quiet()
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "magi test"]);
        run(&["config", "user.email", "magi@example.com"]);
        std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "init"]);
    }

    /// A due, terminal run with a decided winner that still has a real
    /// worktree and branch on disk - so a test can tell whether folding it
    /// actually removed the winner or only left it registered as folded.
    pub(crate) async fn due_run_with_winner_worktree(
        runs: &Path,
        wt: &Path,
        repo: &Path,
        id: &str,
        status: RunStatus,
    ) -> (String, PathBuf) {
        use crate::run::{Candidate, Tally};
        use std::collections::BTreeMap;

        let mut config = crate::config::Config::default();
        config.graph.worktree_root = Some(wt.to_path_buf());
        let mut state = RunState::new(
            repo.to_path_buf(),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            String::new(),
            config,
        );
        state.id = id.to_owned();
        state.status = status;
        state.updated_at = ts("2026-08-01T00:00:00Z");

        let winner_wt = state.worktree_root().join("cand-A");
        crate::git::worktree_add_branch(repo, &winner_wt, &format!("magi/{id}/A"), "main")
            .await
            .expect("winner worktree");

        state.candidates.push(Candidate {
            index: 0,
            label: 'A',
            agent: "agent".to_owned(),
            branch: format!("magi/{id}/A"),
            worktree: winner_wt.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        });
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 1,
            present: 1,
            quorum: 1,
            met_quorum: true,
            uncontested: None,
        });
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(
            runs.join(id).join("run.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
        (id.to_owned(), winner_wt)
    }

    /// [`due_run_with_winner_worktree`] for a *confirmed merged* run: the
    /// fetched base (`refs/remotes/origin/main`) already holds the winner's
    /// tip, so the confirmed-merge source proof passes and the fold keeps an
    /// evidence manifest instead of any patch. The record keeps its old
    /// `updated_at`, so the run stays past any grace.
    pub(crate) async fn merged_run_with_worktree(
        home: &Path,
        repo: &Path,
        wt: &Path,
        id: &str,
    ) -> (String, PathBuf) {
        let (id, worktree) =
            due_run_with_winner_worktree(&home.join("runs"), wt, repo, id, RunStatus::Merged).await;
        crate::git::git(repo, &["update-ref", "refs/remotes/origin/main", "main"])
            .await
            .unwrap();
        let mut state = RunState::load_under(&id, home).unwrap();
        state.merge = Some(crate::run::MergeOutcome {
            mode: crate::config::MergeMode::Pr,
            ok: true,
            detail: "confirmed merged".into(),
            empty: false,
        });
        state.driver_exited = true;
        let aged = state.updated_at;
        state.save_under_at(home, aged).unwrap();
        (id, worktree)
    }

    #[tokio::test]
    async fn completed_task_retains_independent_historical_sources_without_a_merge_proof() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let queue = crate::queue::Queue::at(home.join("queue"));
        let mut task = crate::queue::Task::new(
            "completed".into(),
            "retain independent historical changes".into(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        let mut sources = Vec::new();
        for (id, status) in [
            ("20260801-000000-old1", RunStatus::Superseded),
            ("20260801-000000-old2", RunStatus::Failed),
        ] {
            let (id, worktree) = due_run_with_winner_worktree(&runs, &wt, &repo, id, status).await;
            std::fs::write(worktree.join("independent.txt"), &id).unwrap();
            crate::git::git(&worktree, &["add", "independent.txt"])
                .await
                .unwrap();
            crate::git::git(
                &worktree,
                &["commit", "-m", "Independent historical change"],
            )
            .await
            .unwrap();
            let head = crate::git::rev_parse(&worktree, "HEAD").await.unwrap();
            let mut state = RunState::load_under(&id, home).unwrap();
            state.driver_exited = true;
            if status == RunStatus::Failed {
                state.tally = None;
            }
            state.save_under(home).unwrap();
            task.link_run(&id);
            sources.push((id, worktree, head));
        }
        // A newer empty attempt and task completion are no proof that either
        // independent historical implementation was incorporated.
        let mut latest = RunState::new(
            repo.clone(),
            "main".into(),
            crate::git::rev_parse(&repo, "main").await.unwrap(),
            task.instruction.clone(),
            crate::config::Config::default(),
        );
        latest.status = RunStatus::Ready;
        latest.driver_exited = true;
        latest.save_under(home).unwrap();
        task.start(latest.id.clone());
        task.succeed(crate::queue::CompletionContext::new(
            crate::queue::CompletionActor::DaemonSettle,
            None,
            home,
            &task,
        ));
        queue.put(&mut task).unwrap();

        let (folded, unreadable) = fold_due(
            &runs,
            home,
            &wt,
            &Disk::default(),
            ts("2026-09-05T00:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!((folded, unreadable), (0, 0));
        for (id, worktree, head) in sources {
            assert!(worktree.exists());
            assert_eq!(
                crate::git::rev_parse(&worktree, "HEAD").await.unwrap(),
                head
            );
            let mut state = RunState::load_under(&id, home).unwrap();
            assert!(!state.candidates[0].folded);
            let error = crate::graph::fold_run(&mut state, true, home)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("source of task {}", task.id))
            );
            assert_eq!(
                crate::git::rev_parse(&repo, &state.candidates[0].branch)
                    .await
                    .unwrap(),
                head
            );
            assert!(worktree.exists());
            assert!(runs.join(&id).join("run.json").exists());
        }
    }

    #[tokio::test]
    async fn recovery_retains_historical_sources_after_success_and_automatic_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let mut task = crate::queue::Task::new(
            "recover".into(),
            "keep measured behavior".into(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        let mut sources = Vec::new();
        for (id, status) in [
            ("20260801-000000-old1", RunStatus::Blocked),
            ("20260801-000000-old2", RunStatus::Stalled),
        ] {
            let (id, worktree) = due_run_with_winner_worktree(&runs, &wt, &repo, id, status).await;
            let mut state = RunState::load_under(&id, home).unwrap();
            state.driver_exited = true;
            state.save_under(home).unwrap();
            task.link_run(&id);
            sources.push((id, worktree));
        }
        crate::pr_recovery::retain_sources(&task, home).unwrap();
        let mut recovered = RunState::new(
            repo.clone(),
            "main".into(),
            "base".into(),
            task.instruction.clone(),
            crate::config::Config::default(),
        );
        recovered.config.graph.worktree_root = Some(wt.clone());
        recovered.status = RunStatus::Ready;
        recovered.driver_exited = true;
        recovered.save_under(home).unwrap();
        task.start(recovered.id.clone());
        task.succeed(crate::queue::CompletionContext::new(
            crate::queue::CompletionActor::DaemonSettle,
            None,
            home,
            &task,
        ));
        crate::daemon::supersede_prior_runs(&task, home);
        for (id, _) in &sources {
            assert_eq!(
                RunState::load_under(id, home).unwrap().status,
                RunStatus::Superseded
            );
        }
        let disk = Disk::default();
        let now = Timestamp::now()
            .checked_add(SignedDuration::from_secs(
                i64::try_from(disk.fold_grace_secs + 1).unwrap(),
            ))
            .unwrap();
        let (_, unreadable) = fold_due(&runs, home, &wt, &disk, now).await.unwrap();
        assert_eq!(unreadable, 0);
        for (id, worktree) in sources {
            assert!(worktree.exists());
            assert!(
                crate::git::branch_exists(&repo, &format!("magi/{id}/A"))
                    .await
                    .unwrap()
            );
            let mut historical = RunState::load_under(&id, home).unwrap();
            assert!(!historical.candidates[0].folded);
            let error = crate::graph::fold_run(&mut historical, true, home)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("explicitly retained recovery source")
            );
            assert!(!historical.candidates[0].folded);
            assert!(
                worktree.exists(),
                "every automatic fold entry preserves retained sources"
            );
        }
    }

    #[tokio::test]
    async fn fold_due_drops_a_superseded_runs_own_winner_worktree_but_keeps_a_readys() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let repo = dir.path().join("repo");
        let home = dir.path().to_path_buf();
        let disk = Disk::default();
        let now = ts("2026-09-05T00:00:00Z");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let (superseded_id, superseded_wt) = due_run_with_winner_worktree(
            &runs,
            &wt,
            &repo,
            "20260801-000000-supw",
            RunStatus::Superseded,
        )
        .await;
        // `Ready` is auto-folded exactly like `Superseded` (both terminal and
        // non-resumable), but never landed anywhere - its own winner is
        // deliberately kept, which is what makes it the right contrast here.
        // `Blocked`/`Stalled` are `resumable()` and so never even reach
        // `fold_run` in the first place; they would not exercise the
        // `drop_winner` choice this test is about.
        let (ready_id, ready_wt) = due_run_with_winner_worktree(
            &runs,
            &wt,
            &repo,
            "20260801-000000-rdyw",
            RunStatus::Ready,
        )
        .await;

        let (folded, unreadable) = fold_due(&runs, &home, &wt, &disk, now)
            .await
            .expect("fold_due");
        assert_eq!(folded, 2);
        assert_eq!(unreadable, 0);

        assert!(
            !superseded_wt.exists(),
            "a superseded run's own winner never lands anywhere else, so its worktree \
             must be dropped exactly like a merged run's"
        );
        assert!(
            ready_wt.exists(),
            "a still-ready run's winner may yet be merged by hand - folding must not \
             touch it"
        );

        // Both records survive the fold; only the worktrees differ.
        assert!(read_meta(&runs, &superseded_id).is_ok());
        assert!(read_meta(&runs, &ready_id).is_ok());
    }

    /// What a test does between `fold_one`'s first read and its locks.
    type Interleaving = Box<dyn FnOnce(&Path, &str)>;

    thread_local! {
        /// Run once by `fold_one` after its first read of a run and before the
        /// fold takes the run's locks: the window a handover falls into.
        pub(crate) static AFTER_FIRST_READ: std::cell::RefCell<Option<Interleaving>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Rewrite one field of a saved `run.json` without going through `RunState`.
    fn edit_run(runs: &Path, id: &str, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = runs.join(id).join("run.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        edit(&mut value);
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    const NO_BOUNDS: FoldBounds = FoldBounds {
        inspect: usize::MAX,
        fold: usize::MAX,
        budget: std::time::Duration::from_secs(3600),
    };

    /// A pass that is far enough in the future for every fixture run to be past
    /// its grace period even after `fold_run` re-saved it (which stamps real time).
    fn well_after_every_fold() -> Timestamp {
        Timestamp::now()
            .checked_add(SignedDuration::from_hours(48))
            .unwrap()
    }

    /// One pass from an explicit saved state, without touching the home's cycle file.
    async fn pass_from(
        runs: &Path,
        home: &Path,
        disk: &Disk,
        now: Timestamp,
        saved: Saved,
        bounds: FoldBounds,
    ) -> FoldTurn {
        fold_turn_from(runs, home, disk, now, saved, bounds).await.0
    }

    fn run_json(home: &Path, id: &str) -> RunState {
        RunState::load_under(id, home).unwrap()
    }

    #[tokio::test]
    async fn a_mixed_pass_counts_each_outcome_and_touches_only_what_fold_due_would() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let now = ts("2026-09-05T00:00:00Z");
        let disk = Disk {
            fold_grace_secs: 600,
            ..Disk::default()
        };
        let run = |n: u32| format!("20260801-000000-a{n:03}");
        let mut worktrees = Vec::new();
        for (n, status) in [
            (1, RunStatus::Superseded),  // folds
            (2, RunStatus::Ready),       // nothing to remove: its winner is kept
            (3, RunStatus::Blocked),     // resumable
            (4, RunStatus::Superseded),  // within grace
            (5, RunStatus::Superseded),  // live (daemon claim)
            (6, RunStatus::Superseded),  // unreadable
            (7, RunStatus::Superseded),  // startup incomplete
            (8, RunStatus::Superseded),  // candidate preservation pending
            (9, RunStatus::Superseded),  // explicitly retained
            (10, RunStatus::Superseded), // fold fails: its repository is gone
            (11, RunStatus::Merged),     // confirmed merge, no fetched base to prove it
            (12, RunStatus::Superseded), // live (a borrower holds its output lease)
        ] {
            let (_, worktree) =
                due_run_with_winner_worktree(&runs, &wt, &repo, &run(n), status).await;
            worktrees.push(worktree);
        }
        let mut merged = RunState::load_under(&run(11), home).unwrap();
        merged.merge = Some(crate::run::MergeOutcome {
            mode: crate::config::MergeMode::Pr,
            ok: true,
            detail: "confirmed merged".into(),
            empty: false,
        });
        let aged = merged.updated_at;
        merged.save_under_at(home, aged).unwrap();
        edit_run(&runs, &run(4), |v| {
            v["updated_at"] = "2026-09-04T23:59:00Z".into()
        });
        let mut status = crate::daemon::Status::new();
        status.current = vec![crate::daemon::Current {
            worker: None,
            task: "20260905-000000-task".to_owned(),
            run: run(5),
        }];
        status.updated_at = now;
        crate::daemon::write_status_to(&home.join("daemon.json"), &status).unwrap();
        std::fs::write(runs.join(run(6)).join("run.json"), "{ not json").unwrap();
        edit_run(&runs, &run(7), |v| {
            v["startup"] = serde_json::json!({
                "kind": "implementation", "branch": null, "stage": "prepare",
                "complete": false, "failure": null
            })
        });
        edit_run(&runs, &run(8), |v| {
            v["candidate_preservation"] = serde_json::json!({"A": "rescue pending"})
        });
        let mut task = crate::queue::Task::new(
            "keep".into(),
            "keep".into(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        task.link_run(&run(9));
        crate::pr_recovery::retain_sources(&task, home).unwrap();
        edit_run(&runs, &run(10), |v| {
            v["repo"] = home.join("gone").to_string_lossy().into_owned().into()
        });
        let _borrower = crate::retention::OutputLease::take(&worktrees[11], false).unwrap();

        let turn = pass_from(&runs, home, &disk, now, Saved::Absent, NO_BOUNDS).await;
        assert_eq!(
            (
                turn.inspected,
                turn.invoked,
                turn.folded,
                turn.nothing_left,
                turn.already_folded,
                turn.resumable,
                turn.within_grace,
                turn.live,
                turn.unreadable,
                turn.proof_incomplete,
                turn.recovery_retained,
                turn.failed,
            ),
            // Run 2 (`Ready`, winner kept) has a bay on disk, so it is not
            // settled: a fold is invoked and finds nothing to remove.
            (12, 5, 1, 1, 0, 1, 1, 2, 1, 3, 1, 1),
            "{turn:?}"
        );
        assert_eq!(
            (turn.cursor.as_deref(), turn.stopped),
            (None, FoldStop::EndOfCycle)
        );
        assert!(!worktrees[0].exists(), "the due superseded run folded");
        for (n, worktree) in worktrees.iter().enumerate().skip(1) {
            assert!(
                worktree.exists(),
                "run {} must be left exactly as it was found",
                n + 1
            );
        }
        let note = |id: String| turn.notes.iter().find(|note| note.run == id).unwrap();
        assert_eq!(note(run(7)).outcome, "proof_incomplete");
        let unproven = note(run(11));
        assert_eq!(unproven.outcome, "proof_incomplete");
        assert!(unproven.text.contains("fetched base"), "{unproven:?}");
        assert_eq!(note(run(10)).outcome, "failed");
        assert!(
            turn.notes
                .iter()
                .all(|note| note.text.chars().count() <= NOTE_CHARS)
        );
    }

    /// Bound, cursor and restart: more eligible runs than one pass may fold.
    #[tokio::test]
    async fn bounded_passes_resume_from_the_saved_cursor_and_fold_each_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let now = well_after_every_fold();
        let disk = Disk {
            fold_grace_secs: 600,
            ..Disk::default()
        };
        let bounds = FoldBounds {
            fold: 2,
            ..NO_BOUNDS
        };
        let ids: Vec<String> = (1..=5)
            .map(|n| format!("20260801-000000-b{n:03}"))
            .collect();
        let mut worktrees = Vec::new();
        for id in &ids {
            let (_, worktree) =
                due_run_with_winner_worktree(&runs, &wt, &repo, id, RunStatus::Superseded).await;
            worktrees.push(worktree);
        }
        let gone = |worktrees: &[PathBuf]| worktrees.iter().filter(|w| !w.exists()).count();

        let first = fold_turn(&runs, home, &disk, now, bounds).await;
        assert_eq!(
            (first.folded, first.inspected, first.invoked),
            (2, 2, 2),
            "{first:?}"
        );
        assert_eq!(first.stopped, FoldStop::FoldLimit);
        assert_eq!(first.cursor.as_deref(), Some(ids[1].as_str()));
        assert_eq!(first.high_water.as_deref(), Some(ids[4].as_str()));
        assert_eq!(gone(&worktrees), 2);
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join(FOLD_TURN_CURSOR_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            (saved["cursor"].as_str(), saved["high_water"].as_str()),
            (Some(ids[1].as_str()), Some(ids[4].as_str())),
            "the cycle is saved before anyone could restart"
        );

        // Nothing is held in memory between passes: this is the restart.
        let second = fold_turn(&runs, home, &disk, now, bounds).await;
        assert_eq!(
            (second.folded, second.nothing_left, second.already_folded),
            (2, 0, 0),
            "the first two are not revisited: {second:?}"
        );
        assert_eq!(second.cursor.as_deref(), Some(ids[3].as_str()));
        assert_eq!(gone(&worktrees), 4);

        let third = fold_turn(&runs, home, &disk, now, bounds).await;
        assert_eq!((third.folded, third.inspected), (1, 1), "{third:?}");
        assert_eq!(
            (third.cursor.as_deref(), third.stopped),
            (None, FoldStop::EndOfCycle)
        );
        assert_eq!(gone(&worktrees), 5);
        assert!(
            !home.join(FOLD_TURN_CURSOR_FILE).exists(),
            "a finished cycle leaves nothing saved; the next pass begins another"
        );
        assert_eq!(first.folded + second.folded + third.folded, ids.len());

        // A fold that removed something has not seen the bay empty yet: the next
        // cycle's fold finds nothing left and records it (`fold_settled_at`),
        // so each of these runs spends one more invocation, once.
        let fourth = fold_turn(&runs, home, &disk, now, NO_BOUNDS).await;
        assert_eq!(
            (fourth.nothing_left, fourth.already_folded, fourth.invoked),
            (5, 0, 5),
            "{fourth:?}"
        );
        // Then the whole history is judged in one pass and no fold is started:
        // the fact a fold recorded and a bay confirmed absent are the whole proof.
        let fifth = fold_turn(&runs, home, &disk, now, bounds).await;
        assert_eq!(
            (fifth.already_folded, fifth.inspected, fifth.invoked),
            (5, 5, 0),
            "{fifth:?}"
        );
        assert_eq!(fifth.stopped, FoldStop::EndOfCycle);
    }

    #[tokio::test]
    async fn a_zero_time_budget_still_judges_one_run_per_pass_and_reaches_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let disk = Disk::default();
        let now = well_after_every_fold();
        let mut worktrees = Vec::new();
        for n in 1..=3 {
            let id = format!("20260801-000000-c{n:03}");
            let (_, worktree) =
                due_run_with_winner_worktree(&runs, &wt, &repo, &id, RunStatus::Superseded).await;
            worktrees.push(worktree);
        }
        let bounds = FoldBounds {
            budget: std::time::Duration::ZERO,
            ..NO_BOUNDS
        };
        for (pass, worktree) in worktrees.iter().enumerate() {
            let turn = fold_turn(&runs, home, &disk, now, bounds).await;
            assert_eq!(
                (turn.inspected, turn.folded),
                (1, 1),
                "pass {pass}: {turn:?}"
            );
            assert!(!worktree.exists());
            let expected = if pass == 2 {
                FoldStop::EndOfCycle
            } else {
                FoldStop::TimeBudget
            };
            assert_eq!(turn.stopped, expected);
        }
    }

    #[tokio::test]
    async fn judging_is_bounded_too() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let disk = Disk::default();
        let now = ts("2026-09-05T00:00:00Z");
        let ids: Vec<String> = (1..=5)
            .map(|n| format!("20260801-000000-d{n:03}"))
            .collect();
        for id in &ids {
            write_meta(&runs, id, "blocked", "2026-08-01T00:00:00Z");
        }
        let bounds = FoldBounds {
            inspect: 2,
            ..NO_BOUNDS
        };
        let mut seen = Vec::new();
        for _ in 0..3 {
            let turn = fold_turn(&runs, home, &disk, now, bounds).await;
            assert_eq!(turn.resumable, turn.inspected);
            seen.push((turn.inspected, turn.cursor, turn.stopped));
        }
        assert_eq!(
            seen,
            vec![
                (2, Some(ids[1].clone()), FoldStop::InspectLimit),
                (2, Some(ids[3].clone()), FoldStop::InspectLimit),
                (1, None, FoldStop::EndOfCycle),
            ]
        );
    }

    /// A saved cycle that cannot be trusted starts a clean one, and says so.
    #[tokio::test]
    async fn a_saved_cycle_that_cannot_be_trusted_starts_over_and_says_why() {
        let ids: Vec<String> = (1..=5)
            .map(|n| format!("20260801-000000-r{n:03}"))
            .collect();
        let saved = |cursor: &str, high_water: &str| {
            serde_json::json!({"cursor": cursor, "high_water": high_water}).to_string()
        };
        let cases = [
            ("unreadable", Some("{ not json".to_owned())),
            (
                "unreadable",
                Some(ids[2].clone()), // the plain-id format of an earlier build
            ),
            (
                "no longer exists",
                Some(saved("20260801-000000-q999", &ids[4])),
            ),
            ("below the cursor", Some(saved(&ids[3], &ids[1]))),
            ("nothing left", Some(saved(&ids[4], &ids[4]))),
            // Ids that cannot be run ids, and a mark newer than every run:
            // with the latter, arrivals would never end the cycle.
            ("not a run id", Some(saved("nonsense", &ids[4]))),
            ("not a run id", Some(saved(&ids[1], "zzzz"))),
            (
                "newer than every run",
                Some(saved(&ids[1], "20260801-000000-zzzz")),
            ),
            ("unreadable", None), // a directory where the file belongs
        ];
        for (reason, body) in cases {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path();
            let runs = home.join("runs");
            for id in &ids {
                write_meta(&runs, id, "blocked", "2026-08-01T00:00:00Z");
            }
            match body {
                Some(body) => std::fs::write(home.join(FOLD_TURN_CURSOR_FILE), body).unwrap(),
                None => std::fs::create_dir(home.join(FOLD_TURN_CURSOR_FILE)).unwrap(),
            }
            let bounds = FoldBounds {
                inspect: 2,
                ..NO_BOUNDS
            };
            let turn = fold_turn(
                &runs,
                home,
                &Disk::default(),
                ts("2026-09-05T00:00:00Z"),
                bounds,
            )
            .await;
            assert!(
                turn.cycle_restart
                    .as_deref()
                    .is_some_and(|why| why.contains(reason)),
                "{reason}: {turn:?}"
            );
            assert_eq!(turn.inspected, 2, "{reason}: restarted from the oldest");
            assert_eq!(turn.cursor.as_deref(), Some(ids[1].as_str()), "{reason}");
            assert_eq!(
                turn.high_water.as_deref(),
                Some(ids[4].as_str()),
                "{reason}: a fresh high-water mark"
            );
        }
    }

    #[tokio::test]
    async fn an_unsaved_cursor_is_reported_not_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        for n in 1..=2 {
            write_meta(
                &runs,
                &format!("20260801-000000-e{n:03}"),
                "blocked",
                "2026-08-01T00:00:00Z",
            );
        }
        // A directory where the cursor file belongs cannot be replaced.
        std::fs::create_dir(home.join(FOLD_TURN_CURSOR_FILE)).unwrap();
        let bounds = FoldBounds {
            inspect: 1,
            ..NO_BOUNDS
        };
        let turn = fold_turn(
            &runs,
            home,
            &Disk::default(),
            ts("2026-09-05T00:00:00Z"),
            bounds,
        )
        .await;
        assert!(turn.cursor_error.is_some(), "{turn:?}");
    }

    /// Runs created while a cycle is under way wait for the next one, so a
    /// history that keeps growing cannot keep an earlier run from being
    /// revisited after it leaves its grace period. With a cursor that only
    /// wrapped at the current end of the history, two arrivals per pass against
    /// two judgements per pass never reached the end and the run was never folded.
    #[tokio::test]
    async fn continuous_arrivals_cannot_postpone_a_run_that_leaves_its_grace() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let disk = Disk {
            fold_grace_secs: 600,
            ..Disk::default()
        };
        let bounds = FoldBounds {
            inspect: 2,
            ..NO_BOUNDS
        };
        // The run's record says 2026-08-01T00:00:00Z: inside its grace at the
        // first pass, long past it afterwards.
        let (_, worktree) = due_run_with_winner_worktree(
            &runs,
            &wt,
            &repo,
            "20260801-000000-g001",
            RunStatus::Superseded,
        )
        .await;
        for n in 2..=4 {
            write_meta(
                &runs,
                &format!("20260801-000000-g{n:03}"),
                "blocked",
                "2026-08-01T00:00:00Z",
            );
        }
        let arrive = |pass: u32| {
            for k in 0..2 {
                write_meta(
                    &runs,
                    &format!("20260802-000000-n{pass}{k}"),
                    "blocked",
                    "2026-08-02T00:00:00Z",
                );
            }
        };
        let early = ts("2026-08-01T00:05:00Z");
        let late = ts("2026-09-05T00:00:00Z");

        // N = 4 records at the start: ceil(4 / 2) = 2 passes for the cycle,
        // and the run is folded by the first pass of the next one.
        let first = fold_turn(&runs, home, &disk, early, bounds).await;
        assert_eq!(first.within_grace, 1, "{first:?}");
        assert_eq!(first.stopped, FoldStop::InspectLimit);
        arrive(1);
        let second = fold_turn(&runs, home, &disk, late, bounds).await;
        assert_eq!(second.high_water, first.high_water, "same cycle");
        assert_eq!(second.stopped, FoldStop::EndOfCycle, "{second:?}");
        assert!(worktree.exists(), "judged inside its grace in this cycle");
        arrive(2);
        let third = fold_turn(&runs, home, &disk, late, bounds).await;
        assert_ne!(third.high_water, first.high_water, "a new cycle");
        assert_eq!(third.folded, 1, "{third:?}");
        assert!(!worktree.exists());
    }

    /// A contended fold is still a fold invocation: it takes the run's locks and
    /// reads its record, so the bound counts it. Charging only the folds that
    /// removed something let 64 contended runs start 64 folds.
    #[tokio::test]
    async fn contended_folds_are_charged_to_the_fold_budget() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let now = well_after_every_fold();
        let mut borrowers = Vec::new();
        let mut worktrees = Vec::new();
        for n in 1..=6 {
            let id = format!("20260801-000000-h{n:03}");
            let (_, worktree) =
                due_run_with_winner_worktree(&runs, &wt, &repo, &id, RunStatus::Superseded).await;
            borrowers.push(crate::retention::OutputLease::take(&worktree, false).unwrap());
            worktrees.push(worktree);
        }
        let bounds = FoldBounds {
            fold: 2,
            ..NO_BOUNDS
        };
        let turn = pass_from(&runs, home, &Disk::default(), now, Saved::Absent, bounds).await;
        assert_eq!(
            (turn.invoked, turn.live, turn.inspected, turn.stopped),
            (2, 2, 2, FoldStop::FoldLimit),
            "{turn:?}"
        );
        assert!(worktrees.iter().all(|w| w.exists()));
        drop(borrowers);
    }

    /// The fold acts on the record as it is once the run's locks are held. A
    /// handover (`handover::release`) that lands between the first read and the
    /// locks records `released_to` / `released_branches` and removes the old
    /// worktree, keeping the branch; acting on the earlier read deleted that
    /// branch and saved the earlier record over the handover.
    #[tokio::test]
    async fn a_handover_between_the_read_and_the_locks_is_neither_undone_nor_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let id = "20260801-000000-k001";
        let (_, worktree) =
            due_run_with_winner_worktree(&runs, &wt, &repo, id, RunStatus::Superseded).await;
        let branch = format!("magi/{id}/A");
        AFTER_FIRST_READ.with(|hook| {
            let (repo, worktree, branch) = (repo.clone(), worktree.clone(), branch.clone());
            *hook.borrow_mut() = Some(Box::new(move |home: &Path, id: &str| {
                use crate::proc::Quiet as _;
                let mut fresh = RunState::load_under(id, home).unwrap();
                fresh.candidates[0].folded = true;
                fresh.released_to = Some("20260901-000000-succ".to_owned());
                fresh.released_branches.push(branch);
                let aged = fresh.updated_at;
                fresh.save_under_at(home, aged).unwrap();
                let removed = std::process::Command::new("git")
                    .args(["worktree", "remove"])
                    .arg(&worktree)
                    .current_dir(&repo)
                    .quiet()
                    .output()
                    .unwrap();
                assert!(removed.status.success());
            }));
        });

        let turn = pass_from(
            &runs,
            home,
            &Disk::default(),
            well_after_every_fold(),
            Saved::Absent,
            NO_BOUNDS,
        )
        .await;
        assert_eq!(
            (turn.invoked, turn.folded, turn.nothing_left),
            (1, 0, 1),
            "the run was handed over, not folded: {turn:?}"
        );
        assert!(
            crate::git::branch_exists(&repo, &branch).await.unwrap(),
            "the handed-over branch survives"
        );
        let saved = run_json(home, id);
        assert_eq!(saved.released_to.as_deref(), Some("20260901-000000-succ"));
        assert_eq!(saved.released_branches, [branch]);
        assert!(saved.candidates[0].folded);
    }

    /// Eligibility is judged again on the fresh record: a run that became
    /// resumable between the read and the locks is left exactly as it is.
    #[tokio::test]
    async fn a_run_that_became_resumable_after_the_read_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let id = "20260801-000000-k002";
        let (_, worktree) =
            due_run_with_winner_worktree(&runs, &wt, &repo, id, RunStatus::Superseded).await;
        AFTER_FIRST_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|home: &Path, id: &str| {
                let mut fresh = RunState::load_under(id, home).unwrap();
                fresh.status = RunStatus::Blocked;
                let aged = fresh.updated_at;
                fresh.save_under_at(home, aged).unwrap();
            }));
        });

        let turn = pass_from(
            &runs,
            home,
            &Disk::default(),
            well_after_every_fold(),
            Saved::Absent,
            NO_BOUNDS,
        )
        .await;
        assert_eq!(
            (turn.invoked, turn.resumable, turn.folded),
            (1, 1, 0),
            "{turn:?}"
        );
        assert!(worktree.exists());
        assert_eq!(run_json(home, id).status, RunStatus::Blocked);
    }

    /// A confirmed merge is folded on the turn, and its evidence stays.
    #[tokio::test]
    async fn a_confirmed_merged_run_is_folded_and_keeps_its_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let (id, worktree) =
            merged_run_with_worktree(home, &repo, &wt, "20260801-000000-m001").await;
        let turn = pass_from(
            &home.join("runs"),
            home,
            &Disk::default(),
            well_after_every_fold(),
            Saved::Absent,
            NO_BOUNDS,
        )
        .await;
        assert_eq!(
            (turn.folded, turn.proof_incomplete, turn.failed),
            (1, 0, 0),
            "{turn:?}"
        );
        assert!(!worktree.exists());
        assert!(
            !crate::git::branch_exists(&repo, &format!("magi/{id}/A"))
                .await
                .unwrap()
        );
        let evidence = home.join("runs").join(&id).join("artifacts/fold-evidence");
        assert!(evidence.join("manifest.json").is_file());
        assert!(home.join("runs").join(&id).join("run.json").is_file());
    }

    /// The scenario of issue 1275: a queue that always has something runnable
    /// and a busy worker. Housekeeping is never admitted there, so before this
    /// turn nothing called `fold_due`: the assertion that the old run's
    /// worktree is gone after one fold service pass is what failed on
    /// `origin/main`.
    #[tokio::test]
    async fn the_fold_service_folds_an_old_run_beside_runnable_and_running_work() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let queue = crate::queue::Queue::at(home.join("queue"));
        let mut running = crate::queue::Task::new(
            "busy".into(),
            "busy".into(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        running.status = crate::queue::TaskStatus::Running;
        queue.put(&mut running).unwrap();
        let mut waiting = crate::queue::Task::new(
            "next".into(),
            "next".into(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        queue.put(&mut waiting).unwrap();
        let (id, worktree) = due_run_with_winner_worktree(
            &runs,
            &wt,
            &repo,
            "20260801-000000-f001",
            RunStatus::Superseded,
        )
        .await;
        let config = home.join("magi.toml");
        let write_config = |auto_fold: bool| {
            std::fs::write(
                &config,
                format!(
                    "[disk]\nmin_free_bytes = 0\ncache_limit_bytes = 0\nauto_fold = {auto_fold}\n"
                ),
            )
            .unwrap();
        };
        let payload = serde_json::json!({
            "opts": crate::daemon::Opts {
                repo: home.join("not-a-checkout"),
                config: Some(config.clone()),
                ..crate::daemon::Opts::default()
            },
            "queue": queue.root(),
            "worktrees_root": wt,
            "operation": "fold",
        });
        let receipt = || -> serde_json::Value {
            serde_json::from_slice(
                &std::fs::read(home.join("source-eviction-fold-worker.json")).unwrap(),
            )
            .unwrap()
        };

        write_config(false);
        crate::daemon::run_service_worker(home, payload.clone())
            .await
            .unwrap();
        assert!(worktree.exists(), "auto_fold = false folds nothing");
        assert_eq!(receipt()["outcome"], "skipped");
        assert_eq!(receipt()["detail"], "auto_fold disabled");

        write_config(true);
        crate::daemon::run_service_worker(home, payload)
            .await
            .unwrap();
        assert!(!worktree.exists(), "one pass folded the old finished run");
        assert!(runs.join(&id).join("run.json").exists());
        let receipt = receipt();
        assert_eq!(receipt["outcome"], "completed");
        assert_eq!(receipt["operation"], "fold");
        let fold = &receipt["fold"];
        assert_eq!(fold["records"], 1);
        assert_eq!(fold["inspected"], 1);
        assert_eq!(fold["invoked"], 1);
        assert_eq!(fold["folded"], 1);
        for zero in [
            "nothing_left",
            "already_folded",
            "live",
            "resumable",
            "within_grace",
            "recovery_retained",
            "proof_incomplete",
            "unreadable",
            "record_gone",
            "failed",
        ] {
            assert_eq!(fold[zero], 0, "{zero}");
        }
        assert_eq!(fold["stopped"], "end_of_cycle");
        assert_eq!(fold["high_water"], id);
        assert!(fold["cursor"].is_null());
        assert!(fold["cursor_error"].is_null());
        assert!(fold["cycle_restart"].is_null());
        assert!(fold["elapsed_ms"].is_u64());
        assert_eq!(fold["notes"], serde_json::json!([]));
    }

    /// One pass, no bounds, from the home's own cycle file's absence.
    async fn one_pass(home: &Path, now: Timestamp) -> FoldTurn {
        pass_from(
            &home.join("runs"),
            home,
            &Disk::default(),
            now,
            Saved::Absent,
            NO_BOUNDS,
        )
        .await
    }

    /// A due `Superseded` run with a winner worktree under `home`, plus its repo.
    async fn due_superseded(home: &Path, id: &str) -> (PathBuf, PathBuf) {
        let repo = home.join("repo");
        if !repo.exists() {
            std::fs::create_dir_all(&repo).unwrap();
            init_repo(&repo);
        }
        let (_, worktree) = due_run_with_winner_worktree(
            &home.join("runs"),
            &home.join("wt"),
            &repo,
            id,
            RunStatus::Superseded,
        )
        .await;
        (repo, worktree)
    }

    /// What a fold itself found is what lets the turn skip a run: nothing left
    /// and no bay, recorded under its locks, and the bay confirmed absent now.
    #[tokio::test]
    async fn a_fold_that_finds_nothing_marks_the_run_and_the_turn_then_skips_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let id = "20260801-000000-z001";
        let (_, worktree) = due_superseded(home, id).await;
        let now = well_after_every_fold();

        let first = one_pass(home, now).await;
        assert_eq!((first.folded, first.invoked), (1, 1), "{first:?}");
        assert!(!worktree.exists());
        assert!(
            run_json(home, id).fold_settled_at.is_none(),
            "a fold that removed something has not yet seen the bay empty"
        );

        let second = one_pass(home, now).await;
        assert_eq!((second.nothing_left, second.invoked), (1, 1), "{second:?}");
        assert!(run_json(home, id).fold_settled_at.is_some());

        let record = home.join("runs").join(id).join("run.json");
        let before = std::fs::read(&record).unwrap();
        let third = one_pass(home, now).await;
        assert_eq!(
            (third.already_folded, third.invoked, third.nothing_left),
            (1, 0, 0),
            "{third:?}"
        );
        assert_eq!(std::fs::read(&record).unwrap(), before, "no fold, no save");
    }

    /// An existing bay - even an empty one - is not "gone": the fold is invoked,
    /// removes it and records what it saw.
    #[tokio::test]
    async fn an_empty_bay_is_work_and_a_marked_run_with_work_is_folded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let id = "20260801-000000-z002";
        let (_, worktree) = due_superseded(home, id).await;
        let now = well_after_every_fold();
        one_pass(home, now).await;
        one_pass(home, now).await;
        let bay = run_json(home, id).worktree_root();
        assert!(run_json(home, id).fold_settled_at.is_some());
        assert!(!bay.exists());

        std::fs::create_dir_all(&bay).unwrap();
        let pass = one_pass(home, now).await;
        assert_eq!((pass.invoked, pass.already_folded), (1, 0), "{pass:?}");
        assert!(!bay.exists(), "the fold removed the empty bay");
        assert!(run_json(home, id).fold_settled_at.is_some());

        // Work behind a mark: a bay that holds a worktree again. The fold that
        // removes it clears the mark in the same save.
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        crate::git::worktree_add_branch(&home.join("repo"), &worktree, "magi/z002-again", "main")
            .await
            .unwrap();
        let pass = one_pass(home, now).await;
        assert_eq!((pass.invoked, pass.already_folded), (1, 0), "{pass:?}");
        assert!(!worktree.exists());
        assert!(
            run_json(home, id).fold_settled_at.is_none(),
            "a fold that found work clears the mark"
        );
    }

    #[test]
    fn settled_needs_the_mark_and_a_bay_confirmed_absent() {
        let mut state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc".to_owned(),
            "fixture".to_owned(),
            crate::config::Config::default(),
        );
        assert!(!settled(&state, true, Ok(false)), "no mark: unknown");
        state.fold_settled_at = Some(Timestamp::now());
        assert!(settled(&state, true, Ok(false)));
        assert!(!settled(&state, true, Ok(true)), "a bay on disk is work");
        let failed = Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(!settled(&state, true, failed), "an unanswered check is not");
        state.enabled_worktree_config = true;
        assert!(
            !settled(&state, true, Ok(false)),
            "a pending worktree-config release is work"
        );
        assert!(settled(&state, false, Ok(false)));
    }

    /// A run deleted between the first read and the fold's locks is refused and
    /// nothing of it is brought back: no record, no directory, no lock files.
    #[tokio::test]
    async fn a_run_deleted_after_the_first_read_is_refused_and_not_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let id = "20260801-000000-z003";
        let (repo, worktree) = due_superseded(home, id).await;
        AFTER_FIRST_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|home: &Path, id: &str| {
                std::fs::remove_dir_all(home.join("runs").join(id)).unwrap();
            }));
        });

        let turn = one_pass(home, well_after_every_fold()).await;
        assert_eq!(
            (turn.invoked, turn.record_gone, turn.folded, turn.failed),
            (1, 1, 0, 0),
            "{turn:?}"
        );
        assert!(
            !home.join("runs").join(id).exists(),
            "the fold recreated what was deleted"
        );
        assert!(worktree.exists(), "nothing of a deleted run is folded");
        assert!(
            crate::git::branch_exists(&repo, &format!("magi/{id}/A"))
                .await
                .unwrap()
        );
    }

    /// A record readable at the first look and not under the locks is
    /// `unreadable` - the idle pass counts it like any unreadable record - and
    /// is left exactly as found.
    #[tokio::test]
    async fn a_record_that_became_unreadable_under_the_locks_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let id = "20260801-000000-z004";
        let (_, worktree) = due_superseded(home, id).await;
        AFTER_FIRST_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|home: &Path, id: &str| {
                std::fs::write(home.join("runs").join(id).join("run.json"), "{ not json").unwrap();
            }));
        });
        let (folded, unreadable) = fold_due(
            &home.join("runs"),
            home,
            &home.join("wt"),
            &Disk::default(),
            well_after_every_fold(),
        )
        .await
        .unwrap();
        assert_eq!((folded, unreadable), (0, 1));
        assert!(worktree.exists());
        assert_eq!(
            std::fs::read(home.join("runs").join(id).join("run.json")).unwrap(),
            b"{ not json"
        );
    }

    /// Explicit callers keep the migrating loader: a schema-14 record whose
    /// unfolded candidate is empty and unverified gets its legacy preservation
    /// marker from the migration, and the explicit fold refuses on that fresh
    /// record. The tolerant read housekeeping uses never derives the marker.
    #[tokio::test]
    async fn an_explicit_fold_applies_the_migration_its_preservation_check_rests_on() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let id = "20260801-000000-z005";
        let (_, worktree) = due_superseded(home, id).await;
        edit_run(&home.join("runs"), id, |v| {
            v["schema"] = 14.into();
            v["candidates"][0]["empty"] = true.into();
        });
        let record = home.join("runs").join(id).join("run.json");
        let before = std::fs::read(&record).unwrap();

        let mut first_look = read_state(&home.join("runs"), id).unwrap();
        assert!(first_look.candidate_preservation.is_empty());
        let error = crate::graph::fold_run(&mut first_look, true, home)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<crate::graph::FoldRefusal>(),
                Some(crate::graph::FoldRefusal::ProofIncomplete(_))
            ),
            "{error:#}"
        );
        assert!(worktree.exists());
        assert_eq!(std::fs::read(&record).unwrap(), before, "nothing was saved");

        let turn = one_pass(home, well_after_every_fold()).await;
        assert_eq!(
            turn.folded, 1,
            "housekeeping reads the record as saved: {turn:?}"
        );
        assert!(!worktree.exists());
    }

    /// Only contention is "somebody holds it"; any other failure of a lock call
    /// is a real failure.
    #[test]
    fn only_lock_contention_means_a_live_owner() {
        use crate::graph::held_by_live_owner;
        let io = |kind| anyhow::Error::from(std::io::Error::from(kind));
        assert!(held_by_live_owner(&io(std::io::ErrorKind::WouldBlock)));
        assert!(held_by_live_owner(
            &io(std::io::ErrorKind::WouldBlock).context("run execution lease unavailable")
        ));
        assert!(!held_by_live_owner(&io(std::io::ErrorKind::Interrupted)));
        assert!(!held_by_live_owner(
            &anyhow::Error::msg("ENOLCK").context("output lease: lock")
        ));
        assert!(!held_by_live_owner(&io(
            std::io::ErrorKind::PermissionDenied
        )));
        let held = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(held.path().join("wt")).unwrap();
        let lock = held.path().join("magi-output.lock");
        let _first = crate::retention::OutputLease::lock(&lock, true).unwrap();
        let error = crate::retention::OutputLease::lock(&lock, true)
            .err()
            .unwrap();
        assert!(held_by_live_owner(&error), "{error:#}");
    }

    /// Deleting a run record takes the fold's locks: a held lock is a refusal,
    /// never a record vanishing under a fold.
    #[tokio::test]
    async fn deleting_a_run_is_refused_while_a_fold_holds_it_and_leaves_no_trace_when_gone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let now = Timestamp::now();
        let refused = |result: Result<()>| -> String {
            let error = result.unwrap_err();
            error
                .downcast_ref::<DeleteRefused>()
                .unwrap_or_else(|| panic!("not a refusal: {error:#}"))
                .0
                .clone()
        };
        let id = "20260801-000000-z006";
        let mut state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc".to_owned(),
            "fixture".to_owned(),
            crate::config::Config::default(),
        );
        state.id = id.to_owned();
        state.status = RunStatus::Failed;
        state.save_under(home).unwrap();
        let run_dir = home.join("runs").join(id);

        {
            let _owner = crate::pr_head::owner_lock(home, id).unwrap();
            assert!(refused(delete_run(home, id, now)).contains("try again later"));
        }
        {
            let _lease = crate::driver::maintenance(home, id).unwrap();
            assert!(refused(delete_run(home, id, now)).contains("try again later"));
        }
        assert!(run_dir.join("run.json").is_file());

        delete_run(home, id, now).unwrap();
        assert!(!run_dir.exists());
        assert!(refused(delete_run(home, id, now)).contains("record gone"));
        assert!(
            !run_dir.exists(),
            "refusing must not recreate the directory"
        );

        // The guard on unfolded candidates is judged on the record read under the locks.
        let unfolded = "20260801-000000-z007";
        let (_, worktree) = due_superseded(home, unfolded).await;
        assert!(refused(delete_run(home, unfolded, now)).contains("magi fold"));
        assert!(worktree.exists());
        assert!(home.join("runs").join(unfolded).join("run.json").is_file());
    }

    /// The wholesale removal of an unreadable run takes the same two locks.
    #[tokio::test]
    async fn an_unreadable_run_is_not_removed_while_a_fold_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runs = home.join("runs");
        let wt = home.join("wt");
        let id = "20260801-000000-z008";
        write_meta(&runs, id, "failed", "2026-08-01T00:00:00Z");
        {
            let _lease = crate::driver::maintenance(home, id).unwrap();
            let error = fold_unreadable(&runs, &wt, id).await.unwrap_err();
            assert!(error.is::<DeleteRefused>(), "{error:#}");
            assert!(runs.join(id).join("run.json").is_file());
        }
        let removed = fold_unreadable(&runs, &wt, id).await.unwrap();
        assert_eq!(removed, [format!("runs/{id}")]);
        assert!(!runs.join(id).exists());
    }
}
