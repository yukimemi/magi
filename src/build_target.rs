//! Isolated Cargo targets are allocated once per linked checkout generation.
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    cache,
    config::{BuildCache, Config},
    git,
    proc::Quiet as _,
    retention::OutputLease,
};

#[doc(hidden)]
pub mod command;

pub(crate) const TARGET_PROOF: &str = ".magi-isolated-owner";
const NAMESPACE: &str = "isolated-builds";
const ANCHOR: &str = "magi-isolated-build";
const ANCHOR_LOCK: &str = "magi-isolated-build.lock";
pub(crate) const KIND: &str = "isolated build cache";

/// Is `name`, in a linked checkout's Git administration directory, one of the
/// files this module puts there: the target anchor, its lock, or the staging
/// file an interrupted allocation leaves behind? Retention inventories that
/// directory and refuses anything it does not know.
pub(crate) fn admin_artifact(name: &str) -> bool {
    name == ANCHOR
        || name == ANCHOR_LOCK
        || name
            .strip_prefix(ANCHOR)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|rest| rest.ends_with(".tmp"))
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Ownership {
    worktree: PathBuf,
    admin: PathBuf,
}

pub(crate) fn isolated(cfg: &Config) -> bool {
    cfg.verify.build_cache == BuildCache::Isolated
}

/// Whether this repository's verification can run with isolated outputs: a
/// POSIX shell and commands inside the grammar `command::normalize` accepts.
pub(crate) fn supports(cfg: &Config) -> Result<()> {
    command::check_shell(&cfg.shell())?;
    for cmd in cfg
        .verify
        .pre_gate
        .iter()
        .chain(&cfg.verify.e2e)
        .chain(&cfg.verify.gate)
    {
        command::normalize(cmd)
            .with_context(|| format!("unsupported isolated verification command: {cmd}"))?;
    }
    Ok(())
}

/// Should an urgent task take isolated outputs by default here? Only when
/// there is a shared cache to wait behind and every command isolated mode
/// would have to run fits it: verification and `[worktree] setup` commands.
/// An urgent task must never be the one that stops on a command isolated mode
/// cannot run; an explicit `isolated` stays strict and says so.
pub(crate) fn urgent_isolation(cfg: &Config) -> bool {
    cfg.verify.cache_dir().is_some()
        && supports(cfg).is_ok()
        && cfg
            .worktree
            .setup
            .iter()
            .filter_map(|step| step.run.as_deref())
            .all(|cmd| command::normalize(cmd).is_ok())
}

pub(crate) fn root(home: &Path) -> PathBuf {
    home.join(NAMESPACE)
}

fn plain(path: &Path, dir: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        ensure!(
            meta.file_attributes() & 0x400 == 0,
            "reparse point in build ownership: {}",
            path.display()
        );
    }
    ensure!(
        (if dir { meta.is_dir() } else { meta.is_file() }) && path.canonicalize()? == path,
        "noncanonical build ownership path: {}",
        path.display()
    );
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn namespace(home: &Path) -> Result<PathBuf> {
    let dir = root(home);
    fs::create_dir_all(&dir)?;
    plain(&dir, true)?;
    Ok(dir)
}

