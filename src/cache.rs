//! Durable ownership of the shared Cargo build cache (`CARGO_TARGET_DIR`).
//!
//! One cache, many borrowers: an implement wave's candidates, a review
//! round's E2E, the final gate, a human's `magi review` invoked by hand — all
//! of them may point `CARGO_TARGET_DIR` at the same directory, sometimes from
//! *different* worktrees, sometimes from a different `magi run` entirely. Two
//! failures came out of sharing it with no ownership record at all:
//!
//! - A read-only reviewer seat that inherited `CARGO_TARGET_DIR` tried to
//!   write into it, was refused by its own sandbox, and reported the refusal
//!   as a defect in the code under review rather than a property of its own
//!   seat. See `graph::wave`, which now only hands the variable to a seat
//!   that [`crate::agent::Invocation::allow_write`] actually permits to use.
//! - Two source trees building the same package name/version into one
//!   `CARGO_TARGET_DIR` in sequence can leave a stale artifact from the
//!   *older* worktree looking fresh to Cargo, and a later `cargo test` runs
//!   binaries compiled from source nobody is looking at. [`ensure_fresh`]
//!   selectively `cargo clean -p`s the workspace's own packages — never the
//!   downloaded dependency graph — the moment the recorded source identity
//!   for a cache directory changes.
//!
//! Both are consequences of one missing fact: *who is using this cache right
//! now, and against which source*. This module is that fact, made durable
//! (a JSON file per cache directory, surviving a process restart) and cheap
//! to ask (`classify`, `in_use`, `inventory`).
//!
//! ## Reclaiming a dead owner
//!
//! Liveness is [`crate::proc::pid_alive`] — the same conservative check
//! [`crate::daemon::sweep_stale_claims`] uses for the queue's own claim
//! locks: every uncertain outcome reads as alive, and a lease whose file
//! cannot even be parsed is never reclaimed automatically ([`Status::Unknown`]).
//! This is deliberately the same policy as the queue's `.lock` files, not a
//! new one — a bare lock file was never trusted alone there either; it is
//! always paired with a liveness check.
//!
//! ## What this module does not do
//!
//! It does not reap a build's orphaned grandchildren once magi kills the
//! seat that started them at a timeout — that is process-tree ownership, a
//! different problem with its own owner elsewhere. A lease held by a process
//! whose pid has exited is `Stale` and reclaimable *as a lease*, whether or
//! not a grandchild is still writing files under the cache; this module only
//! ever answers "who owns the cache directory", not "is every process that
//! might still be touching it definitely gone".

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::proc::{self, Quiet as _};

/// Who is holding, or wants, a cache directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// The run this borrow belongs to.
    pub run: String,
    /// The graph node, e.g. `implement`, `review`, `gate`.
    pub node: String,
    /// Seat key, or a fixed label for a borrow that is not seat-shaped
    /// (`"e2e"`, `"gate"`, `"janitor"`).
    pub seat: String,
    /// Process id of the magi process holding the lease — never the spawned
    /// agent CLI or `cargo` child, both of which end well before this
    /// process's own async task returns and releases the guard.
    pub pid: u32,
    /// The worktree the borrower is building from, for the report and for
    /// [`ensure_fresh`]'s identity comparison.
    pub worktree: String,
    /// The commit the borrower is building, same purpose as `worktree`.
    pub head: String,
}

impl Owner {
    /// An owner for the current process.
    #[must_use]
    pub fn here(run: &str, node: &str, seat: &str, worktree: &Path, head: &str) -> Owner {
        Owner {
            run: run.to_owned(),
            node: node.to_owned(),
            seat: seat.to_owned(),
            pid: std::process::id(),
            worktree: worktree.display().to_string(),
            head: head.to_owned(),
        }
    }
}

/// One held lease, as written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseFile {
    /// The literal cache path, kept alongside the owner so [`inventory`] can
    /// report it without reversing the filename — the filename is a lossy
    /// slug, not the path itself.
    cache_dir: String,
    owner: Owner,
    acquired_at: Timestamp,
}

