//! Durable, bounded source evidence for confirmed-merge cleanup.
//!
//! Each unincorporated source HEAD gets a binary patch against the run base, a raw
//! change index (including paths/modes), and the old blobs needed to read or
//! reconstruct that change after Git garbage collection. Reflogs and ORIG_HEAD
//! are obsolete execution history, not additional retention obligations.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context as _, Result, ensure};
use serde_json::json;
use tokio::io::AsyncReadExt as _;

use crate::proc::Quiet as _;
use crate::{git, run::RunState};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 4096;
const MAX_HEADS: usize = 128;

/// The caller holds owner, driver, repository and source output leases. All
/// writes finish before the first worktree/branch removal. Existing evidence
/// is immutable, so a retry after partial deletion never replaces earlier facts.
pub(crate) async fn preserve(
    state: &RunState,
    home: &Path,
    heads: &BTreeSet<String>,
) -> Result<()> {
    ensure!(
        heads.len() <= MAX_HEADS,
        "fold evidence source limit exceeded"
    );
    let dir = home
        .join("runs")
        .join(&state.id)
        .join("artifacts/fold-evidence");
    std::fs::create_dir_all(&dir)?;
    // Persist newly created directory links before later deleting the only
    // source. Syncing files alone does not persist their parent directories.
    let artifacts = dir.parent().context("artifacts directory")?;
    sync_directory(artifacts)?;
    sync_directory(artifacts.parent().context("run directory")?)?;
    let mut bytes = 0;
    let mut files = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        // Only this writer uses the reserved pending suffix. A process may
        // have stopped between write and rename; no manifest references it.
        if entry.file_name().to_string_lossy().ends_with(".pending") {
            std::fs::remove_file(entry.path())?;
            continue;
        }
        let metadata = entry.metadata()?;
        ensure!(metadata.is_file(), "unexpected fold evidence directory");
        bytes += metadata.len();
        files += 1;
    }
    ensure!(
        bytes <= MAX_BYTES && files <= MAX_FILES,
        "fold evidence storage limit exceeded"
    );
    let mut manifest: BTreeMap<String, serde_json::Value> =
        match std::fs::read(dir.join("manifest.json")) {
            Ok(data) => serde_json::from_slice(&data)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
    let unincorporated = unincorporated_heads(state, heads).await?;
    for head in &unincorporated {
        let key = format!("head-{head}");
        let patch = source_patch(&state.repo, &state.base_commit, head).await?;
        let index = source_index(&state.repo, &state.base_commit, head).await?;
        let mut blobs = BTreeSet::new();
        // --raw -z emits pairs: a metadata header, then the exact pathname.
        let mut records = index.split(|b| *b == 0);
        while let Some(header) = records.next() {
            if header.is_empty() {
                break;
            }
            records.next().context("missing raw fold pathname")?;
            let header = std::str::from_utf8(header)?;
            let fields: Vec<_> = header.split_whitespace().collect();
            ensure!(fields.len() == 5, "invalid raw fold change index");
            let old = fields[2];
            // Gitlinks identify another repository; the patch records their
            // IDs, but that repository's contents were never this run's to own.
            if fields[0] != ":160000" && !old.bytes().all(|b| b == b'0') {
                blobs.insert(old.to_owned());
            }
        }
        for blob in &blobs {
            let data = capture(&state.repo, &["cat-file", "blob", blob]).await?;
            store(&dir, &format!("blob-{blob}"), &data, &mut bytes, &mut files)?;
        }
        let patch_name = format!("{head}.patch");
        let index_name = format!("{head}.index");
        store(&dir, &patch_name, &patch, &mut bytes, &mut files)?;
        store(&dir, &index_name, &index, &mut bytes, &mut files)?;
        manifest.insert(
            key,
            json!({
                "head": head, "base_commit": state.base_commit,
                "patch": patch_name, "index": index_name, "base_blobs": blobs,
                "patch_bytes": patch.len(), "index_bytes": index.len(),
            }),
        );
    }
    // Keep the first inventory, including missing sources, across retries.
    if !manifest.contains_key("sources") {
        let mut sources = Vec::new();
        for candidate in &state.candidates {
            let head = if git::branch_exists(&state.repo, &candidate.branch).await? {
                Some(git::rev_parse(&state.repo, &candidate.branch).await?)
            } else {
                None
            };
            sources.push(json!({
                "head": head,
                "label": candidate.label, "branch": candidate.branch,
                "missing": !candidate.worktree.try_exists()?,
                "released": state.released_branches.contains(&candidate.branch),
            }));
        }
        let entries = match std::fs::read_dir(state.worktree_root()) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        for entry in entries.into_iter().flatten() {
            let entry = entry?;
            sources.push(json!({
                "worktree": entry.file_name().to_string_lossy(),
                "head": git::rev_parse(&entry.path(), "HEAD").await?,
            }));
        }
        manifest.insert("sources".into(), json!(sources));
        manifest.insert(
            "history_policy".into(),
            json!("Unincorporated current source tips preserved; obsolete reflog/ORIG_HEAD history discarded."),
        );
    }
    let data = serde_json::to_vec_pretty(&manifest)?;
    // Account for replacement separately; the old manifest remains valid on
    // any failure. Never silently truncate evidence to fit the budget.
    let old = std::fs::metadata(dir.join("manifest.json")).map_or(0, |m| m.len());
    ensure!(
        bytes - old + data.len() as u64 <= MAX_BYTES,
        "fold evidence byte limit exceeded"
    );
    ensure!(
        files + usize::from(!dir.join("manifest.json").try_exists()?) <= MAX_FILES,
        "fold evidence file limit exceeded"
    );
    atomic_write(&dir.join("manifest.json"), &data)?;
    Ok(())
}