fn read_owner(dir: &Path) -> Result<Ownership> {
    plain(dir, true)?;
    let path = dir.join("owner.json");
    plain(&path, false)?;
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn checkout(cwd: &Path) -> Result<(Ownership, PathBuf)> {
    let worktree = cwd.canonicalize()?;
    plain(&worktree.join(".git"), false)
        .context("isolated builds require a linked Git worktree")?;
    let rev_parse = |arg| -> Result<PathBuf> {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", arg])
            .current_dir(&worktree)
            .quiet()
            .output()?;
        ensure!(
            out.status.success(),
            "could not identify linked worktree: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        git::canonical_output_path(String::from_utf8(out.stdout)?.trim_end_matches(['\r', '\n']))
    };
    ensure!(
        rev_parse("--show-toplevel")? == worktree,
        "build source is not the checkout root"
    );
    let admin = rev_parse("--absolute-git-dir")?;
    let common = rev_parse("--git-common-dir")?;
    ensure!(
        admin.parent() == Some(common.join("worktrees").as_path()),
        "unregistered Git worktree"
    );
    let back = admin.join("gitdir");
    plain(&back, false)?;
    let back = fs::read_to_string(back)?;
    let back = back.trim_end_matches(['\r', '\n']);
    let backlink = if Path::new(back).is_relative() {
        // Git writes this path relative to the worktree administration directory.
        admin.join(back).canonicalize()?
    } else {
        git::canonical_output_path(back)?
    };
    ensure!(
        backlink == worktree.join(".git"),
        "Git worktree registration does not point back to its source"
    );
    Ok((Ownership { worktree, admin }, common))
}

pub(crate) fn resolve(home: &Path, cwd: &Path) -> Result<PathBuf> {
    let (owner, common) = checkout(cwd)?;
    let home = home.canonicalize()?;
    ensure!(
        !home.starts_with(&owner.worktree)
            && !home.starts_with(&common)
            && !(common.file_name().is_some_and(|name| name == ".git")
                && common.parent().is_some_and(|repo| home.starts_with(repo))),
        "isolated outputs must be outside the source repository"
    );
    let base = namespace(&home)?;
    let anchor = owner.admin.join(ANCHOR);
    let _lock = OutputLease::lock(&owner.admin.join(ANCHOR_LOCK), true)?;
    let id = match fs::symlink_metadata(&anchor) {
        Ok(_) => {
            plain(&anchor, false)?;
            fs::read_to_string(&anchor)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let id = crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4();
            let dir = base.join(&id);
            fs::create_dir(&dir)?;
            fs::write(dir.join("owner.json"), serde_json::to_vec(&owner)?)?;
            let target = dir.join("target");
            fs::create_dir(&target)?;
            fs::write(target.join(TARGET_PROOF), &id)?;
            // Publish only a complete generation ID. A killed allocator can
            // leave staging debris, but never a truncated live anchor.
            let staged = owner.admin.join(format!("{ANCHOR}-{id}.tmp"));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            let written = file.write_all(id.as_bytes()).and_then(|()| file.sync_all());
            drop(file);
            if let Err(error) = written.and_then(|()| fs::rename(&staged, &anchor)) {
                let _ = fs::remove_file(&staged);
                return Err(error.into());
            }
            id
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(valid_id(&id), "invalid isolated build generation");
    let dir = base.join(&id);
    ensure!(
        read_owner(&dir)? == owner,
        "isolated target belongs to another checkout"
    );
    let target = dir.join("target");
    check_proof(&target, &id)?;
    Ok(target)
}

pub(crate) struct FoldTarget {
    owner: Ownership,
    target: PathBuf,
}

impl FoldTarget {
    /// Snapshot the existing binding before Git removes its administrative anchor.
    pub(crate) fn for_checkout(home: &Path, cwd: &Path, run_root: &Path) -> Result<Option<Self>> {
        if !cwd.exists() {
            return Ok(None);
        }
        let (owner, _) = checkout(cwd)?;
        ensure!(
            owner.worktree.parent() == Some(run_root.canonicalize()?.as_path()),
            "isolated target source is outside the folding run"
        );
        let anchor = owner.admin.join(ANCHOR);
        match fs::symlink_metadata(&anchor) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            result => {
                result?;
            }
        }
        plain(&anchor, false)?;
        let id = fs::read_to_string(&anchor)?;
        ensure!(valid_id(&id), "invalid isolated build generation");
        let dir = root(&home.canonicalize()?).join(id);
        ensure!(
            read_owner(&dir)? == owner,
            "isolated target belongs to another checkout"
        );
        Ok(Some(Self {
            owner,
            target: dir.join("target"),
        }))
    }

    pub(crate) fn reclaim_orphaned_target(self, home: &Path) -> Result<()> {
        for path in [&self.owner.worktree, &self.owner.admin] {
            ensure!(
                matches!(fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "isolated target source or Git registration still exists"
            );
        }
        ensure!(check_prune(home, &self.target)?, "target is not registered");
        let dir = self.target.parent().context("missing target generation")?;
        ensure!(
            read_owner(dir)? == self.owner,
            "isolated target owner changed"
        );
        // Preserve the ownership marker and catalog, like ordinary maintenance.
        cache::maintenance_prune(home, &self.target, 0)?;
        Ok(())
    }
}

fn check_proof(target: &Path, id: &str) -> Result<()> {
    plain(target, true)?;
    let proof = target.join(TARGET_PROOF);
    plain(&proof, false)?;
    ensure!(
        fs::read_to_string(proof)? == id,
        "unregistered isolated target contents"
    );
    Ok(())
}

/// Validate isolated-target deletion authority independently of the cache catalog.
#[doc(hidden)]
pub fn check_prune(home: &Path, target: &Path) -> Result<bool> {
    let base = match home.canonicalize() {
        Ok(home) => root(&home),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let candidate = target.canonicalize().unwrap_or_else(|_| target.to_owned());
    if !candidate.starts_with(&base) && !target.starts_with(root(home)) {
        return Ok(false);
    }
    let dir = candidate
        .parent()
        .context("isolated target has no parent")?;
    let id = dir
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|id| valid_id(id))
        .context("not a registered isolated build generation")?;
    ensure!(
        dir.parent() == Some(base.as_path())
            && candidate.file_name().is_some_and(|n| n == "target"),
        "not a registered isolated target"
    );
    plain(&base, true)?;
    ensure!(
        git::canonical_output_path(&git::path_arg(target))? == candidate,
        "not a canonical isolated target"
    );
    check_proof(&candidate, id)?;
    let owner = read_owner(dir)?;
    ensure!(
        owner.worktree.is_absolute() && owner.admin.is_absolute(),
        "invalid isolated build owner"
    );
    Ok(true)
}

pub(crate) fn generation(target: &Path) -> Option<&str> {
    let dir = target.parent()?;
    let id = dir.file_name()?.to_str()?;
    (target.file_name()? == "target" && dir.parent()?.file_name()? == NAMESPACE && valid_id(id))
        .then_some(id)
}

pub(crate) fn prune_registered(
    home: &Path,
    disk: &crate::config::Disk,
) -> Result<crate::disk::Prune> {
    prune_registered_with(home, disk, crate::disk::managed_free_bytes)
}

/// [`prune_registered`] with the free-space measurement of the isolated-build
/// volume supplied by the caller.
///
/// Free space below
/// [`Disk::reclaim_threshold`](crate::config::Disk::reclaim_threshold) - the
/// collection threshold, not the admission floor - shrinks the pool's limit by
/// the deficit ([`crate::disk::pressure_limit`]); the same ownership, lease
/// and proof checks decide what may go. While free space is at or above the
/// admission floor the pool also keeps a reserve of half its cap
/// ([`crate::disk::pressure_reserve`]). It is enforced against the pool's
/// aggregate accounting and where files are removed: each target is pruned with
/// the bound `its size - the aggregate bytes above the reserve`
/// ([`cache::maintenance_prune_keeping`]), so a file whose removal would take
/// the pool below it is skipped.
///
/// Like the shared cache this is estimated-deficit collection: one free-space
/// measurement, that many *logical* bytes removed oldest-first, no re-measuring
/// loop; the next pass continues. A pass that removes nothing is not an error
/// (a target whose lease is held is left alone). An unreadable measurement
/// leaves the pool untouched, as before the threshold existed.
///
/// Every pass logs one `info` line (see [`crate::disk::PressureOutcome`]).
pub(crate) fn prune_registered_with(
    home: &Path,
    disk: &crate::config::Disk,
    free_bytes: impl Fn(&Path) -> Result<u64>,
) -> Result<crate::disk::Prune> {
    let mut result = crate::disk::Prune::default();
    if disk.cache_limit_bytes == 0 || !root(home).is_dir() {
        return Ok(result);
    }
    let mut targets = Vec::new();
    for entry in cache::inventory(home) {
        let path = PathBuf::from(entry.cache_dir);
        match check_prune(home, &path) {
            Ok(true) => {
                let size = crate::disk::dir_size(&path);
                result.remaining = result.remaining.saturating_add(size);
                let time = fs::metadata(&path).and_then(|m| m.modified()).ok();
                targets.push((time, path, size));
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(target = %path.display(), %error, "isolated build prune refused")
            }
        }
    }
    let estimate = result.remaining;
    let cap = disk.cache_limit_bytes;
    let floor = disk.min_free_bytes;
    let threshold = disk.reclaim_threshold();
    let free = free_bytes(&root(home))?;
    let limit = crate::disk::pressure_limit(estimate, cap, floor, threshold, free);
    let keep = crate::disk::pressure_reserve(cap, floor, threshold, free);
    let candidates = targets.len();
    let (mut locked, mut locked_size, mut refused) = (0usize, 0u64, 0usize);
    targets.sort_unstable();
    for (_, target, size) in targets {
        let excess = result.remaining.saturating_sub(limit);
        if excess == 0 {
            break;
        }
        if size == 0 {
            continue;
        }
        // What this target may still give up without taking the pool below
        // its reserve; zero when there is no reserve.
        let target_keep = size.saturating_sub(result.remaining.saturating_sub(keep));
        match cache::maintenance_prune_keeping(
            home,
            &target,
            size.saturating_sub(excess),
            target_keep,
        ) {
            Ok(Some(pruned)) => {
                locked += 1;
                locked_size =
                    locked_size.saturating_add(pruned.remaining.saturating_add(pruned.freed));
                result.remaining = result.remaining.saturating_sub(pruned.freed);
                result.freed = result.freed.saturating_add(pruned.freed);
                result.files = result.files.saturating_add(pruned.files);
            }
            Ok(None) => refused += 1,
            Err(error) => {
                tracing::warn!(%error, target = %target.display(), "isolated build prune refused")
            }
        }
    }
    // The size a pass reports is the locked inventories', and only when every
    // target was inspected; the unlocked estimate is never repeated as one.
    let inventoried = (candidates > 0 && locked == candidates).then_some(locked_size);
    let outcome = crate::disk::pressure_outcome(
        // Partial progress is "collected"; only a pass that got nothing at
        // all because maintenance refused is "lease held".
        refused > 0 && result.freed == 0,
        result.freed,
        inventoried.unwrap_or(estimate),
        limit,
        keep,
    );
    tracing::info!(
        cache = %root(home).display(),
        admission_floor = floor,
        collection_threshold = threshold,
        free,
        size = inventoried,
        removed = result.freed,
        refused,
        outcome = outcome.as_str(),
        "isolated build pressure pass"
    );
    Ok(result)
}

pub(crate) async fn acquire(
    home: &Path,
    target: &Path,
    owner: &cache::Owner,
    config: &Config,
) -> Result<cache::Guard> {
    let wait = crate::repo_lock::WaitPolicy::current();
    let limit = Duration::from_secs(config.graph.timeout_resource_wait);
    let guard = match cache::try_acquire(home, target, owner)? {
        cache::AcquireOutcome::Acquired(guard) => guard,
        cache::AcquireOutcome::Busy(busy) => {
            tracing::info!(target = %target.display(), holder = %busy.describe(), "waiting for isolated build cache");
            cache::wait_for_unless(home, target, owner, limit, Duration::from_secs(1), &|| {
                wait.cancelled()
            })
            .await?
            .context("isolated build cache wait cancelled")?
        }
    };
    Ok(guard)
}

pub(crate) fn ensure_fresh(
    home: &Path,
    target: &Path,
    cwd: &Path,
    head: &str,
    config: &Config,
) -> Result<()> {
    cache::ensure_fresh_with(
        home,
        target,
        &cache::Identity::new(&cwd.canonicalize()?, head),
        &config.verify.cargo_manifests(),
        true,
    )
}

pub(crate) struct Operation {
    pub(crate) target: PathBuf,
    pub(crate) outputs: Vec<OutputLease>,
    pub(crate) lease: Option<crate::repo_lock::SeatLease>,
    _cache: cache::Guard,
}

#[derive(Clone, Copy)]
pub(crate) enum Coordination<'a> {
    Acquire,
    Held(Option<&'a crate::repo_lock::SeatLease>),
}

#[derive(Debug)]
pub(crate) struct AdmissionDiskWait;

impl std::fmt::Display for AdmissionDiskWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("disk admission")
    }
}

impl std::error::Error for AdmissionDiskWait {}

impl Operation {
    pub(crate) async fn admit(
        config: &Config,
        cwd: &Path,
        run: &str,
        node: &str,
        coordination: Coordination<'_>,
    ) -> Result<Self> {
        let home = crate::run::home();
        let target = resolve(&home, cwd)?;
        let head = git::rev_parse(cwd, "HEAD").await?;
        let owner = cache::Owner::here(run, node, node, cwd, &head);
        let (guard, lease) = match coordination {
            Coordination::Held(Some(lease)) => {
                // The usual order is cache then repository. Never wait on
                // the cache while holding the repository in the reverse order.
                let guard = match cache::try_acquire(&home, &target, &owner)? {
                    cache::AcquireOutcome::Acquired(guard) => guard,
                    cache::AcquireOutcome::Busy(busy) => {
                        return Err(crate::repo_lock::ResourceBlocked(format!(
                            "{KIND} at {}: {}",
                            target.display(),
                            busy.describe()
                        ))
                        .into());
                    }
                };
                (guard, Some(lease.share_operation()))
            }
            Coordination::Held(None) => (acquire(&home, &target, &owner, config).await?, None),
            Coordination::Acquire => {
                let guard = acquire(&home, &target, &owner, config).await?;
                let wait = crate::repo_lock::WaitPolicy::current();
                let lease = crate::repo_lock::SeatLease::operation_with(
                    cwd,
                    config.coordination.as_ref(),
                    &wait,
                )
                .await?;
                (guard, lease)
            }
        };
        let mut outputs = OutputLease::admit_isolated(
            &home,
            &[cwd.to_owned(), target.clone()],
            config.disk.min_free_bytes,
        )
        .map_err(|error| crate::repo_lock::ResourceBlocked(format!("disk admission: {error:#}")))
        .context(AdmissionDiskWait)?;
        outputs.push(OutputLease::cache(&home, &target, false)?);
        ensure_fresh(&home, &target, cwd, &head, config)?;
        // These commands can generate or edit sources without advancing HEAD.
        cache::invalidate_identity(&home, &target);
        Ok(Self {
            target,
            outputs,
            lease,
            _cache: guard,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_recognises_only_this_modules_admin_files() {
        for name in [
            ANCHOR,
            ANCHOR_LOCK,
            "magi-isolated-build-0b9c7f0e-6f3a-4c55-9d0a-0d2d8a0c1e11.tmp",
        ] {
            assert!(admin_artifact(name), "{name}");
        }
        for name in [
            "magi-isolated-build.bak",
            "magi-isolated-build-id",
            "magi-isolated-buildX.tmp",
            "magi-output.lock",
            "HEAD",
        ] {
            assert!(!admin_artifact(name), "{name}");
        }
    }

    fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// One registered, idle isolated target per entry of `sets`, each holding
    /// the given `(name, bytes, mtime)` files, under a private home and real
    /// linked worktrees of one repository.
    fn registered_targets(
        sets: &[&[(&str, usize, u64)]],
    ) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("f"), "x").unwrap();
        git(&repo, &["add", "f"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        let mut targets = Vec::new();
        for (i, files) in sets.iter().enumerate() {
            let worktree = root.join(format!("wt{i}"));
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    worktree.to_str().unwrap(),
                    "-b",
                    &format!("feat{i}"),
                ],
            );
            let target = resolve(&home, &worktree).expect("register the isolated target");
            let owner = cache::Owner::here("run", "node", "seat", &worktree, "head");
            match cache::try_acquire(&home, &target, &owner).unwrap() {
                cache::AcquireOutcome::Acquired(guard) => guard.release(),
                cache::AcquireOutcome::Busy(_) => panic!("fresh target is busy"),
            }
            for (name, bytes, secs) in *files {
                let file = target.join(name);
                fs::write(&file, vec![0u8; *bytes]).unwrap();
                fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(
                        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(*secs),
                    ))
                    .unwrap();
            }
            targets.push(target);
        }
        (tmp, home, targets)
    }

    /// One target of four 100-byte files, `f1` the oldest.
    fn registered_target() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let files: &[(&str, usize, u64)] = &[
            ("f1", 100, 1_000_001),
            ("f2", 100, 1_000_002),
            ("f3", 100, 1_000_003),
            ("f4", 100, 1_000_004),
        ];
        let (tmp, home, mut targets) = registered_targets(&[files]);
        (tmp, home, targets.remove(0))
    }

    /// A pool whose reserve (half the cap) is exactly `reserve`.
    fn pool(
        reserve: u64,
        min_free_bytes: u64,
        reclaim_free_bytes: Option<u64>,
    ) -> crate::config::Disk {
        crate::config::Disk {
            min_free_bytes,
            reclaim_free_bytes,
            cache_limit_bytes: 2 * reserve,
            ..crate::config::Disk::default()
        }
    }

    fn left(target: &Path) -> Vec<&'static str> {
        ["f1", "f2", "f3", "f4"]
            .into_iter()
            .filter(|name| target.join(name).exists())
            .collect()
    }

    #[test]
    fn a_pass_removes_the_estimated_deficit_between_the_floor_and_the_threshold() {
        let (_keep, home, target) = registered_target();
        let size = crate::disk::dir_size(&target);
        // Room for exactly one 100-byte file above the reserve (size - 150).
        let reserve = size - 150;

        // Threshold equal to the floor is the old rule: free 11 is above the
        // floor, so nothing is collected.
        let none = prune_registered_with(&home, &pool(reserve, 10, Some(10)), |_| Ok(11)).unwrap();
        assert_eq!(none.files, 0);
        assert_eq!(left(&target).len(), 4);

        // Threshold 30: a 19-byte deficit, taken from one free-space number
        // (the injected 11 never changes). The oldest file covers the deficit
        // and the rest stay; the ownership proof is never a candidate. On the
        // old rule this input collects nothing, so this assertion would fail.
        let some = prune_registered_with(&home, &pool(reserve, 10, Some(30)), |_| Ok(11)).unwrap();
        assert_eq!((some.files, some.freed), (1, 100));
        assert_eq!(left(&target), ["f2", "f3", "f4"]);
        assert!(target.join(TARGET_PROOF).exists());

        // Free at the threshold: left alone.
        let at = prune_registered_with(&home, &pool(reserve, 10, Some(30)), |_| Ok(30)).unwrap();
        assert_eq!(at.files, 0);

        // A zero cap still turns pruning off.
        let (_keep2, home2, target2) = registered_target();
        let mut off = pool(reserve, 10, Some(1_000_000));
        off.cache_limit_bytes = 0;
        let pass = prune_registered_with(&home2, &off, |_| Ok(0)).unwrap();
        assert_eq!(pass.files, 0);
        assert_eq!(left(&target2).len(), 4);
    }

    #[test]
    fn the_reserve_is_enforced_where_files_go_and_passes_converge() {
        let (_keep, home, target) = registered_target();
        let size = crate::disk::dir_size(&target);
        let reserve = size - 150;
        // The threshold is out of reach and free space is pinned at 11. The
        // oldest file goes (size - 100, still above the reserve); the next
        // removal would leave size - 200, below it, so it is skipped, as is
        // every later one - on every pass.
        let disk = pool(reserve, 10, Some(1_000_000));
        let first = prune_registered_with(&home, &disk, |_| Ok(11)).unwrap();
        assert_eq!((first.files, first.remaining), (1, size - 100));
        assert_eq!(left(&target), ["f2", "f3", "f4"]);
        let second = prune_registered_with(&home, &disk, |_| Ok(11)).unwrap();
        assert_eq!(second.files, 0);
        assert_eq!(left(&target), ["f2", "f3", "f4"]);
        assert!(target.join(TARGET_PROOF).exists());
    }

    #[test]
    fn an_artifact_larger_than_size_minus_reserve_survives_every_pass() {
        let files: &[(&str, usize, u64)] = &[("artifact", 300, 1_000_000)];
        let (_keep, home, targets) = registered_targets(&[files]);
        let target = &targets[0];
        let size = crate::disk::dir_size(target);
        // Reserve 100 bytes above what removing the artifact would leave.
        let disk = pool(size - 100, 10, Some(1_000_000));
        for _ in 0..3 {
            // The simulated rebuild between passes.
            fs::write(target.join("artifact"), vec![0u8; 300]).unwrap();
            let pass = prune_registered_with(&home, &disk, |_| Ok(11)).unwrap();
            assert_eq!(pass.files, 0, "{pass:?}");
            assert!(target.join("artifact").exists());
        }
        // Below the admission floor the reserve is gone: space outranks it.
        let pass = prune_registered_with(&home, &disk, |_| Ok(0)).unwrap();
        assert_eq!(pass.files, 1);
        assert!(!target.join("artifact").exists());
        assert!(target.join(TARGET_PROOF).exists());
    }

    #[test]
    fn the_bound_is_the_pools_aggregate_not_each_target() {
        let a: &[(&str, usize, u64)] = &[("f1", 100, 1_000_001), ("f2", 100, 1_000_002)];
        let b: &[(&str, usize, u64)] = &[("f3", 100, 1_000_003), ("f4", 100, 1_000_004)];
        let (_keep, home, targets) = registered_targets(&[a, b]);
        let pool_size: u64 = targets.iter().map(|t| crate::disk::dir_size(t)).sum();
        // The pool may give up 250 bytes above its reserve; whichever target
        // is visited first, that allows two files in all, never three.
        let reserve = pool_size - 250;
        let disk = pool(reserve, 10, Some(1_000_000));
        let pass = prune_registered_with(&home, &disk, |_| Ok(11)).unwrap();
        assert_eq!((pass.files, pass.freed), (2, 200), "{pass:?}");
        let after: u64 = targets.iter().map(|t| crate::disk::dir_size(t)).sum();
        assert_eq!(after, pool_size - 200);
        assert!(after >= reserve, "the pool never crosses its reserve");
        // Another pass finds the pool at what it may give up: nothing more.
        assert_eq!(
            prune_registered_with(&home, &disk, |_| Ok(11))
                .unwrap()
                .files,
            0
        );
    }

    #[test]
    fn below_the_admission_floor_the_pool_has_no_reserve() {
        let (_keep, home, target) = registered_target();
        let size = crate::disk::dir_size(&target);
        // Floor 1000, threshold 2000, free 0: the old rule shrinks the limit by
        // the whole 1000-byte deficit, past anything the pool holds.
        let pass =
            prune_registered_with(&home, &pool(size - 150, 1000, Some(2000)), |_| Ok(0)).unwrap();
        assert_eq!((pass.files, pass.freed), (4, 400));
        assert!(left(&target).is_empty());
        assert!(target.join(TARGET_PROOF).exists());
    }

    #[test]
    fn an_unmeasurable_volume_leaves_the_pool_untouched() {
        let (_keep, home, target) = registered_target();
        let size = crate::disk::dir_size(&target);
        let disk = pool(size - 150, 10, Some(1_000_000));
        assert!(prune_registered_with(&home, &disk, |_| anyhow::bail!("no volume")).is_err());
        assert_eq!(left(&target).len(), 4);
    }

    #[test]
    fn a_target_in_use_is_left_alone() {
        let (_keep, home, target) = registered_target();
        let size = crate::disk::dir_size(&target);
        let owner = cache::Owner::here("run", "node", "seat", &target, "head");
        let _held = match cache::try_acquire(&home, &target, &owner).unwrap() {
            cache::AcquireOutcome::Acquired(guard) => guard,
            cache::AcquireOutcome::Busy(_) => panic!("released target is busy"),
        };
        let pass = prune_registered_with(&home, &pool(size - 150, 10, Some(1_000_000)), |_| Ok(11))
            .unwrap();
        assert_eq!(pass.files, 0);
        assert_eq!(left(&target).len(), 4);
    }
}