/// Where a cache directory's classification lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No lease file, or one whose owner is confirmed gone.
    Free,
    /// A live owner holds it.
    Active(Owner),
    /// A lease file names an owner whose pid is confirmed gone. Reclaimable.
    Stale(Owner),
    /// A lease file exists but could not be trusted — unreadable or
    /// unparseable. Never reclaimed automatically; a human decides.
    Unknown,
}

/// Why [`try_acquire`] did not hand back a [`Guard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Busy {
    /// Held by a confirmed-live owner.
    Active(Owner),
    /// A lease is present but could not be trusted.
    Unknown,
    /// Lost a race with another acquirer; the caller should just try again.
    Contended,
}

impl Busy {
    /// One line for an event log or an error message.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Busy::Active(o) => format!(
                "held by run {} node {} seat {} (pid {})",
                o.run, o.node, o.seat, o.pid
            ),
            Busy::Unknown => {
                "an unreadable lease is present; refusing to guess who holds it".to_owned()
            }
            Busy::Contended => "lost a race for the lease; retrying".to_owned(),
        }
    }
}

/// A held lease. Releases on drop, so a panicking or early-returning caller
/// never leaves the cache permanently marked busy — the same guarantee
/// [`crate::queue::Claim`] gives the task queue's own lock file.
#[derive(Debug)]
pub struct Guard {
    path: PathBuf,
    released: bool,
}

impl Guard {
    fn new(path: PathBuf) -> Guard {
        Guard {
            path,
            released: false,
        }
    }

    /// Release explicitly. Equivalent to dropping the guard; spelled out for
    /// call sites where "the build finished, release now" reads better than
    /// waiting for scope exit.
    pub fn release(mut self) {
        self.do_release();
    }