/// Pin the fetched base once. A confirmed merge permits proof, but does not
/// establish that every losing candidate or reviewer tip was incorporated.
async fn unincorporated_heads(
    state: &RunState,
    heads: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    if heads.is_empty() {
        return Ok(BTreeSet::new());
    }
    let reference = format!(
        "refs/remotes/{}/{}",
        state.config.merge.remote, state.base_branch
    );
    let base = git::rev_parse(&state.repo, &reference)
        .await
        .context("cannot prove merged sources against the fetched base")?;
    let mut squash_winner = None;
    let mut checked_squash = false;
    let mut remaining = BTreeSet::new();
    for head in heads {
        if is_ancestor(&state.repo, head, &base).await? {
            continue;
        }
        if !checked_squash {
            squash_winner = proven_squash_winner(state, &base).await?;
            checked_squash = true;
        }
        if let Some(winner) = &squash_winner
            && is_ancestor(&state.repo, head, winner).await?
        {
            continue;
        }
        remaining.insert(head.clone());
    }
    Ok(remaining)
}

async fn is_ancestor(repo: &Path, head: &str, base: &str) -> Result<bool> {
    Ok(
        git::git_raw(repo, &["merge-base", "--is-ancestor", head, base])
            .await?
            .ok(),
    )
}

/// Match the complete binary patch against a single-parent commit in fetched
/// history, including modes and blob IDs. Later base changes do not invalidate
/// this proof. Missing winner branches on cleanup retries provide no proof.
async fn proven_squash_winner(state: &RunState, base: &str) -> Result<Option<String>> {
    let Some(winner) = state.winner() else {
        return Ok(None);
    };
    if !git::branch_exists(&state.repo, &winner.branch).await? {
        return Ok(None);
    }
    let head = git::rev_parse(&state.repo, &winner.branch).await?;
    if !is_ancestor(&state.repo, &state.base_commit, &head).await? {
        return Ok(None);
    }
    let patch = source_patch(&state.repo, &state.base_commit, &head).await?;
    if patch.is_empty() {
        return Ok(None);
    }
    let range = format!("{}..{base}", state.base_commit);
    let commits = capture(&state.repo, &["log", "--no-merges", "--format=%H", &range]).await?;
    for commit in std::str::from_utf8(&commits)?.lines() {
        let parent = format!("{commit}^");
        if source_patch(&state.repo, &parent, commit).await? == patch {
            return Ok(Some(head));
        }
    }
    Ok(None)
}

async fn source_patch(repo: &Path, from: &str, to: &str) -> Result<Vec<u8>> {
    capture(
        repo,
        &[
            "-c",
            "core.quotePath=true",
            "diff",
            "--binary",
            "--full-index",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=none",
            "--submodule=short",
            from,
            to,
            "--",
        ],
    )
    .await
}

async fn source_index(repo: &Path, from: &str, to: &str) -> Result<Vec<u8>> {
    capture(
        repo,
        &[
            "diff",
            "--raw",
            "--no-ext-diff",
            "--no-textconv",
            "--no-abbrev",
            "--no-renames",
            "-z",
            from,
            to,
            "--",
        ],
    )
    .await
}

