//! Checkout eviction does not retire a run or alter task/recovery lineage.
use super::*;
use std::collections::BTreeMap;

const LIMIT: usize = 5;
const EVIDENCE_BUDGET: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Eviction {
    version: u32,
    run: String,
    source: PathBuf,
    head: String,
    evidence: PathBuf,
    admin: BTreeMap<PathBuf, String>,
    admin_contents: BTreeMap<PathBuf, Vec<u8>>,
    history: BTreeMap<String, PathBuf>,
    phase: String,
}

fn read_record(path: &Path) -> Result<serde_json::Value> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && path.canonicalize()? == path,
        "noncanonical ownership record"
    );
    ensure!(
        meta.len() <= 16 * 1024 * 1024,
        "ownership record inspection limit exceeded"
    );
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

// Supported migrations may add defaults or normalize scalar values. They must
// not silently discard a future source/authority field, including nested ones.
fn known_shape(raw: &serde_json::Value, known: &serde_json::Value) -> bool {
    match raw {
        serde_json::Value::Object(fields) => known.as_object().is_some_and(|object| {
            fields
                .iter()
                .all(|(key, value)| object.get(key).is_some_and(|k| known_shape(value, k)))
        }),
        serde_json::Value::Array(values) => known.as_array().is_some_and(|array| {
            values
                .iter()
                .enumerate()
                .all(|(i, value)| array.get(i).is_some_and(|k| known_shape(value, k)))
        }),
        _ => true,
    }
}

fn history(home: &Path) -> Result<Vec<RunState>> {
    let mut states = Vec::new();
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(home.join("runs"))? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !crate::run::is_run_id(&id) {
            continue;
        }
        let path = entry.path().join("run.json");
        bytes = bytes.saturating_add(std::fs::symlink_metadata(&path)?.len());
        ensure!(
            bytes <= 256 * 1024 * 1024 && states.len() < 4096,
            "run ownership inspection budget exceeded"
        );
        let raw = read_record(&path)?;
        let state = RunState::load_under(&id, home)?;
        ensure!(
            known_shape(&raw, &serde_json::to_value(&state)?),
            "unknown source/authority fields in run {id}"
        );
        states.push(state);
    }
    Ok(states)
}

fn tasks(home: &Path) -> Result<Vec<crate::queue::Task>> {
    let root = home.join("queue");
    if root.try_exists()? {
        ensure!(
            plain_dir(&root) && root.canonicalize()? == root,
            "noncanonical queue ownership"
        );
        let mut bytes = 0u64;
        let mut count = 0usize;
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|s| s == "json") {
                bytes = bytes.saturating_add(entry.metadata()?.len());
                count += 1;
                ensure!(
                    bytes <= 64 * 1024 * 1024 && count <= 4096,
                    "task ownership inspection budget exceeded"
                );
            }
        }
    }
    let queue = crate::queue::Queue::at(root);
    ensure!(queue.unreadable().is_empty(), "unreadable task ownership");
    let tasks = queue.list();
    for task in &tasks {
        let raw = read_record(&queue.path_of(&task.id))?;
        ensure!(
            known_shape(&raw, &serde_json::to_value(task)?),
            "unknown source/authority fields in task {}",
            task.id
        );
    }
    Ok(tasks)
}

fn mentions(value: &serde_json::Value, path: &Path) -> bool {
    match value {
        serde_json::Value::String(s) => Path::new(s) == path || Path::new(s).starts_with(path),
        serde_json::Value::Array(a) => a.iter().any(|v| mentions(v, path)),
        serde_json::Value::Object(o) => o.values().any(|v| mentions(v, path)),
        _ => false,
    }
}

fn unborrowed(home: &Path, state: &RunState, path: &Path) -> Result<()> {
    for other in history(home)? {
        if other.id != state.id {
            ensure!(
                other.worktree_root() != state.worktree_root(),
                "run root shared with {}",
                other.id
            );
            // Lineage IDs alone do not borrow files. Explicit source
            // selections and concrete paths still retain their authority.
            ensure!(
                !other
                    .pr_recovery
                    .as_ref()
                    .is_some_and(|p| p.source_run.as_ref() == Some(&state.id)
                        || p.source_proof.as_ref().is_some_and(|p| p.run == state.id)),
                "selected recovery source for run {}",
                other.id
            );
            ensure!(
                !mentions(&serde_json::to_value(&other)?, path),
                "source referenced by run {}",
                other.id
            );
        }
    }
    for task in tasks(home)? {
        ensure!(
            !mentions(&serde_json::to_value(&task)?, path),
            "source referenced by task {}",
            task.id
        );
    }
    Ok(())
}

