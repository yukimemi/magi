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
use serde::Deserialize;

use crate::ask::Questions;
use crate::config::Disk;
use crate::run::{RunState, RunStatus, SCHEMA, short_of};

use crate::disk::{Prune, dir_size, prune_dir};

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
    /// Neighbor build caches removed - see [`neighbor_cache_inventory`] and
    /// [`sweep_reclaimable`]. Only ever [`CacheStatus::Reclaimable`] entries;
    /// `Active` and `Unknown` are never touched by this pass.
    pub neighbor_cache_removed: usize,
    /// Bytes freed by the removal above.
    pub neighbor_cache_freed: u64,
}

/// Run the janitor: fold due runs, reclaim orphaned worktrees, prune stale
/// worktree registrations, sweep reclaimable neighbor build caches, then
/// prune the configured cache if it is over its cap.
///
/// Every part is best-effort; a jammed cache lock or a run whose worktree
/// another borrower holds must not stop the rest. Errors are reported through
/// `tracing::warn` - this is housekeeping, and the daemon keeps serving
/// either way.
///
/// Called only once the caller has confirmed nothing is in flight (see
/// [`crate::daemon`]'s own idle-only call site), which is also what makes the
/// neighbor-cache sweep safe to run unattended: a cache a live verify still
/// has open is `Active`, per [`neighbor_cache_inventory`], and this pass
/// never removes anything else.
pub async fn housekeep(
    cfg: &crate::config::Config,
    home: &Path,
    worktrees_root: &Path,
    repo: &Path,
    now: Timestamp,
) -> Housekeeping {
    let mut out = Housekeeping::default();
    if cfg.disk.auto_fold {
        let runs = home.join("runs");
        match fold_due(&runs, home, worktrees_root, &cfg.disk, now).await {
            Ok((folded, unreadable)) => {
                out.folded = folded;
                out.unreadable = unreadable;
            }
            Err(e) => tracing::warn!("housekeep: fold due runs: {e:#}"),
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
        // Same knob as the worktree fold above, not `cache_limit_bytes`: that
        // one caps a single directory's own size, while this is "does magi
        // remove things it finds unattended" — the same question `auto_fold`
        // already answers for a run's worktrees. Only ever `Reclaimable`
        // entries move; see [`neighbor_cache_inventory`]'s own doc on why
        // `Active` and `Unknown` never do, automatically or otherwise.
        let neighbors = neighbor_cache_inventory(cfg, home, worktrees_root, now);
        for (path, bytes, result) in sweep_reclaimable(&neighbors, false) {
            match result {
                Ok(()) => {
                    out.neighbor_cache_removed += 1;
                    out.neighbor_cache_freed += bytes;
                }
                Err(e) => {
                    tracing::warn!("housekeep: remove neighbor cache {}: {e:#}", path.display())
                }
            }
        }
    }
    match prune_cache_if_over_limit(cfg) {
        Ok(Some(pruned)) => {
            out.cache_files = pruned.files;
            out.cache_freed = pruned.freed;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("housekeep: prune cache: {e:#}"),
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
    let mut ids: Vec<String> = std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("run.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort_unstable();
    for id in ids {
        if crate::daemon::is_working_on(home, &id, now) {
            continue;
        }
        let meta = match read_meta(runs, &id) {
            Ok(meta) => meta,
            Err(e) => {
                unreadable += 1;
                tracing::warn!("housekeep: run {id} unreadable, left alone: {e:#}");
                continue;
            }
        };
        if meta.status.resumable() || !due(now, meta.updated_at, disk.fold_grace_secs) {
            continue;
        }
        // `read_meta` already proved the file parses; `read_state` asks for
        // the rest of the fields `graph::fold_run` needs (worktree paths,
        // candidates, tally). A schema mismatch alone does not fail this -
        // see the module docs - so reaching `Err` here means the JSON itself
        // is broken in a way `read_meta` did not exercise, which is rare but
        // not impossible (a body truncated between the two fields it reads
        // and the rest). That must not cost every other run its turn through
        // this loop, so it is a skip, not a `?`.
        let mut state = match read_state(runs, &id) {
            Ok(state) => state,
            Err(e) => {
                unreadable += 1;
                tracing::warn!("housekeep: run {id} unreadable, left alone: {e:#}");
                continue;
            }
        };
        if state.schema != SCHEMA {
            tracing::info!(
                "housekeep: run {id} was written by schema {} (this build speaks {SCHEMA}); \
                 folding it anyway",
                state.schema
            );
        }
        let drop_winner = state.status == RunStatus::Merged;
        // One run's fold must not cost every later run its turn. A worktree
        // another borrower holds, a branch git refuses to delete, a repository
        // that has since moved: each is a reason this run cannot be folded
        // now, and none is a reason to stop the pass. Left unfolded, it is
        // simply due again next time; a `?` here stopped automatic folding
        // permanently at the first such run (finding R3-1-1 of run 51a3).
        match crate::graph::fold_run(&mut state, drop_winner).await {
            Ok(_) => folded += 1,
            Err(e) => tracing::warn!("housekeep: fold {id}: {e:#}"),
        }
    }
    Ok((folded, unreadable))
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
    let path = runs.join(id).join("run.json");
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let state: RunState =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    Ok(state)
}

/// Remove a run that cannot be read: its state directory under `runs` and its
/// worktree directory under `worktrees_root`.
///
/// The state file is the only record of a run's repository and branches, so a
/// run this unreadable is discarded at the filesystem level - there is no
/// candidate list to fold first. The worktrees live under
/// [`crate::run::default_worktree_root`] unless the run's config relocated
/// them, which an unreadable run cannot tell us; the default location is
/// removed, and anything the run placed elsewhere is a leftover for whoever
/// knows where it went.
///
/// Deleting a worktree directory by hand leaves its registration in git, and a
/// registered path cannot be re-`worktree add`-ed until it is pruned - so every
/// worktree is unregistered from its repository first, best-effort, via the
/// `gitdir:` link git keeps inside the directory.
pub async fn fold_unreadable(runs: &Path, worktrees_root: &Path, id: &str) -> Result<Vec<String>> {
    let resolved = resolve_id_path(runs, id)?;
    let mut removed = Vec::new();
    let run_dir = runs.join(&resolved);
    if run_dir.exists() {
        std::fs::remove_dir_all(&run_dir)
            .with_context(|| format!("remove {}", run_dir.display()))?;
        removed.push(format!("runs/{resolved}"));
    }
    let wt = worktrees_root.join(short_of(&resolved));
    if wt.exists() {
        crate::git::remove_worktree_from_linked(&wt).await;
        for e in std::fs::read_dir(&wt).into_iter().flatten().flatten() {
            crate::git::remove_worktree_from_linked(&e.path()).await;
        }
        std::fs::remove_dir_all(&wt).with_context(|| format!("remove {}", wt.display()))?;
        removed.push(wt.to_string_lossy().into_owned());
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
        crate::git::remove_worktree_from_linked(&wt).await;
        for e in std::fs::read_dir(&wt).into_iter().flatten().flatten() {
            crate::git::remove_worktree_from_linked(&e.path()).await;
        }
        match std::fs::remove_dir_all(&wt) {
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
pub fn clear_abandoned_active(state: &mut RunState, home: &Path, now: Timestamp) -> Result<bool> {
    if crate::daemon::is_working_on(home, &state.id, now) || !state.active_all_overrun(now) {
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

/// Delete files from the shared build cache until it fits its cap.
///
/// See [`crate::disk::prune_dir`] for the oldest-first policy.
pub fn prune_cache(cache: &Path, limit_bytes: u64) -> Result<Prune> {
    prune_dir(cache, limit_bytes)
}

/// [`prune_cache`], but resolving the operator's opt-out and missing
/// `CARGO_TARGET_DIR` first — the same two checks [`housekeep`]'s idle pass
/// makes before ever measuring the cache, factored out so
/// [`crate::daemon`]'s between-runs check (see the module's own doc for why
/// congestion can make "idle" arrive too rarely to matter) makes them
/// identically rather than growing its own copy that could drift. `Ok(None)`
/// covers both a cap of `0` (see the module docs on `cache_limit_bytes`) and
/// a config that renders no `CARGO_TARGET_DIR` to aggregate at all.
pub fn prune_cache_if_over_limit(cfg: &crate::config::Config) -> Result<Option<Prune>> {
    if cfg.disk.cache_limit_bytes == 0 {
        return Ok(None);
    }
    let Some(cache) = cfg.cache_dir() else {
        return Ok(None);
    };
    prune_cache(&cache, cfg.disk.cache_limit_bytes).map(Some)
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

/// How confidently a neighbor cache's owner is known.
///
/// The same asymmetry the rest of this module is built around governs this
/// too: wrongly reclaiming a cache a live run still needs costs that run its
/// build, wrongly leaving a genuinely dead one as `Unknown` costs nothing but
/// a report the operator reads once and a few GB they clear by hand. Every
/// classifier that produces this is built to fail toward `Unknown`, never
/// toward `Reclaimable`, whenever it cannot fully tell.
///
/// Ownership here is read-only inference from `<home>/runs`, never a new
/// ledger of its own — a lease system and live-process visibility for a
/// resumed child are separate concerns this deliberately leaves alone. A path
/// this cannot attribute to any run's own record stays `Unknown` until an
/// operator, or a future ledger, says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    /// A run that owns this path is either being worked on right now, or is
    /// still resumable — parked, mid-review, or otherwise not finished.
    Active,
    /// Every run that owns this path is finished, and nothing here suggests
    /// a build still has it open.
    Reclaimable,
    /// No run's record points here, or one does but something about it
    /// withheld trust (see [`classify_neighbor`]). Never removed
    /// automatically.
    Unknown,
}

/// One build-cache directory found outside the single path magi's own cap
/// and janitor already track (see [`crate::disk::scan_neighbor_caches`]),
/// classified against the run records that might own it.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// The directory itself.
    pub path: PathBuf,
    /// Its size on disk.
    pub bytes: u64,
    /// The run id whose rendered verify command names this path, when one was
    /// found. `None` alongside [`CacheStatus::Unknown`] means no record
    /// claims it at all; `Some` alongside `Unknown` means a record does, but
    /// [`classify_neighbor`] would not vouch for it being free.
    pub owner: Option<String>,
    /// What [`classify_neighbor`] decided.
    pub status: CacheStatus,
    /// Why, in words an operator reads on `magi cache status` or a sweep's
    /// `--dry-run` preview without having to already know how
    /// [`classify_neighbor`] works. Every `CacheStatus` variant gets one, not
    /// only `Unknown` — "why did magi leave this alone" is exactly as much a
    /// question for an `Active` entry, and a silent line next to `Reclaimable`
    /// is what a PowerShell recipe already gave the operator.
    pub reason: String,
}

/// [`classify_neighbor`]'s answer: who (if anyone) owns the path, what that
/// makes it, and the sentence a CLI prints next to it.
struct Classification {
    owner: Option<String>,
    status: CacheStatus,
    reason: String,
}

/// Decide one neighbor cache's [`CacheStatus`] from the runs that own it.
///
/// `owners` is every `(run id, that run's own cache directory, is that run
/// still active)` triple worth checking, built by the caller from
/// `<home>/runs` — kept as a pure decision over already-extracted facts
/// rather than reading the filesystem itself, so the policy is asserted
/// directly. `path` and every path inside `owners` are expected already
/// canonicalized by the caller ([`crate::disk::canonical_or`]), so this never
/// has to guess whether two different-looking paths name the same directory.
/// More than one run can legitimately own the same path — the configured
/// cache is shared across a repository's whole run history — so any one
/// active owner is enough to withhold the entry, and only when every owner
/// found is finished does [`has_lock`] get the final say.
fn classify_neighbor(
    path: &Path,
    owners: &[(String, PathBuf, bool)],
    has_lock: bool,
) -> Classification {
    let matches: Vec<&(String, PathBuf, bool)> =
        owners.iter().filter(|(_, p, _)| p == path).collect();
    if let Some((id, _, _)) = matches.iter().find(|(_, _, active)| *active) {
        return Classification {
            owner: Some(id.clone()),
            status: CacheStatus::Active,
            reason: format!("run {id} is still resumable or being worked on"),
        };
    }
    let Some((id, _, _)) = matches.first() else {
        return Classification {
            owner: None,
            status: CacheStatus::Unknown,
            reason: "no run record claims this path".to_owned(),
        };
    };
    if has_lock {
        Classification {
            owner: Some(id.clone()),
            status: CacheStatus::Unknown,
            reason: format!(
                "run {id} is finished, but a `.cargo-lock` file is present — \
                 a resumed or otherwise still-running build may still hold it"
            ),
        }
    } else {
        Classification {
            owner: Some(id.clone()),
            status: CacheStatus::Reclaimable,
            reason: format!("run {id} is finished and no `.cargo-lock` file is present"),
        }
    }
}

/// Build-cache directories outside the single path the cap and the janitor
/// already track, classified per [`classify_neighbor`].
///
/// Scans one level next to the configured cache — see
/// [`crate::disk::scan_neighbor_caches`] for why not further, and why the
/// worktree bay and `<home>/runs` are always excluded regardless of what they
/// look like. Ownership is read back out of every readable run record's own
/// saved `Config`, the same field [`crate::config::Verify::cache_dir`] reads
/// off a live one — a run's `run.json` keeps the config it actually ran
/// with, rendered at the time, so this is exact for whichever cache a given
/// run actually built into, even one the operator's `magi.toml` has since
/// moved on from. A run record this cannot read is skipped exactly like
/// everywhere else in this module: left neither as evidence of ownership nor
/// against it.
pub fn neighbor_cache_inventory(
    cfg: &crate::config::Config,
    home: &Path,
    worktrees_root: &Path,
    now: Timestamp,
) -> Vec<CacheEntry> {
    let Some(configured) = cfg.cache_dir() else {
        return Vec::new();
    };
    let runs = home.join("runs");
    let mut owners: Vec<(String, PathBuf, bool)> = Vec::new();
    for entry in std::fs::read_dir(&runs).into_iter().flatten().flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if !crate::run::is_run_id(&id) {
            continue;
        }
        let Ok(state) = read_state(&runs, &id) else {
            continue;
        };
        let Some(path) = state.config.cache_dir() else {
            continue;
        };
        let active = crate::daemon::is_working_on(home, &id, now) || state.status.resumable();
        owners.push((id, crate::disk::canonical_or(&path), active));
    }

    let neighbors =
        crate::disk::scan_neighbor_caches(&configured, &[worktrees_root.to_path_buf(), runs]);
    neighbors
        .into_iter()
        .map(|n| {
            let canon = crate::disk::canonical_or(&n.path);
            let has_lock = crate::disk::has_lock_file(&n.path);
            let c = classify_neighbor(&canon, &owners, has_lock);
            CacheEntry {
                path: n.path,
                bytes: n.bytes,
                owner: c.owner,
                status: c.status,
                reason: c.reason,
            }
        })
        .collect()
}

/// Delete every [`CacheStatus::Reclaimable`] entry in `entries`; leave
/// `Active` and `Unknown` alone regardless of what else is true about them.
///
/// `dry_run` skips the actual removal but still walks the same list, so a
/// preview and the real sweep report identically shaped results — what
/// `magi cache sweep --dry-run` shows is exactly what `magi cache sweep`
/// would have done. Best-effort per entry: a directory Windows still has a
/// file open in (see [`crate::disk::has_lock_file`]'s own doc on why that is
/// not always caught in advance) fails its own `remove_dir_all` without
/// costing the rest of the sweep its turn.
pub fn sweep_reclaimable(entries: &[CacheEntry], dry_run: bool) -> Vec<(PathBuf, u64, Result<()>)> {
    entries
        .iter()
        .filter(|e| e.status == CacheStatus::Reclaimable)
        .map(|e| {
            let result = if dry_run {
                Ok(())
            } else {
                std::fs::remove_dir_all(&e.path)
                    .with_context(|| format!("remove {}", e.path.display()))
            };
            (e.path.clone(), e.bytes, result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
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
    fn fold_unreadable_releases_run_dir_and_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let id = "20260905-000000-abcd";
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(runs.join(id).join("run.json"), "garbage").unwrap();
        std::fs::create_dir_all(wt.join("abcd")).unwrap();
        std::fs::write(wt.join("abcd").join("leftover"), b"x").unwrap();

        let removed = block_on(fold_unreadable(&runs, &wt, id)).expect("fold");
        assert_eq!(removed.len(), 2);
        assert!(!runs.join(id).exists(), "run dir gone");
        assert!(!wt.join("abcd").exists(), "worktrees gone");

        // A prefix resolves like `run::resolve_id` does.
        std::fs::create_dir_all(runs.join(id)).unwrap();
        std::fs::write(runs.join(id).join("run.json"), "garbage").unwrap();
        std::fs::create_dir_all(wt.join("abcd")).unwrap();
        std::fs::write(wt.join("abcd").join("leftover"), b"x").unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        // Same size, different age: only the age decides, and the newest
        // generation - the one the next build reuses - is what survives.
        fs::write(dir.path().join("old"), b"xx").unwrap();
        fs::write(dir.path().join("new"), b"yy").unwrap();
        touch(&dir.path().join("old"), 1_000_000);
        touch(&dir.path().join("new"), 2_000_000);

        let out = prune_cache(dir.path(), 2).expect("prune");
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
        let out = prune_cache(tied.path(), 2).expect("prune");
        assert_eq!(out.files, 1, "the big one alone gets under the cap");
        assert_eq!(out.remaining, 2);
        assert!(tied.path().join("small").exists());
    }

    #[test]
    fn prune_cache_if_over_limit_resolves_the_opt_outs_before_ever_measuring() {
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
            prune_cache_if_over_limit(&cfg).unwrap(),
            None,
            "a zero cap must not even look at the directory"
        );
        assert!(dir.path().join("big").exists());

        // No `CARGO_TARGET_DIR` in either verify command: nothing to
        // aggregate, so there is nothing to prune either.
        let mut no_cache = crate::config::Config::default();
        no_cache.disk.cache_limit_bytes = 1;
        assert_eq!(prune_cache_if_over_limit(&no_cache).unwrap(), None);

        // Over the cap and configured: pruned exactly like `prune_cache`
        // itself would.
        cfg.disk.cache_limit_bytes = 1;
        let pruned = prune_cache_if_over_limit(&cfg)
            .unwrap()
            .expect("a real cache dir over its cap prunes");
        assert_eq!(pruned.files, 1);
        assert!(!dir.path().join("big").exists());
    }

    #[test]
    fn classify_neighbor_prefers_active_over_any_other_owner_of_the_same_path() {
        let path = PathBuf::from("/cache/orphan");
        let owners = vec![
            ("20260801-000000-fini".to_owned(), path.clone(), false),
            ("20260801-000000-live".to_owned(), path.clone(), true),
        ];
        let c = classify_neighbor(&path, &owners, false);
        assert_eq!(
            c.owner,
            Some("20260801-000000-live".to_owned()),
            "one active owner is enough to withhold a path several runs share"
        );
        assert_eq!(c.status, CacheStatus::Active);
        assert!(
            c.reason.contains("20260801-000000-live"),
            "the reason names the run that is holding it: {}",
            c.reason
        );
    }

    #[test]
    fn classify_neighbor_is_reclaimable_only_once_finished_and_unlocked() {
        let path = PathBuf::from("/cache/orphan");
        let finished = vec![("20260801-000000-fini".to_owned(), path.clone(), false)];

        let unlocked = classify_neighbor(&path, &finished, false);
        assert_eq!(unlocked.owner, Some("20260801-000000-fini".to_owned()));
        assert_eq!(unlocked.status, CacheStatus::Reclaimable);

        let locked = classify_neighbor(&path, &finished, true);
        assert_eq!(locked.owner, Some("20260801-000000-fini".to_owned()));
        assert_eq!(
            locked.status,
            CacheStatus::Unknown,
            "a lock file withholds trust even once every owner is finished"
        );
        assert!(
            locked.reason.contains("cargo-lock"),
            "the reason says what withheld trust: {}",
            locked.reason
        );

        let unowned = classify_neighbor(&path, &[], false);
        assert_eq!(unowned.owner, None);
        assert_eq!(
            unowned.status,
            CacheStatus::Unknown,
            "no owner at all is unknown, never reclaimable by default"
        );
        assert!(unowned.reason.contains("no run record"));
    }

    #[test]
    fn neighbor_cache_inventory_attributes_by_reading_each_runs_own_saved_config() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        let now = ts("2026-09-05T00:00:00Z");

        let mut cfg = crate::config::Config::default();
        let configured_cache = dir.path().join("magi-target");
        cfg.verify.gate = vec![format!(
            "CARGO_TARGET_DIR={} cargo make check",
            configured_cache.display()
        )];
        fs::create_dir_all(&configured_cache).unwrap();

        // A finished run whose own saved config points at a neighbor cache
        // nothing else claims: reclaimable.
        let dead_cache = dir.path().join("magi-land6");
        fs::create_dir_all(&dead_cache).unwrap();
        fs::write(dead_cache.join("CACHEDIR.TAG"), b"tag").unwrap();
        write_run_with_cache(&runs, &wt, "20260801-000000-dead", "ready", &dead_cache);

        // A run still resumable, pointing at a different neighbor: active,
        // however old its `updated_at`.
        let live_cache = dir.path().join("magi-land7");
        fs::create_dir_all(&live_cache).unwrap();
        fs::write(live_cache.join("CACHEDIR.TAG"), b"tag").unwrap();
        write_run_with_cache(&runs, &wt, "20260801-000000-live", "blocked", &live_cache);

        // A cargo-shaped neighbor no run record names at all: unknown.
        let orphan_cache = dir.path().join("magi-land9");
        fs::create_dir_all(&orphan_cache).unwrap();
        fs::write(orphan_cache.join("CACHEDIR.TAG"), b"tag").unwrap();

        let mut entries = neighbor_cache_inventory(&cfg, &home, &wt, now);
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let by_path: std::collections::BTreeMap<_, _> =
            entries.iter().map(|e| (e.path.clone(), e)).collect();

        assert_eq!(entries.len(), 3, "{entries:?}");
        assert_eq!(by_path[&dead_cache].status, CacheStatus::Reclaimable);
        assert_eq!(
            by_path[&dead_cache].owner.as_deref(),
            Some("20260801-000000-dead")
        );
        assert!(by_path[&dead_cache].reason.contains("20260801-000000-dead"));
        assert_eq!(by_path[&live_cache].status, CacheStatus::Active);
        assert_eq!(by_path[&orphan_cache].status, CacheStatus::Unknown);
        assert_eq!(by_path[&orphan_cache].owner, None);
        assert!(
            by_path[&orphan_cache].reason.contains("no run record"),
            "an entry with a plain english reason, not just a status word: {}",
            by_path[&orphan_cache].reason
        );
    }

    /// Write a minimal run whose saved `Config` renders `CARGO_TARGET_DIR` to
    /// `cache`, so [`neighbor_cache_inventory`] can read it back exactly the
    /// way it would a real run's own recorded config.
    fn write_run_with_cache(runs: &Path, wt: &Path, id: &str, status: &str, cache: &Path) {
        let mut config = crate::config::Config::default();
        config.graph.worktree_root = Some(wt.to_path_buf());
        config.verify.gate = vec![format!("CARGO_TARGET_DIR={} cargo test", cache.display())];
        let mut state = RunState::new(
            PathBuf::from("/nonexistent/repo"),
            "main".to_owned(),
            "deadbeef".to_owned(),
            String::new(),
            config,
        );
        state.id = id.to_owned();
        std::fs::create_dir_all(runs.join(id)).unwrap();
        let mut value = serde_json::to_value(&state).unwrap();
        value["status"] = serde_json::json!(status);
        value["updated_at"] = serde_json::json!("2026-08-01T00:00:00Z");
        std::fs::write(
            runs.join(id).join("run.json"),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sweep_reclaimable_deletes_only_reclaimable_entries_and_a_dry_run_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("active");
        let reclaimable = dir.path().join("reclaimable");
        let unknown = dir.path().join("unknown");
        for d in [&active, &reclaimable, &unknown] {
            fs::create_dir_all(d).unwrap();
        }
        let entries = vec![
            CacheEntry {
                path: active.clone(),
                bytes: 1,
                owner: Some("a".to_owned()),
                status: CacheStatus::Active,
                reason: "run a is still resumable or being worked on".to_owned(),
            },
            CacheEntry {
                path: reclaimable.clone(),
                bytes: 2,
                owner: Some("b".to_owned()),
                status: CacheStatus::Reclaimable,
                reason: "run b is finished and no `.cargo-lock` file is present".to_owned(),
            },
            CacheEntry {
                path: unknown.clone(),
                bytes: 3,
                owner: None,
                status: CacheStatus::Unknown,
                reason: "no run record claims this path".to_owned(),
            },
        ];

        let preview = sweep_reclaimable(&entries, true);
        assert_eq!(
            preview.len(),
            1,
            "only the reclaimable entry is even a candidate"
        );
        assert!(reclaimable.exists(), "a dry run never removes anything");

        let done = sweep_reclaimable(&entries, false);
        assert_eq!(done.len(), 1);
        assert!(done[0].2.is_ok());
        assert!(
            !reclaimable.exists(),
            "the reclaimable entry is actually gone"
        );
        assert!(active.exists(), "active is never touched by this sweep");
        assert!(unknown.exists(), "unknown is never touched by this sweep");
    }

    #[tokio::test]
    async fn housekeep_sweeps_a_reclaimable_neighbor_cache_only_when_auto_fold_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&runs).unwrap();
        init_repo(&repo);

        let mut cfg = crate::config::Config::default();
        let configured = dir.path().join("magi-target");
        std::fs::create_dir_all(&configured).unwrap();
        cfg.verify.gate = vec![format!(
            "CARGO_TARGET_DIR={} cargo make check",
            configured.display()
        )];
        // Opting out of the size cap and the disk gate keeps this test about
        // the neighbor sweep alone, not the other two policies `housekeep`
        // also runs.
        cfg.disk.cache_limit_bytes = 0;

        let dead_cache = dir.path().join("magi-land6");
        std::fs::create_dir_all(&dead_cache).unwrap();
        std::fs::write(dead_cache.join("CACHEDIR.TAG"), b"tag").unwrap();
        write_run_with_cache(&runs, &wt, "20260801-000000-dead", "ready", &dead_cache);

        // `auto_fold = false` is the operator's opt-out for every unattended
        // removal this pass makes, worktrees and neighbor caches alike - it
        // must leave this exactly as it found it.
        cfg.disk.auto_fold = false;
        let out = housekeep(&cfg, &home, &wt, &repo, Timestamp::now()).await;
        assert_eq!(out.neighbor_cache_removed, 0);
        assert!(dead_cache.exists(), "auto_fold = false touches nothing");

        cfg.disk.auto_fold = true;
        let out = housekeep(&cfg, &home, &wt, &repo, Timestamp::now()).await;
        assert_eq!(out.neighbor_cache_removed, 1);
        assert!(out.neighbor_cache_freed > 0);
        assert!(
            !dead_cache.exists(),
            "a reclaimable neighbor cache is swept at the same run boundary \
             the worktree fold already runs at"
        );
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
        // `graph::fold_run` (invoked below for the due, readable runs) saves
        // through the process-global home; pinning it to this test's own
        // directory is what keeps that write off the operator's real one (see
        // `run::home`'s doc). Harmless if another test already pinned it
        // first - this test never reads that global value back.
        crate::run::set_home(dir.path().to_path_buf());

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
        let due_ready = due_run(&runs, "20260801-000000-ffff", SCHEMA);

        // 5. Finished, well past grace, but written by a schema number this
        //    build no longer matches - the defect this task exists to fix.
        //    It still parses cleanly, so only the version number differs, and
        //    that alone must not block folding.
        let due_old_schema = due_run(&runs, "20260801-000000-eeee", SCHEMA - 1);

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
    }

    /// A run parked mid-graph is neither `resumable() == false` nor `done()`
    /// — its `status` is whatever non-terminal node it stopped at (see
    /// `graph::Runner::park_here`) — so it already takes the same
    /// `resumable()` exit `fold_due` gives any other in-progress run. This
    /// pins that down explicitly for `parked`, rather than leaving it as an
    /// inference from the ordinary "runnable" case.
    #[test]
    fn fold_due_leaves_a_parked_runs_worktree_alone() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        let disk = Disk::default();
        let now = ts("2026-09-05T00:00:00Z");

        let parked = "20260801-000000-park";
        // Well past the default grace period, so only the park keeps it —
        // the same shape the operator hits when an interrupt task parks a
        // long-running competition overnight.
        write_meta(&runs, parked, "implementing", "2026-08-01T00:00:00Z");
        std::fs::create_dir_all(wt.join("park")).unwrap();

        let (folded, unreadable) =
            block_on(fold_due(&runs, &home, &wt, &disk, now)).expect("fold_due");
        assert_eq!(folded, 0, "a parked run is never due, however old");
        assert_eq!(unreadable, 0);
        assert!(runs.join(parked).exists(), "parked run record kept");
    }

    /// `Blocked` and `Stalled` are `resumable()`, so they never even reach
    /// this far — `fold_due` stops at the freshness check for them
    /// regardless of age (see the "runnable" case in
    /// `fold_due_folds_terminal_runs_of_any_schema_but_leaves_genuinely_unreadable_ones`).
    /// `Ready` is the terminal status that actually exercises the winner-vs-
    /// loser split unattended: `MergeMode::None` (or a pull request closed by
    /// hand) leaves a run `Ready` with nobody having merged its winner, and
    /// `fold_due` still folds it once its grace elapses — `drop_winner` is
    /// only ever true for `Merged`. This is the line `magi fold`'s own
    /// `--all` flag draws, asserted for the *unattended* path: a winner
    /// nobody merged is the operator's own answer to look at, not junk the
    /// janitor may sweep on its own, while the loser and a `Merged` run's
    /// winner are both compiles of history either way.
    #[tokio::test]
    async fn fold_due_keeps_an_unmerged_winner_but_drops_a_merged_one() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        let disk = Disk::default();
        let now = ts("2026-09-05T00:00:00Z");

        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let ready_id = "20260801-000000-rdy1";
        let ready_root =
            write_run_with_candidates(&runs, &wt, &repo, ready_id, RunStatus::Ready).await;
        let merged_id = "20260801-000000-mrgd";
        let merged_root =
            write_run_with_candidates(&runs, &wt, &repo, merged_id, RunStatus::Merged).await;

        let (folded, unreadable) = fold_due(&runs, &home, &wt, &disk, now)
            .await
            .expect("fold_due");
        assert_eq!(folded, 2);
        assert_eq!(unreadable, 0);

        assert!(
            ready_root.join("cand-A").exists(),
            "a ready run finished without merging — its winner is the \
             operator's own answer to look at, not junk to sweep"
        );
        assert!(
            !ready_root.join("cand-B").exists(),
            "the loser never had a reason to survive, merged or not"
        );
        assert!(
            !merged_root.join("cand-A").exists(),
            "once merged, the winner's worktree is a compile of history, \
             exactly like the loser's"
        );
    }

    /// Write a run with two real candidate worktrees (`A` wins, `B` loses)
    /// under `wt`, well past the default fold grace, and return the run's own
    /// worktree root.
    async fn write_run_with_candidates(
        runs: &Path,
        wt: &Path,
        repo: &Path,
        id: &str,
        status: RunStatus,
    ) -> PathBuf {
        let short = crate::run::short_of(id).to_owned();
        let mut config = crate::config::Config::default();
        config.graph.worktree_root = Some(wt.to_path_buf());
        let mut state = RunState::new(
            repo.to_path_buf(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            String::new(),
            config,
        );
        state.id = id.to_owned();
        state.status = status;

        let root = wt.join(&short);
        for label in ['A', 'B'] {
            let branch = format!("magi/{short}/{label}");
            let path = root.join(format!("cand-{label}"));
            crate::git::worktree_add_branch(repo, &path, &branch, "main")
                .await
                .expect("add worktree");
            state.candidates.push(crate::run::Candidate {
                index: usize::from(label == 'B'),
                label,
                agent: "alpha".to_owned(),
                branch,
                worktree: path,
                summary: String::new(),
                stat: String::new(),
                files: 0,
                commits: 0,
                empty: false,
                failed: None,
                duration_ms: 0,
                folded: false,
            });
        }
        state.tally = Some(crate::run::Tally {
            first_choice: std::collections::BTreeMap::from([('A', 1)]),
            borda: std::collections::BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            met_quorum: true,
            quorum: 0,
            uncontested: None,
        });

        std::fs::create_dir_all(runs.join(id)).unwrap();
        let mut value = serde_json::to_value(&state).unwrap();
        value["updated_at"] = serde_json::json!("2026-08-01T00:00:00Z");
        std::fs::write(
            runs.join(id).join("run.json"),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
        root
    }

    /// A throwaway repo with one commit on `main`, so `git worktree add`
    /// (and, if it runs at all, `git branch -d`) have something real to work
    /// against.
    fn init_repo(dir: &Path) {
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
            },
        );
        assert!(!clear_abandoned_active(&mut fresh, &home, now).unwrap());
        assert!(!fresh.active.is_empty());
        assert_eq!(fresh.status, RunStatus::Implementing);

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
    fn due_run(runs: &Path, id: &str, schema: u32) -> String {
        let mut state = RunState::new(
            PathBuf::from("/nonexistent/repo"),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            String::new(),
            crate::config::Config::default(),
        );
        state.id = id.to_owned();
        state.status = RunStatus::Ready;
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
}