async fn capture(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new("git")
        .quiet()
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut data = Vec::new();
    child
        .stdout
        .take()
        .context("git stdout")?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut data)
        .await?;
    ensure!(
        data.len() as u64 <= MAX_BYTES,
        "fold evidence byte limit exceeded"
    );
    ensure!(
        child.wait().await?.success(),
        "git fold evidence capture failed: {args:?}"
    );
    Ok(data)
}

fn store(dir: &Path, name: &str, data: &[u8], bytes: &mut u64, files: &mut usize) -> Result<()> {
    let path = dir.join(name);
    if path.try_exists()? {
        ensure!(
            std::fs::read(&path)? == data,
            "fold evidence content mismatch: {name}"
        );
        return Ok(());
    }
    ensure!(
        *files < MAX_FILES && *bytes + data.len() as u64 <= MAX_BYTES,
        "fold evidence storage limit exceeded"
    );
    atomic_write(&path, data)?;
    *bytes += data.len() as u64;
    *files += 1;
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.pending",
        path.file_name().context("artifact name")?.to_string_lossy()
    ));
    let mut opened = false;
    let result = (|| {
        if let Ok(meta) = std::fs::symlink_metadata(&tmp) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "non-file evidence staging path"
            );
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&tmp)?;
        opened = true;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        sync_directory(path.parent().context("artifact parent")?)?;
        Ok(())
    })();
    if result.is_err() && opened {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub(crate) fn sync_directory(_path: &Path) -> Result<()> {
    // Normal directory handles cannot be opened/synced this way on Windows.
    // Keep file syncing and atomic publication on every platform, and persist
    // directory entries on Unix, as in retention::reconcile::durable_new_with.
    #[cfg(unix)]
    std::fs::File::open(_path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn evidence_staging_preserves_unknown_symlinks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("operator-file");
        std::fs::write(&target, "keep")?;
        let artifact = temp.path().join("artifact");
        let pending = temp.path().join("artifact.pending");
        std::os::unix::fs::symlink(&target, &pending)?;
        assert!(atomic_write(&artifact, b"new evidence").is_err());
        assert!(
            std::fs::symlink_metadata(&pending)?
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target)?, b"keep");
        Ok(())
    }

    #[test]
    fn evidence_publication_syncs_supported_directories() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let artifacts = dir.path().join("artifacts");
        let evidence = artifacts.join("fold-evidence");
        std::fs::create_dir_all(&evidence)?;
        sync_directory(&artifacts)?;
        sync_directory(dir.path())?;

        let path = evidence.join("manifest.json");
        atomic_write(&path, b"{}")?;
        assert_eq!(std::fs::read(&path)?, b"{}");
        assert!(!evidence.join("manifest.json.pending").exists());
        Ok(())
    }
}

/// A bounded, history-independent tree archive. Pack input is an explicit set
/// of tree/blob objects, never a revision walk through the repository history.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    head: String,
    base: String,
    head_tree: String,
    head_commit: Vec<u8>,
    base_commit: Vec<u8>,
    base_tree: String,
    pack_hash: String,
    patch_hash: String,
    index_hash: String,
}

async fn input(repo: &Path, args: &[&str], data: Vec<u8>) -> Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt as _;
    let mut child = tokio::process::Command::new("git")
        .quiet()
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("git stdin")?;
    let mut stdout = child
        .stdout
        .take()
        .context("git stdout")?
        .take(MAX_BYTES + 1);
    let mut output = Vec::new();
    let (write, read) = tokio::join!(
        async move { stdin.write_all(&data).await },
        stdout.read_to_end(&mut output)
    );
    write?;
    read?;
    ensure!(
        output.len() as u64 <= MAX_BYTES,
        "snapshot byte limit exceeded"
    );
    ensure!(
        child.wait().await?.success(),
        "snapshot git command failed: {args:?}"
    );
    Ok(output)
}

async fn digest(repo: &Path, bytes: Vec<u8>) -> Result<String> {
    Ok(
        String::from_utf8(input(repo, &["hash-object", "--stdin"], bytes).await?)?
            .trim()
            .to_owned(),
    )
}

fn plain(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        (meta.is_file() || meta.is_dir())
            && !meta.file_type().is_symlink()
            && path.canonicalize()? == path,
        "noncanonical source evidence"
    );
    Ok(())
}