fn eligible(
    home: &Path,
    state: &RunState,
    now: Timestamp,
    disk: &crate::config::Disk,
) -> Result<()> {
    ensure!(
        !state.status.resumable(),
        "resumable run retains materialized seat checkouts"
    );
    ensure!(
        state.liveness_under(home, crate::daemon::is_working_on(home, &state.id, now))
            == Liveness::Dead,
        "run live or ownership unknown"
    );
    ensure!(
        crate::clean::due(now, state.output_activity(), disk.fold_grace_secs),
        "execution activity within grace period"
    );
    ensure!(
        !crate::pr_recovery::explicit_sources_retained(home, &state.id),
        "explicitly retained recovery sources"
    );
    ensure!(
        state.pr.is_none()
            && state.pr_head.is_none()
            && state.pr_recovery.is_none()
            && state.pr_pushed_head.is_none()
            && state.pr_merged_head.is_none()
            && state.pr_taken_over_by.is_none()
            && !state.pr_publication_pending
            && state.land_approval.is_none()
            && state.land_armed_head.is_none()
            && state.merge_decision.is_none()
            && state.recovery.is_none()
            && state.candidate_preservation.is_empty()
            && state.interrupted_implementations.is_empty(),
        "public authority, approval or recovery checkpoint"
    );
    let questions = crate::ask::Questions::at(home.join("questions")).list();
    for task in tasks(home)? {
        if task.runs.contains(&state.id) {
            ensure!(
                !questions
                    .iter()
                    .any(|q| q.status.open()
                        && (q.run == task.id || task.blocked_by.contains(&q.id))),
                "task has an unanswered owner decision"
            );
        }
        ensure!(
            !task
                .resume_override
                .as_ref()
                .is_some_and(|p| p.pinned_run.as_ref() == Some(&state.id))
                && !task.replacement.as_ref().is_some_and(|p| p.run == state.id)
                && !task
                    .pr_recovery
                    .as_ref()
                    .is_some_and(|p| p.source_run.as_ref() == Some(&state.id)
                        || p.source_proof.as_ref().is_some_and(|p| p.run == state.id)),
            "selected recovery source for task {}",
            task.id
        );
    }
    ensure!(
        !crate::ask::Questions::at(home.join("questions"))
            .list()
            .iter()
            .any(|q| q.run == state.id && q.status.open()),
        "unanswered owner decision"
    );
    Ok(())
}

fn disposable(state: &RunState, path: &Path) -> bool {
    read_seat(path)
        || state.status == crate::run::RunStatus::Superseded
            && state
                .candidates
                .iter()
                .any(|c| c.worktree == path && c.empty && c.commits == 0)
}

fn read_seat(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| {
            ["review-", "judge-", "advisor-"].iter().any(|prefix| {
                name.strip_prefix(prefix)
                    .is_some_and(|n| n.parse::<usize>().is_ok_and(|n| n > 0))
            })
        })
}

async fn proof(
    home: &Path,
    state: &RunState,
    path: &Path,
    now: Timestamp,
    disk: &crate::config::Disk,
) -> Result<String> {
    ensure!(
        worktrees(state).await?.contains(&path.to_owned()),
        "source registration changed"
    );
    ensure!(disposable(state, path), "selected implementation checkout");
    unborrowed(home, state, path)?;
    let head = git::rev_parse(path, "HEAD").await?;
    if let Some(candidate) = state.candidates.iter().find(|c| c.worktree == path) {
        ensure!(
            state.status == crate::run::RunStatus::Superseded
                && head == state.base_commit
                && candidate.empty
                && candidate.commits == 0
                && git::rev_parse(&state.repo, &candidate.branch).await? == head
                && git::git(path, &["symbolic-ref", "HEAD"]).await?
                    == format!("refs/heads/{}", candidate.branch),
            "historical implementation is not an empty superseded attempt"
        );
    } else {
        ensure!(
            !git::git_raw(path, &["symbolic-ref", "-q", "HEAD"])
                .await?
                .ok(),
            "source is attached to a branch"
        );
        ensure!(
            head == state.base_commit || state.reviews.iter().any(|r| r.head == head),
            "HEAD has no reproducible seat checkpoint"
        );
    }
    ensure!(
        git::git(
            path,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored"
            ]
        )
        .await?
        .is_empty(),
        "tracked, untracked, ignored content or build outputs retained"
    );
    ensure!(
        git::git(path, &["write-tree"]).await? == git::rev_parse(path, "HEAD^{tree}").await?,
        "index differs from HEAD"
    );
    tracked_base(path, &head).await?;
    exact_inventory(path, &head).await?;
    let admin = PathBuf::from(git::git(path, &["rev-parse", "--absolute-git-dir"]).await?);
    let lease = std::fs::metadata(admin.join("magi-output.lock"))?;
    ensure!(
        lease.len() == 0
            || lease.len() == 1
                && crate::clean::due(
                    now,
                    Timestamp::try_from(lease.modified()?)?,
                    disk.fold_grace_secs
                ),
        "recent or unreadable output activity"
    );
    Ok(head)
}

