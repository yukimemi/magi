//! The disk janitor: finished runs get their worktrees folded and the shared
//! build cache is pruned to its cap. A run whose state magi cannot read is
//! left alone here — see [`fold_due`] — and is only ever removed by an
//! explicit operator action (`magi fold`, or the equivalent phone route).
//!
//! Everything policy-shaped — which statuses are foldable, how long a finished
//! run is left alone, whether the cache is over its limit — is a pure function
//! injected with numbers, so nothing here has to ask the operating system to
//! be testable. The only I/O is the removal itself.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::config::Disk;
use crate::run::{RunState, RunStatus, SCHEMA, short_of};

use crate::disk::{Prune, dir_size, prune_dir};

/// What one janitor pass did, for the caller's log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Housekeeping {
    /// Runs folded (worktrees dropped).
    pub folded: usize,
    /// Unused: automatic housekeeping never removes a run whose state it
    /// cannot read (see [`fold_due`]), so this is always `0`. Kept on the
    /// struct because [`crate::daemon`] already reports it and a run that
    /// changes the shape of this type is a bigger diff than leaving a field
    /// that is honest about counting nothing.
    pub unreadable: usize,
    /// Files dropped from the shared cache.
    pub cache_files: usize,
    /// Bytes freed from the shared cache.
    pub cache_freed: u64,
}

/// Run the janitor: fold due runs, then prune the cache if it is over its cap.
///
/// Both halves are best-effort; a jammed cache lock or a run whose worktree
/// another borrower holds must not stop the other half. Errors are reported
/// through `tracing::warn` - this is housekeeping, and the daemon keeps
/// serving either way.
pub async fn housekeep(
    cfg: &crate::config::Config,
    home: &Path,
    worktrees_root: &Path,
    now: Timestamp,
) -> Housekeeping {
    let mut out = Housekeeping::default();
    if cfg.disk.auto_fold {
        match fold_due(&home.join("runs"), home, worktrees_root, &cfg.disk, now).await {
            Ok(folded) => out.folded = folded,
            Err(e) => tracing::warn!("housekeep: fold due runs: {e:#}"),
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
    out
}

/// Fold every run that is finished, older than the grace period, and not being
/// worked on; count them.
///
/// A run that magi can no longer read — a state file from another schema, a
/// half-written `run.json` — is left exactly as it is. Automatic housekeeping
/// cannot tell a mid-write file from one that will never parse again, and
/// `<home>/runs/<id>/` is the evidence `magi stats` and the deck read; when
/// unsure whether it is safe to touch, the janitor keeps rather than deletes
/// (see the module docs). Discarding a record this unreadable is an explicit
/// operator action (`magi fold`, or the equivalent phone route), never
/// something that happens unattended.
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
) -> Result<usize> {
    let mut folded = 0usize;
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
        let Ok(meta) = read_meta(runs, &id) else {
            continue;
        };
        if meta.status.resumable() || !due(now, meta.updated_at, disk.fold_grace_secs) {
            continue;
        }
        // `read_meta` only demands `status` and `updated_at`, which an older
        // schema's `run.json` can still supply; the stricter schema check in
        // `read_state` can still fail here. That must not cost every other
        // run its turn through this loop, so it is a skip, not a `?`.
        let Ok(mut state) = read_state(runs, &id) else {
            continue;
        };
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
    Ok(folded)
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

/// Read and version-check a whole run state from a runs directory.
///
/// Mirrors [`RunState::load`] but against an explicit directory rather than
/// the process-global home.
fn read_state(runs: &Path, id: &str) -> Result<RunState> {
    let path = runs.join(id).join("run.json");
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let state: RunState =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    if state.schema != SCHEMA {
        bail!(
            "run {} was written by a different magi (schema {}, this build \
             speaks {SCHEMA})",
            state.id,
            state.schema
        );
    }
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
    fn prune_cache_falls_back_to_the_dir_pruner() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"xxxx").unwrap();
        fs::write(dir.path().join("b"), b"yy").unwrap();
        let out = prune_cache(dir.path(), 2).expect("prune");
        assert_eq!(out.files, 1);
        assert_eq!(out.remaining, 2);
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
    fn fold_due_skips_fresh_runnable_and_unreadable_but_folds_a_due_terminal_run() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let wt = dir.path().join("wt");
        let home = dir.path().to_path_buf();
        let disk = Disk::default();
        let now = ts("2026-09-05T00:00:00Z");
        // `graph::fold_run` (invoked below for the due, readable run) saves
        // through the process-global home; pinning it to this test's own
        // directory is what keeps that write off the operator's real one (see
        // `run::home`'s doc). Harmless if another test already pinned it
        // first - this test never reads that global value back.
        crate::run::set_home(dir.path().to_path_buf());

        // 1. Runnable (judging): never folded, however old.
        let judging = "20260801-000000-0001";
        write_meta(&runs, judging, "judging", "2026-08-01T00:00:00Z");

        // 2. Finished but fresh: grace not elapsed.
        let ready_fresh = "20260904-000000-0002";
        write_meta(&runs, ready_fresh, "ready", "2026-09-04T00:00:00Z");

        // 3. Unreadable: left alone. Automatic housekeeping never deletes a
        //    run record it cannot parse (see `fold_due`'s docs); that is an
        //    explicit operator action, not something a background pass does.
        let garbage = "20260901-000000-0004";
        std::fs::create_dir_all(runs.join(garbage)).unwrap();
        std::fs::write(runs.join(garbage).join("run.json"), "not json").unwrap();
        std::fs::create_dir_all(wt.join("0004")).unwrap();

        // 4. Finished, well past grace, and readable: this is the one run
        //    `fold_due` should actually act on.
        let due_ready = "20260801-000000-ffff";
        let mut ready_state = RunState::new(
            PathBuf::from("/nonexistent/repo"),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            String::new(),
            crate::config::Config::default(),
        );
        ready_state.id = due_ready.to_owned();
        ready_state.status = RunStatus::Ready;
        ready_state.updated_at = ts("2026-08-01T00:00:00Z");
        std::fs::create_dir_all(runs.join(due_ready)).unwrap();
        std::fs::write(
            runs.join(due_ready).join("run.json"),
            serde_json::to_string_pretty(&ready_state).unwrap(),
        )
        .unwrap();

        let folded = block_on(fold_due(&runs, &home, &wt, &disk, now)).expect("fold_due");
        assert_eq!(folded, 1, "only the due, readable run");
        assert!(runs.join(judging).exists(), "runnable never folded");
        assert!(runs.join(ready_fresh).exists(), "fresh never folded");
        assert!(runs.join(garbage).exists(), "unreadable record kept");
        assert!(wt.join("0004").exists(), "unreadable worktree kept");
        assert!(
            runs.join(due_ready).exists(),
            "folding drops worktrees, not the record"
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
}