/// Publish an immutable source snapshot alongside the existing fold artifacts.
/// It includes both complete trees, binary patch and raw mode/path index, so
/// reading/restoring it never depends on refs, reflogs or future object GC.
pub(crate) async fn snapshot(
    state: &RunState,
    home: &Path,
    head: &str,
    budget: u64,
) -> Result<std::path::PathBuf> {
    let parent = home
        .join("runs")
        .join(&state.id)
        .join("artifacts/source-evidence");
    let artifacts = parent.parent().context("artifacts")?;
    plain(artifacts.parent().context("run")?)?;
    create_plain_dir(artifacts)?;
    create_plain_dir(&parent)?;
    sync_directory(parent.parent().context("artifacts")?)?;
    sync_directory(parent.parent().and_then(Path::parent).context("run")?)?;
    let dir = parent.join(format!("{head}-{}", state.base_commit));
    if dir.try_exists()? {
        validate_snapshot(&dir).await?;
        let manifest: Snapshot =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
        ensure!(
            manifest.head == head && manifest.base == state.base_commit,
            "snapshot identity mismatch"
        );
        return Ok(dir);
    }
    let base_tree = git::rev_parse(&state.repo, &format!("{}^{{tree}}", state.base_commit)).await?;
    let head_tree = git::rev_parse(&state.repo, &format!("{head}^{{tree}}")).await?;
    let mut objects = BTreeSet::from([base_tree.clone(), head_tree.clone()]);
    for tree in [&base_tree, &head_tree] {
        let entries = capture(&state.repo, &["ls-tree", "-r", "-t", "-z", tree]).await?;
        for entry in entries.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let header =
                std::str::from_utf8(entry.split(|b| *b == b'\t').next().context("tree entry")?)?;
            let fields: Vec<_> = header.split_whitespace().collect();
            ensure!(
                fields.len() == 3 && matches!(fields[1], "tree" | "blob"),
                "unsupported snapshot object"
            );
            objects.insert(fields[2].to_owned());
            ensure!(objects.len() <= MAX_FILES, "snapshot object limit exceeded");
        }
    }
    let object_list = objects
        .iter()
        .map(|s| s.to_owned() + "\n")
        .collect::<String>()
        .into_bytes();
    let sizes = input(
        &state.repo,
        &["cat-file", "--batch-check=%(objectsize)"],
        object_list.clone(),
    )
    .await?;
    let total =
        std::str::from_utf8(&sizes)?
            .lines()
            .try_fold(0u64, |sum, size| -> Result<u64> {
                let sum = sum
                    .checked_add(size.parse::<u64>()?)
                    .context("object size overflow")?;
                ensure!(sum <= MAX_BYTES, "snapshot expanded object limit exceeded");
                Ok(sum)
            })?;
    ensure!(
        total <= MAX_BYTES,
        "snapshot expanded object limit exceeded"
    );
    let pack = input(&state.repo, &["pack-objects", "--stdout"], object_list).await?;
    let patch = source_patch(&state.repo, &state.base_commit, head).await?;
    let index = source_index(&state.repo, &state.base_commit, head).await?;
    ensure!(
        pack.len() + patch.len() + index.len() <= MAX_BYTES.min(budget) as usize,
        "snapshot storage limit exceeded"
    );
    let head_commit = capture(&state.repo, &["cat-file", "commit", head]).await?;
    let base_commit = capture(&state.repo, &["cat-file", "commit", &state.base_commit]).await?;
    ensure!(
        head_commit.len() + base_commit.len() <= 1024 * 1024,
        "snapshot commit metadata limit exceeded"
    );
    let manifest = Snapshot {
        version: 1,
        head: head.to_owned(),
        base: state.base_commit.clone(),
        base_tree,
        head_tree,
        head_commit,
        base_commit,
        pack_hash: digest(&state.repo, pack.clone()).await?,
        patch_hash: digest(&state.repo, patch.clone()).await?,
        index_hash: digest(&state.repo, index.clone()).await?,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    ensure!(
        pack.len() + patch.len() + index.len() + manifest_bytes.len()
            <= MAX_BYTES.min(budget) as usize,
        "snapshot publication budget exceeded"
    );
    let pending = dir.with_extension("pending");
    let existing = snapshot_storage(&parent)?;
    let staged = if pending.try_exists()? {
        snapshot_storage(&pending)?
    } else {
        0
    };
    ensure!(
        existing
            .saturating_sub(staged)
            .saturating_add((pack.len() + patch.len() + index.len() + manifest_bytes.len()) as u64)
            <= MAX_BYTES,
        "source evidence per-run storage limit exceeded"
    );
    create_plain_dir(&pending)?;
    for entry in std::fs::read_dir(&pending)? {
        let path = entry?.path();
        plain(&path)?;
        ensure!(
            matches!(
                path.file_name().and_then(|s| s.to_str()),
                Some(
                    "objects.pack"
                        | "change.patch"
                        | "change.index"
                        | "manifest.json"
                        | "objects.pack.pending"
                        | "change.patch.pending"
                        | "change.index.pending"
                        | "manifest.json.pending"
                )
            ),
            "unknown pending archive content"
        );
    }
    sync_directory(&parent)?;
    let mut bytes = 0;
    let mut files = 0;
    store(&pending, "objects.pack", &pack, &mut bytes, &mut files)?;
    store(&pending, "change.patch", &patch, &mut bytes, &mut files)?;
    store(&pending, "change.index", &index, &mut bytes, &mut files)?;
    store(
        &pending,
        "manifest.json",
        &manifest_bytes,
        &mut bytes,
        &mut files,
    )?;
    validate_snapshot(&pending).await?;
    std::fs::rename(&pending, &dir)?;
    sync_directory(&parent)?;
    Ok(dir)
}

