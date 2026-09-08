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
    }
    // A cap of `0` is the operator's opt-out (see `Disk::cache_limit_bytes`);
    // `prune_dir`'s `over_limit` cannot distinguish "cap of zero" from "cache
    // must be emptied", so the opt-out is handled here, before the cache is
    // ever measured - the same place `disk_gate` handles a zero
    // `min_free_bytes`.
    if cfg.disk.cache_limit_bytes > 0 {
        if let Some(cache) = cfg.cache_dir() {
            match prune_cache(&cache, cfg.disk.cache_limit_bytes) {
                Ok(pruned) => {
                    out.cache_files = pruned.files;
                    out.cache_freed = pruned.freed;
                }
                Err(e) => tracing::warn!("housekeep: prune cache: {e:#}"),
            }
        }
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
fn stale_enough(dir: &Path, grace_secs: u64, now: Timestamp) -> bool {
    let Ok(modified) = std::fs::metadata(dir).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(ts) = Timestamp::try_from(modified) else {
        return false;
    };
    due(now, ts, grace_secs)
}

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

/// Delete files from the shared build cache until it fits its cap.
///
/// See [`crate::disk::prune_dir`] for the oldest-first policy.
pub fn prune_cache(cache: &Path, limit_bytes: u64) -> Result<Prune> {
    prune_dir(cache, limit_bytes)
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

        // Real wall-clock time, taken after every directory above was
        // created, so a zero grace - the same "always due" escape hatch
        // `due` itself documents - can stand in for "old enough" here
        // without faking a worktree's mtime. The sleep is what keeps that
        // ordering unambiguous on a filesystem whose mtime resolution is
        // coarser than the gap between two back-to-back instructions.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now = Timestamp::now();
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