    fn do_release(&mut self) {
        if !self.released {
            let _ = std::fs::remove_file(&self.path);
            self.released = true;
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.do_release();
    }
}

/// The directory leases live under, inside the magi home — never inside the
/// cache directory itself, so [`crate::disk::prune_dir`]'s oldest-first sweep
/// of the cache can never delete a lease file it does not know exists.
fn leases_dir(home: &Path) -> PathBuf {
    home.join("cache-leases")
}

/// A stable, filesystem-safe name for one cache directory's lease file.
/// Human-legible prefix (so a directory listing is self-explanatory) plus a
/// hash suffix (so two paths that sanitize to the same prefix — unlikely,
/// but not impossible on a long path — never collide).
fn slug(cache_dir: &Path) -> String {
    let norm = normalize(cache_dir);
    let mut readable: String = norm
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    readable.truncate(80);
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    norm.hash(&mut hasher);
    format!("{readable}-{:08x}", hasher.finish() as u32)
}

/// Best-effort canonical form of a cache path, for hashing only — never
/// shown to a human, who gets the literal path out of the lease file's own
/// `cache_dir` field instead. Falls back to the literal string when the
/// directory does not exist yet, so a lease can be taken before the first
/// build creates it.
fn normalize(p: &Path) -> String {
    std::fs::canonicalize(p)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn lease_path(home: &Path, cache_dir: &Path) -> PathBuf {
    leases_dir(home).join(format!("{}.json", slug(cache_dir)))
}

fn identity_path(home: &Path, cache_dir: &Path) -> PathBuf {
    leases_dir(home).join(format!("{}.identity.json", slug(cache_dir)))
}

/// Read whatever JSON is at `path` as a [`LeaseFile`], or `None` when it is
/// missing or does not parse.
fn read_lease(path: &Path) -> Option<LeaseFile> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Best-effort read of just the `cache_dir` field, for [`inventory`] to
/// report a path even when the rest of the record — the owner — does not
/// parse. A corrupt lease is still evidence of *which* directory is in a
/// state nobody can vouch for.
fn peek_cache_dir(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    value
        .get("cache_dir")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Classify the lease at `path`, using the real process-liveness query.
fn classify(path: &Path) -> Status {
    classify_with(path, proc::pid_alive)
}

/// [`classify`] with its process-liveness query supplied by the caller —
/// mirrors [`crate::daemon::sweep_stale_claims_with`], which exists for the
/// identical reason: a real "confirmed dead" pid cannot be produced portably
/// from a test (an out-of-range value reads as *unavailable*, not dead, to
/// `tasklist`/`kill -0`, and the conservative policy those already apply -
/// correctly - treats an unavailable query as alive). Production code always
/// goes through [`classify`]; this is what a test calls directly to assert
/// the classification logic against an injected answer instead.
fn classify_with<F: Fn(u32) -> bool>(path: &Path, alive: F) -> Status {
    if !path.exists() {
        return Status::Free;
    }
    let Some(lease) = read_lease(path) else {
        return Status::Unknown;
    };
    let this_process = std::process::id();
    if lease.owner.pid == this_process || alive(lease.owner.pid) {
        Status::Active(lease.owner)
    } else {
        Status::Stale(lease.owner)
    }
}

/// Write a brand-new lease file, atomically — `create_new` refuses to
/// overwrite an existing one, exactly like [`crate::queue::Queue::claim`]'s
/// task lock, which this mirrors on purpose rather than inventing a second
/// exclusion primitive in the same codebase.
fn write_new(path: &Path, cache_dir: &Path, owner: &Owner) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let lease = LeaseFile {
        cache_dir: cache_dir.display().to_string(),
        owner: owner.clone(),
        acquired_at: Timestamp::now(),
    };
    let body = serde_json::to_string_pretty(&lease).unwrap_or_default();
    f.write_all(body.as_bytes())?;
    Ok(())
}

/// What [`try_acquire`] returned.
pub enum AcquireOutcome {
    /// The lease is now held by the caller.
    Acquired(Guard),
    /// Somebody else has it, or its state could not be trusted.
    Busy(Busy),
}

/// Take the lease for `cache_dir` if nobody live holds it, reclaiming a
/// stale one first. One attempt — a caller that wants to wait uses
/// [`wait_for`], which is this in a loop bounded by a budget.
pub fn try_acquire(home: &Path, cache_dir: &Path, owner: &Owner) -> Result<AcquireOutcome> {
    try_acquire_with(home, cache_dir, owner, proc::pid_alive)
}

/// [`try_acquire`] with its process-liveness query supplied by the caller —
/// see [`classify_with`] for why this split exists.
fn try_acquire_with<F: Fn(u32) -> bool + Copy>(
    home: &Path,
    cache_dir: &Path,
    owner: &Owner,
    alive: F,
) -> Result<AcquireOutcome> {
    let dir = leases_dir(home);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.json", slug(cache_dir)));

    // Two attempts: the first is the common case (free, or held by someone
    // live); the second only runs after reclaiming a confirmed-stale lease,
    // so this can never loop more than once per call — a caller that keeps
    // losing the race to genuinely live contenders is exactly what `Busy`
    // reports back, not something this function spins on.
    for _ in 0..2 {
        match write_new(&path, cache_dir, owner) {
            Ok(()) => return Ok(AcquireOutcome::Acquired(Guard::new(path))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
        match classify_with(&path, alive) {
            Status::Free => {} // reclaimed between the write and the read; loop once more
            Status::Stale(_) => {
                let _ = std::fs::remove_file(&path);
            }
            Status::Active(o) => return Ok(AcquireOutcome::Busy(Busy::Active(o))),
            Status::Unknown => return Ok(AcquireOutcome::Busy(Busy::Unknown)),
        }
    }
    Ok(AcquireOutcome::Busy(Busy::Contended))
}

/// Is `cache_dir` in use right now — held by a live owner, or in a state
/// nobody can vouch for? The conservative half of the janitor's prune guard:
/// [`Status::Unknown`] refuses exactly like [`Status::Active`], because a
/// lease this process cannot read is not evidence the directory is free.
#[must_use]
pub fn in_use(home: &Path, cache_dir: &Path) -> bool {
    let path = lease_path(home, cache_dir);
    matches!(classify(&path), Status::Active(_) | Status::Unknown)
}

/// Acquire the lease for `cache_dir`, waiting out contention rather than
/// failing on the first busy owner — but never past `budget`, and the wait
/// comes out of that same budget rather than a second, unbounded one. A
/// caller already has a node timeout; this is that timeout, not a new clock
/// next to it, which is what keeps a wait from becoming the "無期限待機"
/// AGENTS.md's own build-cache section warns is never acceptable. The error
/// on timeout names who is still holding it, for the event this gets logged
/// into.
pub async fn wait_for(
    home: &Path,
    cache_dir: &Path,
    owner: &Owner,
    budget: Duration,
    poll: Duration,
) -> Result<Guard> {
    let start = std::time::Instant::now();
    loop {
        match try_acquire(home, cache_dir, owner)? {
            AcquireOutcome::Acquired(g) => return Ok(g),
            AcquireOutcome::Busy(busy) => {
                let elapsed = start.elapsed();
                if elapsed >= budget {
                    bail!(
                        "timed out after {}s waiting for the build cache at {} ({})",
                        budget.as_secs(),
                        cache_dir.display(),
                        busy.describe()
                    );
                }
                tokio::time::sleep(poll.min(budget - elapsed)).await;
            }
        }
    }
}

/// One cache directory's state, for `a0fc`'s capacity/cleanup inventory (and
/// `magi cache show`, eventually). Every lease file on disk is reported —
/// including ones this process itself does not want to touch — because the
/// question this answers is "what is registered right now", not "what can I
/// safely act on".
#[derive(Debug, Clone)]
pub struct Entry {
    /// The literal cache path this entry describes.
    pub cache_dir: String,
    /// What [`classify`] made of the lease registered against it.
    pub status: EntryStatus,
}

/// [`Entry::status`]'s possible values.
#[derive(Debug, Clone)]
pub enum EntryStatus {
    /// Held by a confirmed-live owner.
    Active(Owner),
    /// Present, but the owner is confirmed gone — safe to reclaim.
    Stale(Owner),
    /// Present, but unreadable — never assume safe to reclaim.
    Unknown,
}

/// Every registered lease under `home`, active or not. Free directories
/// (nothing registered, or already reclaimed) are not entries here — there
/// is nothing for `a0fc` to reason about in their absence.
#[must_use]
pub fn inventory(home: &Path) -> Vec<Entry> {
    inventory_with(home, proc::pid_alive)
}

/// [`inventory`] with its process-liveness query supplied by the caller —
/// see [`classify_with`] for why this split exists.
fn inventory_with<F: Fn(u32) -> bool + Copy>(home: &Path, alive: F) -> Vec<Entry> {
    let dir = leases_dir(home);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json")
            || path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".identity.json"))
        {
            continue;
        }
        let status = match classify_with(&path, alive) {
            Status::Free => continue,
            Status::Active(o) => EntryStatus::Active(o),
            Status::Stale(o) => EntryStatus::Stale(o),
            Status::Unknown => EntryStatus::Unknown,
        };
        // A lease so corrupted that not even its `cache_dir` field can be
        // read still has to be findable: the lease file's own name is a
        // one-way slug of the path it was for, so the file itself - not the
        // path it can no longer name - is what a human or `a0fc` gets
        // pointed at.
        let cache_dir = peek_cache_dir(&path)
            .unwrap_or_else(|| format!("(unreadable lease file: {})", path.display()));
        out.push(Entry { cache_dir, status });
    }
    out.sort_by(|a, b| a.cache_dir.cmp(&b.cache_dir));
    out
}

/// Prune `cache_dir` to `limit`, but only while holding the lease — a prune
/// that raced a live build would delete files a compile in flight still
/// needs, on top of confusing whatever built them about why its own output
/// vanished. `Ok(None)` when the cache is in use right now; that is not an
/// error, it is the janitor's next pass catching it once the borrower is
/// done. Never touches a lease file itself — they live outside `cache_dir`
/// (see [`leases_dir`]), so [`crate::disk::prune_dir`]'s own sweep can never
/// reach one.
pub fn maintenance_prune(
    home: &Path,
    cache_dir: &Path,
    limit: u64,
) -> Result<Option<crate::disk::Prune>> {
    let owner = Owner {
        run: "maintenance".to_owned(),
        node: "prune".to_owned(),
        seat: "janitor".to_owned(),
        pid: std::process::id(),
        worktree: String::new(),
        head: String::new(),
    };
    match try_acquire(home, cache_dir, &owner)? {
        AcquireOutcome::Busy(_) => Ok(None),
        AcquireOutcome::Acquired(guard) => {
            let result = crate::disk::prune_dir(cache_dir, limit)?;
            guard.release();
            Ok(Some(result))
        }
    }
}

/// The source a build against a cache directory was last known to come from.
/// Compared by [`needs_refresh`] on every acquire, so a cache directory that
/// only ever sees one worktree/head pair never pays a clean it does not need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The worktree the build ran from.
    pub worktree: String,
    /// The commit it built.
    pub head: String,
}