async fn load_snapshot(dir: &Path, repo: &Path) -> Result<Snapshot> {
    plain(dir)?;
    plain(&dir.join("manifest.json"))?;
    ensure!(
        std::fs::metadata(dir.join("manifest.json"))?.len() <= 8 * 1024 * 1024,
        "snapshot manifest limit exceeded"
    );
    let manifest: Snapshot = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
    ensure!(manifest.version == 1, "unsupported snapshot version");
    for oid in [
        &manifest.head,
        &manifest.base,
        &manifest.head_tree,
        &manifest.base_tree,
    ] {
        ensure!(
            matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid snapshot object ID"
        );
    }
    for (oid, tree, commit) in [
        (&manifest.head, &manifest.head_tree, &manifest.head_commit),
        (&manifest.base, &manifest.base_tree, &manifest.base_commit),
    ] {
        ensure!(
            commit.starts_with(format!("tree {tree}\n").as_bytes()),
            "snapshot commit/tree mismatch"
        );
        let actual = input(
            repo,
            &["hash-object", "-t", "commit", "--stdin"],
            commit.clone(),
        )
        .await?;
        ensure!(
            std::str::from_utf8(&actual)?.trim() == oid,
            "snapshot commit identity mismatch"
        );
    }
    let mut total = 0;
    for (name, hash) in [
        ("objects.pack", &manifest.pack_hash),
        ("change.patch", &manifest.patch_hash),
        ("change.index", &manifest.index_hash),
    ] {
        let path = dir.join(name);
        plain(&path)?;
        total += std::fs::metadata(&path)?.len();
        ensure!(total <= MAX_BYTES, "snapshot storage limit exceeded");
        ensure!(
            digest(repo, std::fs::read(path)?).await? == *hash,
            "corrupt snapshot: {name}"
        );
    }
    input(
        repo,
        &["unpack-objects", "-r"],
        std::fs::read(dir.join("objects.pack"))?,
    )
    .await?;
    for tree in [&manifest.base_tree, &manifest.head_tree] {
        capture(repo, &["read-tree", tree]).await?;
        // write-tree traverses the index and rejects missing tree/blob objects.
        ensure!(
            git::git(repo, &["write-tree"]).await? == *tree,
            "incomplete snapshot tree"
        );
    }
    ensure!(
        source_patch(repo, &manifest.base_tree, &manifest.head_tree).await?
            == std::fs::read(dir.join("change.patch"))?,
        "snapshot patch mismatch"
    );
    let index = source_index(repo, &manifest.base_tree, &manifest.head_tree).await?;
    ensure!(
        index == std::fs::read(dir.join("change.index"))?,
        "snapshot index mismatch"
    );
    Ok(manifest)
}

pub(crate) async fn validate_identity(dir: &Path, head: &str, base: &str) -> Result<()> {
    validate_snapshot(dir).await?;
    let manifest: Snapshot = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
    ensure!(
        manifest.head == head && manifest.base == base,
        "snapshot identity mismatch"
    );
    Ok(())
}

