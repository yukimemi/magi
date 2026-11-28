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
    // The collection threshold does not reach the isolated-build pool: it is
    // collected to its cap and, below the admission floor, by the deficit
    // against it. A threshold equal to the floor is exactly that rule.
    let limit = crate::disk::pressure_limit(
        result.remaining,
        disk.cache_limit_bytes,
        disk.min_free_bytes,
        disk.min_free_bytes,
        crate::disk::managed_free_bytes(&root(home))?,
    );
    targets.sort_unstable();
    for (_, target, size) in targets {
        let excess = result.remaining.saturating_sub(limit);
        if excess == 0 {
            break;
        }
        if size == 0 {
            continue;
        }
        match cache::maintenance_prune(home, &target, size.saturating_sub(excess)) {
            Ok(Some(pruned)) => {
                result.remaining = result.remaining.saturating_sub(pruned.freed);
                result.freed = result.freed.saturating_add(pruned.freed);
                result.files = result.files.saturating_add(pruned.files);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, target = %target.display(), "isolated build prune refused")
            }
        }
    }
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
}