async fn exact_inventory(path: &Path, head: &str) -> Result<()> {
    let tree = git::git(path, &["ls-tree", "-r", "-t", "--full-tree", "-z", head]).await?;
    let mut expected = std::collections::BTreeSet::from([path.join(".git")]);
    for record in tree.split('\0').filter(|r| !r.is_empty()) {
        let (_, name) = record.split_once('\t').context("invalid tree inventory")?;
        expected.insert(path.join(name));
    }
    let mut dirs = vec![path.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir)? {
            let file = entry?.path();
            ensure!(
                expected.contains(&file),
                "unknown source entry (including empty directories): {}",
                file.display()
            );
            if std::fs::symlink_metadata(&file)?.is_dir() {
                dirs.push(file);
            }
        }
    }
    Ok(())
}

fn regular_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![root.to_owned()];
    let mut files = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            ensure!(!meta.file_type().is_symlink(), "symlink allocation entry");
            if meta.is_dir() {
                dirs.push(path);
            } else {
                ensure!(meta.is_file(), "unknown allocation entry");
                files.push(path);
            }
            ensure!(
                dirs.len() + files.len() < 100_000,
                "source inventory limit exceeded"
            );
        }
    }
    Ok(files)
}

pub(super) async fn evict(
    home: &Path,
    disk: &crate::config::Disk,
    now: Timestamp,
    report: &mut Report,
) {
    evict_with(home, disk, now, report, |_| Ok(())).await;
}