impl Identity {
    /// Build an identity from a worktree path and the commit it is at.
    #[must_use]
    pub fn new(worktree: &Path, head: &str) -> Identity {
        Identity {
            worktree: worktree.display().to_string(),
            head: head.to_owned(),
        }
    }
}

/// Has the source building against `cache_dir` changed since the last record?
/// True (needs a refresh) whenever nothing was ever recorded — the
/// conservative default for a cache directory this process has not tracked
/// before.
#[must_use]
pub fn needs_refresh(home: &Path, cache_dir: &Path, current: &Identity) -> bool {
    let path = identity_path(home, cache_dir);
    let Ok(body) = std::fs::read_to_string(path) else {
        return true;
    };
    match serde_json::from_str::<Identity>(&body) {
        Ok(recorded) => &recorded != current,
        Err(_) => true,
    }
}

/// Persist `identity` as the last known source for `cache_dir`.
pub fn record_identity(home: &Path, cache_dir: &Path, identity: &Identity) -> Result<()> {
    let path = identity_path(home, cache_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(identity).context("serialize cache identity")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Forget the recorded source identity for `cache_dir`, so the next
/// [`ensure_fresh`] call cannot skip its clean on the strength of a stale
/// match.
///
/// For a builder that does not itself have one coherent (worktree, head) to
/// record - an implement or fix wave, where several different worktrees
/// deliberately share the cache concurrently - there is nothing correct to
/// write in place of the old identity. Removing the record is still correct:
/// it costs the next tracked caller ([`ensure_fresh`] at `e2e`/`gate`) one
/// clean it might not have strictly needed, in exchange for never trusting a
/// match against a write this module never observed. Best-effort: a stale
/// record surviving a failed removal is no worse than the record this
/// replaces.
pub fn invalidate_identity(home: &Path, cache_dir: &Path) {
    let _ = std::fs::remove_file(identity_path(home, cache_dir));
}

/// Parse `cargo metadata --no-deps`'s JSON for the names of packages defined
/// in the workspace itself — never a dependency, which is exactly the
/// distinction that keeps a freshness fix from also discarding a downloaded
/// crate's compiled artifacts on every worktree switch. Pure, so the parse
/// is tested against fixture text without a `cargo` on the test machine.
#[must_use]
pub fn parse_workspace_package_names(metadata_json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata_json) else {
        return Vec::new();
    };
    value
        .get("packages")
        .and_then(|p| p.as_array())
        .map(|packages| {
            packages
                .iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Selectively invalidate the workspace's own compiled artifacts in
/// `cache_dir` — `cargo clean -p <name> --target-dir <cache_dir>` for every
/// package `cargo metadata` reports as local to `worktree`, never a bare
/// `cargo clean` (which would throw away every dependency's compile too).
/// Real subprocess execution: never called from a test, only from
/// [`ensure_fresh`] in the running binary.
fn refresh_stale_packages(worktree: &Path, cache_dir: &Path) -> Result<Vec<String>> {
    let meta = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(worktree)
        .quiet()
        .output()
        .context("run `cargo metadata`")?;
    if !meta.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&meta.stderr)
        );
    }
    let names = parse_workspace_package_names(&String::from_utf8_lossy(&meta.stdout));
    for name in &names {
        let out = std::process::Command::new("cargo")
            .arg("clean")
            .arg("-p")
            .arg(name)
            .arg("--target-dir")
            .arg(cache_dir)
            .current_dir(worktree)
            .quiet()
            .output()
            .with_context(|| format!("cargo clean -p {name}"))?;
        if !out.status.success() {
            tracing::warn!(
                package = %name,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "build cache: cargo clean -p failed; leaving its artifacts as-is"
            );
        }
    }
    Ok(names)
}

/// Guarantee that a build against `cache_dir` from `(worktree, head)` never
/// silently reuses another source's compiled output: clean the workspace's
/// own packages out of the cache when the recorded identity disagrees, then
/// record the new one. A no-op — no subprocess spawned — when the identity
/// already matches, which is the common case once a cache directory settles
/// on one worktree for a while.
///
/// Called with the lease already held: this is a mutation of the cache
/// directory's contents, and it must never race a concurrent build the same
/// way a plain `cargo clean` run by hand would not.
pub fn ensure_fresh(home: &Path, cache_dir: &Path, identity: &Identity) -> Result<()> {
    if needs_refresh(home, cache_dir, identity) {
        let cleaned = refresh_stale_packages(&PathBuf::from(&identity.worktree), cache_dir)?;
        tracing::info!(
            ?cleaned,
            cache = %cache_dir.display(),
            "build cache: source identity changed; cleaned the workspace's own packages before reuse"
        );
    }
    record_identity(home, cache_dir, identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(pid: u32) -> Owner {
        Owner {
            run: "r1".to_owned(),
            node: "gate".to_owned(),
            seat: "gate".to_owned(),
            pid,
            worktree: "/w".to_owned(),
            head: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn an_uncontended_lease_is_acquired_and_freed_on_release() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        let this = std::process::id();
        match try_acquire(home.path(), &cache, &owner(this)).expect("acquire") {
            AcquireOutcome::Acquired(g) => {
                assert!(in_use(home.path(), &cache), "held while the guard lives");
                g.release();
            }
            AcquireOutcome::Busy(b) => panic!("unexpectedly busy: {b:?}"),
        }
        assert!(!in_use(home.path(), &cache), "freed after release");
    }

    #[test]
    fn a_lease_held_by_a_live_pid_is_reported_active_and_refuses_a_second_acquire() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        let this = std::process::id();
        // Acquire as one owner, then try again as a different one — same
        // live pid (this test process), different run/seat, which is exactly
        // "another owner, still alive" without needing to fork a process.
        let _first =
            try_acquire(home.path(), &cache, &owner(this)).expect("first acquire succeeds");
        let mut second_owner = owner(this);
        second_owner.run = "r2".to_owned();
        match try_acquire(home.path(), &cache, &second_owner).expect("no io error") {
            AcquireOutcome::Busy(Busy::Active(held_by)) => assert_eq!(held_by.run, "r1"),
            other => panic!("expected Busy::Active, got a different outcome: {other:?}"),
        }
        assert!(in_use(home.path(), &cache));
    }

    impl std::fmt::Debug for AcquireOutcome {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                AcquireOutcome::Acquired(_) => write!(f, "Acquired"),
                AcquireOutcome::Busy(b) => write!(f, "Busy({b:?})"),
            }
        }
    }

    #[test]
    fn a_lease_whose_pid_is_gone_is_stale_and_reclaimed_by_the_next_acquirer() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        // Liveness is injected rather than asked of the real OS - a "known
        // dead" pid cannot be produced portably (see `classify_with`'s doc):
        // an out-of-range value reads as *unavailable* to `tasklist`, not
        // dead, and the conservative policy then reports it alive, which is
        // exactly what made this test flaky before this fix.
        let dead_owner = owner(999_999);
        let path = lease_path(home.path(), &cache);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_new(&path, &cache, &dead_owner).expect("seed a stale lease");
        assert_eq!(
            classify_with(&path, |_| false),
            Status::Stale(dead_owner.clone())
        );

        match try_acquire_with(home.path(), &cache, &owner(std::process::id()), |_| false)
            .expect("acquire")
        {
            AcquireOutcome::Acquired(_) => {}
            other => panic!("stale lease should have been reclaimed: {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_lease_is_unknown_and_never_reclaimed() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        let path = lease_path(home.path(), &cache);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(classify(&path), Status::Unknown);
        assert!(in_use(home.path(), &cache), "unknown counts as in use");
        match try_acquire(home.path(), &cache, &owner(std::process::id())).expect("no io error") {
            AcquireOutcome::Busy(Busy::Unknown) => {}
            other => panic!("expected Busy::Unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn waiting_for_a_busy_lease_times_out_within_its_own_budget() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        let _held = try_acquire(home.path(), &cache, &owner(std::process::id()))
            .expect("acquire")
            .pipe();
        let mut waiter = owner(std::process::id());
        waiter.run = "r2".to_owned();
        let started = std::time::Instant::now();
        let err = wait_for(
            home.path(),
            &cache,
            &waiter,
            Duration::from_millis(150),
            Duration::from_millis(20),
        )
        .await
        .expect_err("still held, must time out");
        assert!(started.elapsed() < Duration::from_secs(2), "bounded wait");
        assert!(
            err.to_string().contains("r1"),
            "names the current holder: {err}"
        );
    }

    #[tokio::test]
    async fn a_wait_succeeds_as_soon_as_the_lease_is_released() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        let guard =
            match try_acquire(home.path(), &cache, &owner(std::process::id())).expect("acquire") {
                AcquireOutcome::Acquired(g) => g,
                AcquireOutcome::Busy(b) => panic!("unexpectedly busy: {b:?}"),
            };
        let home_path = home.path().to_path_buf();
        let cache_path = cache.clone();
        let mut waiter = owner(std::process::id());
        waiter.run = "r2".to_owned();
        let wait = tokio::spawn(async move {
            wait_for(
                &home_path,
                &cache_path,
                &waiter,
                Duration::from_secs(5),
                Duration::from_millis(10),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        guard.release();
        let acquired = wait.await.expect("task").expect("acquire after release");
        acquired.release();
    }

    #[test]
    fn inventory_reports_active_stale_and_unknown_but_not_free() {
        let home = tempfile::TempDir::new().expect("temp");
        let active_cache = home.path().join("active");
        let stale_cache = home.path().join("stale");
        let unknown_cache = home.path().join("unknown");

        let _held =
            try_acquire(home.path(), &active_cache, &owner(std::process::id())).expect("acquire");
        let stale_path = lease_path(home.path(), &stale_cache);
        std::fs::create_dir_all(stale_path.parent().unwrap()).unwrap();
        write_new(&stale_path, &stale_cache, &owner(999_999)).unwrap();
        let unknown_path = lease_path(home.path(), &unknown_cache);
        std::fs::write(&unknown_path, b"garbage").unwrap();

        // Liveness injected as `false` for everyone but this test process -
        // see `a_lease_whose_pid_is_gone_is_stale_and_reclaimed_by_the_next_acquirer`
        // for why the real OS query cannot portably produce a "confirmed
        // dead" answer.
        let entries = inventory_with(home.path(), |pid| pid == std::process::id());
        assert_eq!(entries.len(), 3, "{entries:?}");
        let by_dir = |dir: &Path| {
            entries
                .iter()
                .find(|e| e.cache_dir == dir.display().to_string())
                .unwrap_or_else(|| panic!("no entry for {}", dir.display()))
        };
        assert!(matches!(
            by_dir(&active_cache).status,
            EntryStatus::Active(_)
        ));
        assert!(matches!(by_dir(&stale_cache).status, EntryStatus::Stale(_)));
        // A lease this corrupted cannot name its own `cache_dir` - the point
        // of this third case - so it is found by status instead of by path,
        // and its reported path must still point somewhere a human can act
        // on: the lease file itself.
        let unknown = entries
            .iter()
            .find(|e| matches!(e.status, EntryStatus::Unknown))
            .unwrap_or_else(|| panic!("no Unknown entry: {entries:?}"));
        assert!(
            unknown
                .cache_dir
                .contains(&unknown_path.display().to_string()),
            "{unknown:?}"
        );
    }

    #[test]
    fn maintenance_prune_refuses_a_cache_a_live_owner_holds() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("big"), vec![0u8; 100]).unwrap();
        let _held = try_acquire(home.path(), &cache, &owner(std::process::id())).expect("acquire");

        let result = maintenance_prune(home.path(), &cache, 1).expect("no io error");
        assert!(
            result.is_none(),
            "must not prune while a live owner holds it"
        );
        assert!(cache.join("big").exists(), "nothing was deleted");
    }

    #[test]
    fn maintenance_prune_acts_once_the_cache_is_free_and_releases_after() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("big"), vec![0u8; 100]).unwrap();

        let pruned = maintenance_prune(home.path(), &cache, 1)
            .expect("no io error")
            .expect("cache was free");
        assert!(pruned.freed > 0);
        assert!(
            !in_use(home.path(), &cache),
            "the maintenance lease was released"
        );
    }

    #[test]
    fn identity_drift_is_detected_once_and_then_settles() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        let a = Identity {
            worktree: "/w/a".to_owned(),
            head: "aaaa".to_owned(),
        };
        let b = Identity {
            worktree: "/w/b".to_owned(),
            head: "bbbb".to_owned(),
        };
        assert!(
            needs_refresh(home.path(), &cache, &a),
            "nothing recorded yet"
        );
        record_identity(home.path(), &cache, &a).expect("record");
        assert!(
            !needs_refresh(home.path(), &cache, &a),
            "same identity, no refresh needed"
        );
        assert!(needs_refresh(home.path(), &cache, &b), "different source");
        record_identity(home.path(), &cache, &b).expect("record");
        assert!(!needs_refresh(home.path(), &cache, &b));
    }

    #[test]
    fn invalidating_forgets_a_recorded_identity_so_the_next_check_refreshes() {
        let home = tempfile::TempDir::new().expect("temp");
        let cache = home.path().join("cache");
        let a = Identity {
            worktree: "/w/a".to_owned(),
            head: "aaaa".to_owned(),
        };
        record_identity(home.path(), &cache, &a).expect("record");
        assert!(!needs_refresh(home.path(), &cache, &a));

        // An untracked writer (an implement/fix wave, which shares the cache
        // across several worktrees at once and so has no single identity of
        // its own to record) touched the cache in between; the next tracked
        // caller must not trust the old match anymore.
        invalidate_identity(home.path(), &cache);
        assert!(
            needs_refresh(home.path(), &cache, &a),
            "invalidation must not be skippable by asking about the same identity again"
        );

        // Invalidating a cache directory nothing ever recorded is a no-op,
        // not an error.
        invalidate_identity(home.path(), &home.path().join("never-recorded"));
    }

    #[test]
    fn workspace_package_names_are_read_from_cargo_metadata_json() {
        let fixture = r#"{
            "packages": [
                {"name": "magi", "version": "0.1.0"},
                {"name": "magi-cli", "version": "0.1.0"}
            ],
            "workspace_members": []
        }"#;
        let mut names = parse_workspace_package_names(fixture);
        names.sort();
        assert_eq!(names, vec!["magi".to_owned(), "magi-cli".to_owned()]);
        assert_eq!(
            parse_workspace_package_names("not json"),
            Vec::<String>::new()
        );
        assert_eq!(parse_workspace_package_names("{}"), Vec::<String>::new());
    }

    #[test]
    fn slugs_are_stable_and_filesystem_safe() {
        let a = slug(Path::new(r"C:\Users\op\Temp\magi-target"));
        let b = slug(Path::new(r"C:\Users\op\Temp\magi-target"));
        assert_eq!(a, b, "same input, same slug");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "filesystem-safe: {a}"
        );
    }

    #[test]
    fn busy_active_describes_the_holder() {
        let b = Busy::Active(owner(123));
        let s = b.describe();
        assert!(
            s.contains("r1") && s.contains("gate") && s.contains("123"),
            "{s}"
        );
    }

    trait Pipe: Sized {
        fn pipe(self) -> Guard;
    }
    impl Pipe for AcquireOutcome {
        fn pipe(self) -> Guard {
            match self {
                AcquireOutcome::Acquired(g) => g,
                AcquireOutcome::Busy(b) => panic!("expected Acquired, got Busy({b:?})"),
            }
        }
    }
}
