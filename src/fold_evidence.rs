//! Durable, bounded source evidence for confirmed-merge cleanup.
//!
//! Each current source HEAD gets a binary patch against the run base, a raw
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
    std::fs::File::open(artifacts)?.sync_all()?;
    std::fs::File::open(artifacts.parent().context("run directory")?)?.sync_all()?;
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
    for head in heads {
        let key = format!("head-{head}");
        let patch = capture(
            &state.repo,
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
                &state.base_commit,
                head,
                "--",
            ],
        )
        .await?;
        let index = capture(
            &state.repo,
            &[
                "diff",
                "--raw",
                "--no-ext-diff",
                "--no-textconv",
                "--no-abbrev",
                "--no-renames",
                "-z",
                &state.base_commit,
                head,
                "--",
            ],
        )
        .await?;
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
            json!("Current source tips preserved; obsolete reflog/ORIG_HEAD history discarded."),
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

async fn capture(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new("git")
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

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.pending",
        path.file_name().context("artifact name")?.to_string_lossy()
    ));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(path.parent().context("artifact parent")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