pub(super) async fn evict_with(
    home: &Path,
    disk: &crate::config::Disk,
    now: Timestamp,
    report: &mut Report,
    before_unlink: fn(&Path) -> Result<()>,
) {
    if !disk.auto_fold || disk.worktree_reclaim_bytes == 0 {
        return;
    }
    let states = match history(home) {
        Ok(mut states) => {
            states.sort_by_key(|s| s.updated_at);
            states
        }
        Err(e) => {
            report
                .retained
                .push(format!("source eviction: unreadable ownership: {e:#}"));
            return;
        }
    };
    let cursor = home.join("source-eviction-cursor");
    let mut states = states;
    match std::fs::symlink_metadata(&cursor) {
        Ok(meta) => {
            if !meta.is_file()
                || meta.file_type().is_symlink()
                || meta.len() > 128
                || !cursor.canonicalize().is_ok_and(|p| p == cursor)
            {
                report
                    .retained
                    .push("source eviction: unknown scan cursor retained".into());
                return;
            }
            if let Ok(last) = std::fs::read_to_string(&cursor)
                && let Some(index) = states.iter().position(|s| s.id == last)
            {
                let len = states.len();
                if len > 0 {
                    states.rotate_left((index + 1) % len);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            report.retained.push(format!("source eviction cursor: {e}"));
            return;
        }
    }
    let mut attempted = 0;
    let mut unlinked = 0u64;
    for listed in states.into_iter().take(64) {
        if attempted >= LIMIT
            || unlinked >= disk.worktree_reclaim_bytes
            || report.source_evidence_added_logical_bytes >= EVIDENCE_BUDGET
        {
            break;
        }
        let result = async {
            let queue = crate::queue::Queue::at(home.join("queue"));
            let tasks = tasks(home)?;
            let mut claims = Vec::new();
            for task in tasks.iter().filter(|t| t.runs.contains(&listed.id)) {
                claims.push(queue.claim(&task.id)?);
            }
            let run_dir = home.join("runs").join(&listed.id);
            ensure!(
                plain_dir(&run_dir) && run_dir.canonicalize()? == run_dir,
                "noncanonical run ownership"
            );
            let _owner = crate::pr_head::owner_lock(home, &listed.id)?;
            let state = RunState::load_under(&listed.id, home)?;
            // Empty roots contain no source bytes. They do not require task
            // retirement or choosing one implementation out of the lineage.
            ensure!(
                state.liveness_under(home, crate::daemon::is_working_on(home, &state.id, now))
                    == Liveness::Dead,
                "run live or ownership unknown"
            );
            ensure!(
                crate::clean::due(now, state.output_activity(), disk.fold_grace_secs),
                "execution activity within grace period"
            );
            let _driver = crate::driver::maintenance(home, &state.id)?;
            let root = state.worktree_root();
            if root.try_exists()? {
                ensure!(
                    plain_dir(&root) && root.canonicalize()? == root,
                    "noncanonical run root"
                );
            }
            let paths = worktrees(&state).await?;
            let mut outputs = Vec::new();
            for path in paths.iter().filter(|p| disposable(&state, p)) {
                outputs.push(OutputLease::take(path, true)?);
            }
            let wait =
                crate::repo_lock::WaitPolicy::new(std::time::Duration::from_secs(1), || false);
            let _coordination = crate::repo_lock::SeatLease::operation_with(
                &state.repo,
                state.config.coordination.as_ref(),
                &wait,
            )
            .await?;
            reconcile_pending(home, &state, disk, now).await?;
            if !root.try_exists()? {
                return Ok(());
            }
            if std::fs::read_dir(&root)?.next().is_none() {
                unborrowed(home, &state, &root)?;
                let list = git::git(&state.repo, &["worktree", "list", "--porcelain"]).await?;
                ensure!(
                    !list
                        .lines()
                        .filter_map(|l| l.strip_prefix("worktree "))
                        .any(|p| Path::new(p).starts_with(&root)),
                    "empty root still has Git registrations"
                );
                std::fs::remove_dir(&root)?;
                report.removed_empty_roots += 1;
                return Ok(());
            }
            eligible(home, &state, now, disk)?;
            for path in paths.iter().filter(|p| disposable(&state, p)) {
                if attempted >= LIMIT
                    || unlinked >= disk.worktree_reclaim_bytes
                    || report.source_evidence_added_logical_bytes >= EVIDENCE_BUDGET
                {
                    break;
                }
                let artifacts = home.join("runs").join(&state.id).join("artifacts");
                let before = if artifacts.exists() {
                    crate::disk::dir_size(&artifacts)
                } else {
                    0
                };
                #[cfg(unix)]
                let allocated_before = if artifacts.exists() {
                    super::allocated_inventory(&artifacts, &mut Default::default())?
                } else {
                    0
                };
                let result = async {
                    let head = proof(home, &state, path, now, disk).await?;
                    // Shared strict administration inventory rejects operations,
                    // local refs, symlinks and unknown checkpoint artifacts.
                    let admin =
                        super::reconcile::evidence(&state, std::slice::from_ref(path)).await?;
                    let mut admin_contents = BTreeMap::new();
                    let mut admin_bytes = 0usize;
                    for (file, hash) in &admin {
                        if hash != "directory" && file.is_absolute() && file.is_file() {
                            let size = std::fs::metadata(file)?.len();
                            ensure!(
                                size <= 1024 * 1024 && admin_bytes + size as usize <= 1024 * 1024,
                                "administration evidence limit exceeded"
                            );
                            let bytes = std::fs::read(file)?;
                            admin_bytes += bytes.len();
                            admin_contents.insert(file.clone(), bytes);
                        }
                    }
                    let mut history = BTreeMap::new();
                    let mut heads: std::collections::BTreeSet<String> =
                        git::git(path, &["reflog", "show", "--format=%H", "HEAD"])
                            .await?
                            .lines()
                            .map(str::to_owned)
                            .collect();
                    let git_dir =
                        PathBuf::from(git::git(path, &["rev-parse", "--absolute-git-dir"]).await?);
                    match std::fs::symlink_metadata(git_dir.join("ORIG_HEAD")) {
                        Ok(_) => {
                            heads.insert(git::rev_parse(path, "ORIG_HEAD").await?);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    heads.insert(head.clone());
                    ensure!(heads.len() <= 128, "source checkpoint limit exceeded");
                    let mut remaining =
                        EVIDENCE_BUDGET.saturating_sub(report.source_evidence_added_logical_bytes);
                    for old in heads {
                        let before = crate::disk::dir_size(&artifacts);
                        let archive =
                            crate::fold_evidence::snapshot(&state, home, &old, remaining).await?;
                        remaining = remaining.saturating_sub(
                            crate::disk::dir_size(&artifacts).saturating_sub(before),
                        );
                        history.insert(old, archive);
                    }
                    let evidence = crate::fold_evidence::snapshot(
                        &state,
                        home,
                        &head,
                        EVIDENCE_BUDGET.saturating_sub(report.source_evidence_added_logical_bytes),
                    )
                    .await?;
                    let ledger = artifacts.join(format!(
                        "eviction-{}.json",
                        path.file_name().context("seat name")?.to_string_lossy()
                    ));
                    let mut record = Eviction {
                        version: 1,
                        run: state.id.clone(),
                        source: path.clone(),
                        head: head.clone(),
                        evidence,
                        admin,
                        admin_contents,
                        history,
                        phase: "pending".into(),
                    };
                    if ledger.try_exists()? {
                        ensure!(
                            std::fs::symlink_metadata(&ledger)?.is_file()
                                && ledger.canonicalize()? == ledger,
                            "noncanonical eviction record"
                        );
                        let previous: Eviction = serde_json::from_slice(&std::fs::read(&ledger)?)?;
                        ensure!(
                            previous.version == 1
                                && previous.run == record.run
                                && previous.source == record.source
                                && previous.head == record.head
                                && previous.evidence == record.evidence
                                && previous.admin == record.admin
                                && previous.admin_contents == record.admin_contents
                                && previous.history == record.history
                                && previous.phase == "pending",
                            "source differs from pending eviction"
                        );
                    }
                    eligible(home, &RunState::load_under(&state.id, home)?, now, disk)?;
                    ensure!(
                        proof(home, &state, path, now, disk).await? == head,
                        "source HEAD changed during archival"
                    );
                    ensure!(
                        super::reconcile::evidence(&state, std::slice::from_ref(path)).await?
                            == record.admin,
                        "source administration changed during archival"
                    );
                    let files = regular_files(path)?;
                    let bytes = files
                        .iter()
                        .map(|p| std::fs::metadata(p).map(|m| m.len()))
                        .collect::<std::io::Result<Vec<_>>>()?
                        .into_iter()
                        .sum::<u64>();
                    let mut allocation = ReleasedAllocation::of(&files)?;
                    // Archive and Git proof finish before taking task write
                    // locks. Fence owner selection across the short durable
                    // intent + clean unlink section; claims alone only fence
                    // workers, not concurrent operator edits.
                    let mut owning: Vec<_> = tasks
                        .iter()
                        .filter(|t| t.runs.contains(&state.id))
                        .collect();
                    owning.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut writes = Vec::new();
                    for task in owning {
                        writes.push(queue.lock_task(&task.id)?);
                        ensure!(
                            serde_json::to_value(queue.get(&task.id)?)?
                                == serde_json::to_value(task)?,
                            "task changed during source archival"
                        );
                    }
                    eligible(home, &RunState::load_under(&state.id, home)?, now, disk)?;
                    unborrowed(home, &state, path)?;
                    let pending = serde_json::to_vec_pretty(&record)?;
                    ensure!(
                        crate::disk::dir_size(&artifacts)
                            .saturating_sub(before)
                            .saturating_add(pending.len() as u64)
                            <= EVIDENCE_BUDGET
                                .saturating_sub(report.source_evidence_added_logical_bytes),
                        "eviction publication budget exceeded"
                    );
                    crate::fold_evidence::atomic_write(&ledger, &pending)?;
                    before_unlink(path)?;
                    // Rebuild every recorded patch in an independent index at
                    // the last boundary before removing its materialized source.
                    for (tip, archive) in &record.history {
                        crate::fold_evidence::validate_identity(archive, tip, &state.base_commit)
                            .await?;
                    }
                    ensure!(
                        git::worktree_remove_clean(&state.repo, path).await?,
                        "Git refused clean checkout eviction"
                    );
                    // Unregistration plus absence, not absence alone, proves the
                    // removal. Pending records survive any interrupted syscall.
                    ensure!(
                        !path.try_exists()? && !worktrees(&state).await?.contains(path),
                        "checkout removal incomplete"
                    );
                    report.evicted_copies += 1;
                    attempted += 1;
                    report.source_unlinked_logical_bytes += bytes;
                    unlinked += bytes;
                    for file in files {
                        report.source_unlinked_allocated_bytes = report
                            .source_unlinked_allocated_bytes
                            .unwrap_or(0)
                            .checked_add(allocation.unlinked(&file).unwrap_or(0));
                    }
                    drop(writes);
                    record.phase = "evicted".into();
                    crate::fold_evidence::atomic_write(
                        &ledger,
                        &serde_json::to_vec_pretty(&record)?,
                    )?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                report.source_evidence_added_logical_bytes +=
                    crate::disk::dir_size(&artifacts).saturating_sub(before);
                #[cfg(unix)]
                if artifacts.exists() {
                    report.source_evidence_added_allocated_bytes = Some(
                        report.source_evidence_added_allocated_bytes.unwrap_or(0)
                            + super::allocated_inventory(&artifacts, &mut Default::default())?
                                .saturating_sub(allocated_before),
                    );
                }
                if let Err(e) = result {
                    report.retained.push(format!(
                        "{}/{}: {e:#}",
                        state.id,
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if !cursor.try_exists().unwrap_or(true) || cursor.canonicalize().is_ok_and(|p| p == cursor)
        {
            let _ = crate::fold_evidence::atomic_write(&cursor, listed.id.as_bytes());
        }
        if let Err(e) = result {
            report
                .retained
                .push(format!("{}: source eviction: {e:#}", listed.id));
        }
    }
}

// Reconcile an interrupted removal only against valid immutable evidence and
// absence from BOTH the filesystem and Git registration. Never infer success
// from a missing path or overwrite a mismatched existing source.
async fn reconcile_pending(
    home: &Path,
    state: &RunState,
    disk: &crate::config::Disk,
    now: Timestamp,
) -> Result<()> {
    let artifacts = home.join("runs").join(&state.id).join("artifacts");
    if !artifacts.try_exists()? {
        return Ok(());
    }
    ensure!(
        plain_dir(&artifacts) && artifacts.canonicalize()? == artifacts,
        "noncanonical artifacts"
    );
    for entry in std::fs::read_dir(&artifacts)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("artifact name")?;
        if !name.starts_with("eviction-") || !name.ends_with(".json") {
            continue;
        }
        ensure!(
            std::fs::symlink_metadata(&path)?.is_file() && path.canonicalize()? == path,
            "noncanonical eviction record"
        );
        let mut record: Eviction = serde_json::from_slice(&std::fs::read(&path)?)?;
        validate_record(state, &artifacts, &record).await?;
        if record.phase == "pending" && !record.source.try_exists()? {
            let list = git::git(&state.repo, &["worktree", "list", "--porcelain"]).await?;
            if list
                .lines()
                .filter_map(|l| l.strip_prefix("worktree "))
                .any(|p| Path::new(p) == record.source)
            {
                eligible(home, state, now, disk)?;
                unborrowed(home, state, &record.source)?;
                remove_missing_registration(state, &record).await?;
            }
            record.phase = "evicted".into();
            crate::fold_evidence::atomic_write(&path, &serde_json::to_vec_pretty(&record)?)?;
        }
    }
    Ok(())
}

// A crash can remove the checkout before Git removes its administration.
// Only the EXACT archived administration can finish that intent. No force,
// broad prune, guessed admin directory, or unknown content is admitted.
async fn remove_missing_registration(state: &RunState, record: &Eviction) -> Result<()> {
    let link = record
        .admin_contents
        .get(&record.source.join(".git"))
        .context("missing archived checkout link")?;
    let link = std::str::from_utf8(link)?
        .trim()
        .strip_prefix("gitdir: ")
        .context("invalid archived checkout link")?;
    let admin = PathBuf::from(link);
    let common = crate::repo_lock::common_dir(&state.repo)
        .await?
        .context("missing repository")?;
    ensure!(
        admin.parent() == Some(common.join("worktrees").as_path())
            && plain_dir(&admin)
            && admin.canonicalize()? == admin,
        "missing source administration identity changed"
    );
    let _output = OutputLease::lock(&admin.join("magi-output.lock"), true)?;
    let mut dirs = vec![admin.clone()];
    while let Some(dir) = dirs.pop() {
        ensure!(
            record.admin.get(&dir).is_some_and(|h| h == "directory"),
            "unrecorded administration directory"
        );
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            ensure!(
                !meta.file_type().is_symlink(),
                "symlink administration artifact"
            );
            if meta.is_dir() {
                dirs.push(path);
            } else {
                ensure!(
                    meta.is_file()
                        && record.admin_contents.get(&path) == Some(&std::fs::read(&path)?),
                    "changed or unknown administration artifact"
                );
            }
        }
    }
    for (path, contents) in record
        .admin_contents
        .iter()
        .filter(|(p, _)| p.starts_with(&admin))
    {
        ensure!(
            std::fs::read(path)? == *contents,
            "missing or changed administration artifact"
        );
    }
    ensure!(
        !record.source.try_exists()?,
        "source reappeared during reconciliation"
    );
    ensure!(
        git::worktree_remove_clean(&state.repo, &record.source).await?,
        "Git refused missing-checkout unregistration"
    );
    let list = git::git(&state.repo, &["worktree", "list", "--porcelain"]).await?;
    ensure!(
        !list
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .any(|p| Path::new(p) == record.source),
        "missing checkout remains registered"
    );
    Ok(())
}

async fn validate_record(state: &RunState, artifacts: &Path, record: &Eviction) -> Result<()> {
    ensure!(
        record.version == 1
            && record.run == state.id
            && record.source.parent() == Some(state.worktree_root().as_path())
            && (read_seat(&record.source)
                || state.candidates.iter().any(|c| c.worktree == record.source))
            && record.evidence
                == artifacts
                    .join("source-evidence")
                    .join(format!("{}-{}", record.head, state.base_commit)),
        "eviction record identity mismatch"
    );
    ensure!(
        matches!(record.phase.as_str(), "pending" | "evicted"),
        "unknown eviction phase"
    );
    ensure!(
        record.history.get(&record.head) == Some(&record.evidence) && record.history.len() <= 128,
        "incomplete checkpoint inventory"
    );
    for (head, evidence) in &record.history {
        ensure!(
            evidence
                == &artifacts
                    .join("source-evidence")
                    .join(format!("{head}-{}", state.base_commit)),
            "checkpoint identity mismatch"
        );
        crate::fold_evidence::validate_identity(evidence, head, &state.base_commit).await?;
    }
    Ok(())
}

/// Read validated eviction records without requiring the original repository.
async fn archived_records(home: &Path, id: &str) -> Result<Vec<Eviction>> {
    ensure!(crate::run::is_run_id(id), "invalid run id");
    let run = home.join("runs").join(id);
    ensure!(
        plain_dir(&run) && run.canonicalize()? == run,
        "noncanonical run ownership"
    );
    let state = RunState::load_under(id, home)?;
    let artifacts = run.join("artifacts");
    if !artifacts.try_exists()? {
        return Ok(Vec::new());
    }
    ensure!(
        plain_dir(&artifacts) && artifacts.canonicalize()? == artifacts,
        "noncanonical artifacts"
    );
    let mut records = Vec::new();
    for entry in std::fs::read_dir(&artifacts)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("artifact name")?;
        if name.starts_with("eviction-") && name.ends_with(".json") {
            let record: Eviction = serde_json::from_value(read_record(&path)?)?;
            validate_record(&state, &artifacts, &record).await?;
            records.push(record);
        }
    }
    records.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(records)
}

pub(super) async fn seat_evidence(home: &Path, id: &str) -> Result<Vec<String>> {
    let mut shown = Vec::new();
    for record in archived_records(home, id).await? {
        for (head, archive) in record.history {
            let manifest: serde_json::Value = read_record(&archive.join("manifest.json"))?;
            shown.push(format!(
                "{} head={} tree={} patch_bytes={} phase={}",
                record
                    .source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                head,
                manifest["head_tree"].as_str().context("snapshot tree")?,
                std::fs::metadata(archive.join("change.patch"))?.len(),
                record.phase
            ));
        }
    }
    Ok(shown)
}

pub(super) async fn restore_seat(
    home: &Path,
    id: &str,
    head: &str,
    destination: &Path,
) -> Result<()> {
    let records = archived_records(home, id).await?;
    let archive = records
        .iter()
        .find_map(|r| r.history.get(head))
        .context("head not found in archived checkout evidence")?;
    crate::fold_evidence::restore_snapshot(archive, destination).await
}