pub(crate) async fn validate_snapshot(dir: &Path) -> Result<()> {
    let temp = Inspection::new(&std::env::temp_dir())?;
    let repo = temp.0.clone();
    git::git(&repo, &["init", "--quiet", "--template="]).await?;
    ensure!(
        git::git(&repo, &["rev-parse", "--show-toplevel"]).await?
            == repo.to_str().context("inspection path")?,
        "inspection worktree redirected by Git configuration"
    );
    let manifest = load_snapshot(dir, &repo).await?;
    // The archive must reproduce its source through the displayed binary
    // patch, not just contain two individually valid trees. This repository
    // and its index are private and have no alternates to the source store.
    capture(&repo, &["read-tree", &manifest.base_tree]).await?;
    let patch = std::fs::read(dir.join("change.patch"))?;
    if !patch.is_empty() {
        input(&repo, &["apply", "--cached", "--binary", "-"], patch).await?;
    }
    ensure!(
        git::git(&repo, &["write-tree"]).await? == manifest.head_tree,
        "archived patch does not reconstruct the source tree"
    );
    capture(&repo, &["checkout-index", "--all", "--force"]).await?;
    // A cached tree/index alone does not prove that every blob is present.
    for tree in [&manifest.head_tree, &manifest.base_tree] {
        capture(&repo, &["read-tree", "--reset", "-u", tree]).await?;
        super::retention::tracked_base(&repo, tree).await?;
    }
    Ok(())
}

/// Restore an archived source into a NEW inspection repository. This does not
/// transfer public/recovery authority or change any task/run record.
pub async fn restore_snapshot(dir: &Path, destination: &Path) -> Result<()> {
    ensure!(
        !destination.try_exists()?,
        "restore destination already exists"
    );
    validate_snapshot(dir).await?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let destination = parent.join(destination.file_name().context("restore directory name")?);
    ensure!(
        matches!(std::fs::symlink_metadata(&destination), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        "restore destination already exists or is inaccessible"
    );
    let temp = Inspection::new(&parent)?;
    let repo = temp.0.clone();
    git::git(&repo, &["init", "--quiet", "--template="]).await?;
    ensure!(
        git::git(&repo, &["rev-parse", "--show-toplevel"]).await?
            == repo.to_str().context("inspection path")?,
        "inspection worktree redirected by Git configuration"
    );
    let manifest = load_snapshot(dir, &repo).await?;
    capture(&repo, &["checkout-index", "--all"]).await?;
    super::retention::tracked_base(&repo, &manifest.head_tree).await?;
    std::fs::rename(&temp.0, destination)?;
    sync_directory(&parent)?;
    Ok(())
}

struct Inspection(std::path::PathBuf);
impl Inspection {
    fn new(parent: &Path) -> Result<Self> {
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG",
        ] {
            ensure!(
                std::env::var_os(name).is_none(),
                "inspection refuses Git environment override: {name}"
            );
        }
        let path = parent.canonicalize()?.join(format!(
            "magi-source-inspection-{:016x}",
            crate::rng::entropy()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Inspection {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create_plain_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => plain(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            plain(path.parent().context("evidence parent")?)?;
            std::fs::create_dir(path)?;
        }
        Err(e) => return Err(e.into()),
    }
    ensure!(
        std::fs::symlink_metadata(path)?.is_dir(),
        "evidence directory is not a directory"
    );
    plain(path)
}

// Include incomplete publication products in the same per-run bound. This
// inventory never follows symlinks or adopts unknown artifact kinds.
fn snapshot_storage(root: &Path) -> Result<u64> {
    plain(root)?;
    let mut dirs = vec![(root.to_owned(), 0usize)];
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut entries = 0usize;
    while let Some((dir, depth)) = dirs.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            plain(&path)?;
            entries += 1;
            ensure!(entries <= MAX_FILES, "source evidence entry limit exceeded");
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                ensure!(
                    depth == 0 && dirs.len() < MAX_HEADS,
                    "unknown nested source evidence directory"
                );
                dirs.push((path, depth + 1));
            } else {
                files += 1;
                bytes = bytes.saturating_add(meta.len());
                ensure!(
                    files <= MAX_FILES && bytes <= MAX_BYTES,
                    "source evidence storage limit exceeded"
                );
            }
        }
    }
    Ok(bytes)
}
