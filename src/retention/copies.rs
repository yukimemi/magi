//! Checkout eviction does not retire a run or alter task/recovery lineage.
mod schema;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<String>,
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
            schema::known(&raw, &state)?,
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
            schema::known(&raw, task)?,
            "unknown source/authority fields in task {}",
            task.id
        );
    }
    Ok(tasks)
}

// Control stores can borrow a checkout independently of their owning run.
// Inspect bounded, canonical primary records rather than the UI's lossy list.
fn control_records(root: &Path) -> Result<Vec<(PathBuf, serde_json::Value)>> {
    match std::fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
        Ok(_) => ensure!(
            plain_dir(root) && root.canonicalize()? == root,
            "noncanonical control store"
        ),
    }
    let mut records = Vec::new();
    let mut bytes = 0u64;
    for (count, entry) in std::fs::read_dir(root)?.enumerate() {
        ensure!(count < 16384, "control store inspection limit exceeded");
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "json")
            && !path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with(".waiter-retry.json"))
        {
            bytes = bytes.saturating_add(std::fs::symlink_metadata(&path)?.len());
            ensure!(
                records.len() < 4096 && bytes <= 64 * 1024 * 1024,
                "control ownership inspection budget exceeded"
            );
            records.push((path.clone(), read_record(&path)?));
        }
    }
    Ok(records)
}

fn questions(home: &Path) -> Result<Vec<crate::ask::Question>> {
    control_records(&home.join("questions"))?
        .into_iter()
        .map(|(path, raw)| {
            let question: crate::ask::Question = serde_json::from_value(raw.clone())?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(question.id.as_str())
                    && question.schema > 0
                    && question.schema <= crate::ask::SCHEMA,
                "unknown question identity/schema: {}",
                path.display()
            );
            ensure!(
                schema::known(&raw, &question)?,
                "unknown question authority: {}",
                path.display()
            );
            Ok(question)
        })
        .collect()
}

// Only registered read seats may separate run authority from checkout authority.
// Candidates retain the original run-level source policy even if named like a seat.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Authority {
    Implementation,
    ReviewSeat,
}

fn authority(state: &RunState, path: &Path) -> Authority {
    if read_seat(path) && !state.candidates.iter().any(|c| c.worktree == path) {
        Authority::ReviewSeat
    } else {
        Authority::Implementation
    }
}

fn references_path(value: &str, source: &Path) -> bool {
    let path = Path::new(value);
    path.starts_with(source)
        || path.is_absolute() && path.canonicalize().is_ok_and(|p| p.starts_with(source))
}

fn path_reference(value: &serde_json::Value, path: &Path, field: &str) -> Option<String> {
    match value {
        serde_json::Value::String(s) if references_path(s, path) => Some(field.to_owned()),
        serde_json::Value::Array(a) => a
            .iter()
            .enumerate()
            .find_map(|(i, v)| path_reference(v, path, &format!("{field}[{i}]"))),
        serde_json::Value::Object(o) => o
            .iter()
            .find_map(|(k, v)| path_reference(v, path, &format!("{field}.{k}"))),
        _ => None,
    }
}

fn unborrowed(home: &Path, state: &RunState, path: &Path, now: Timestamp) -> Result<()> {
    let seat = authority(state, path) == Authority::ReviewSeat;
    for other in history(home)? {
        if other.id != state.id {
            ensure!(
                other.worktree_root() != state.worktree_root(),
                "run root shared with {}",
                other.id
            );
            // Run selection retains implementation provenance, not every read
            // seat. Concrete dependencies below always win over this distinction.
            ensure!(
                seat || !other
                    .pr_recovery
                    .as_ref()
                    .is_some_and(|p| p.source_run.as_ref() == Some(&state.id)
                        || p.source_proof.as_ref().is_some_and(|p| p.run == state.id)),
                "implementation authority: selected recovery source for run {}",
                other.id
            );
        }
        // Include the owning run: a candidate/recovery consumer may borrow one
        // of its own review paths. Seat conversation IDs contain no cwd and are
        // historical on non-resumable runs; they are not filesystem dependencies.
        if (other.id != state.id || seat)
            && let Some(field) = path_reference(&serde_json::to_value(&other)?, path, "run")
        {
            anyhow::bail!("checkout path dependency: run {} field {field}", other.id);
        }
    }
    for task in tasks(home)? {
        if let Some(field) = path_reference(&serde_json::to_value(&task)?, path, "task") {
            anyhow::bail!("checkout path dependency: task {} field {field}", task.id);
        }
    }
    let store = crate::ask::Questions::at(home.join("questions"));
    for question in questions(home)? {
        if question
            .cwd
            .as_ref()
            .is_some_and(|cwd| references_path(cwd, path))
        {
            let lease = store.lease_path(&question.id);
            let leased = match std::fs::symlink_metadata(&lease) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
                Ok(_) => {
                    let lease: crate::ask::Lease = serde_json::from_value(read_record(&lease)?)?;
                    lease.fresh(now)
                }
            };
            ensure!(
                !leased
                    && !question.status.open()
                    && (question.status != crate::ask::QuestionStatus::Answered
                        || question.answer_delivered),
                "continuation path dependency: question {} field cwd",
                question.id
            );
        }
    }
    for (file, raw) in control_records(&home.join("talks"))? {
        let talk: crate::talk::Talk = serde_json::from_value(raw.clone())?;
        ensure!(
            file.file_stem().and_then(|s| s.to_str()) == Some(talk.id.as_str())
                && talk.schema == crate::talk::SCHEMA,
            "unknown talk identity/schema: {}",
            file.display()
        );
        // Talk denies unknown fields itself. Inspect the raw value as well:
        // retired, known recommendation payloads are intentionally IgnoredAny.
        if let Some(field) = path_reference(&raw, path, "talk") {
            anyhow::bail!("checkout path dependency: talk {} field {field}", talk.id);
        }
    }
    Ok(())
}

// The owner lock and a full Dead judgment precede maintenance acquisition.
// Its exclusive lease then prevents driver starts; judging that same lease
// again would mistake this janitor for a live driver. Recheck daemon claims
// and all selection/activity protections while the caller holds the witness.
fn eligible(
    home: &Path,
    state: &RunState,
    now: Timestamp,
    disk: &crate::config::Disk,
    _maintenance: &crate::driver::ExecutionLease,
    path: &Path,
) -> Result<()> {
    let seat = authority(state, path) == Authority::ReviewSeat;
    ensure!(
        !state.status.resumable(),
        "resume contract: resumable run retains materialized seat checkouts"
    );
    ensure!(
        !crate::daemon::is_working_on(home, &state.id, now),
        "run acquired a daemon claim during maintenance"
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
        seat || (state.pr.is_none()
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
            && state.interrupted_implementations.is_empty()),
        "implementation authority: public authority, approval or recovery checkpoint"
    );
    if seat {
        let marker = home
            .join("runs")
            .join(&state.id)
            .join("artifacts")
            .join(format!(
                "restore-{}.pending",
                path.file_name().context("seat name")?.to_string_lossy()
            ));
        ensure!(
            matches!(std::fs::symlink_metadata(marker), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "restoration checkpoint: incomplete execution setup retained"
        );
        let key = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("seat name")?;
        ensure!(
            !state.active.contains_key(key),
            "continuation lease: seat {key} has an unfinished answer"
        );
    }
    let questions = questions(home)?;
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
                && (seat
                    || !task
                        .pr_recovery
                        .as_ref()
                        .is_some_and(|p| p.source_run.as_ref() == Some(&state.id)
                            || p.source_proof.as_ref().is_some_and(|p| p.run == state.id))),
            "resume/source authority: selected recovery source for task {}",
            task.id
        );
    }
    ensure!(
        !questions
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
    unborrowed(home, state, path, now)?;
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
        // Complete snapshots, including original commit/tree/blob identities,
        // prove reproducibility below; review-round membership is not proof.
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
    exact_inventory(path, &head, &[]).await?;
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

async fn exact_inventory(path: &Path, head: &str, products: &[String]) -> Result<()> {
    for product in products {
        crate::worktree_setup::valid_destination(product)?;
        ensure!(
            Path::new(product)
                .components()
                .any(|c| matches!(c, std::path::Component::Normal(_))),
            "unknown setup product root"
        );
    }
    let products: Vec<_> = products.iter().map(|p| path.join(p)).collect();
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
                expected.contains(&file)
                    || products
                        .iter()
                        .any(|p| file.starts_with(p) || p.starts_with(&file)),
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
            reconcile_pending(home, &state, disk, now, &_driver).await?;
            if !root.try_exists()? {
                return Ok(());
            }
            if std::fs::read_dir(&root)?.next().is_none() {
                unborrowed(home, &state, &root, now)?;
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
                    eligible(home, &state, now, disk, &_driver, path)?;
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
                    let mut heads = reflog_heads(&admin_contents)?;
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
                    for archive in history.values() {
                        crate::fold_evidence::validate_execution_snapshot(archive).await?;
                    }
                    let ledger = eviction_ledger(&artifacts, path)?;
                    let mut record = Eviction {
                        version: 2,
                        run: state.id.clone(),
                        source: path.clone(),
                        head: head.clone(),
                        base: Some(state.base_commit.clone()),
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
                            matches!(previous.version, 1 | 2)
                                && record_base(&previous)? == state.base_commit
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
                        // Retry historical intents without upgrading their durable identity.
                        record.version = previous.version;
                        record.base = previous.base;
                    }
                    eligible(
                        home,
                        &RunState::load_under(&state.id, home)?,
                        now,
                        disk,
                        &_driver,
                        path,
                    )?;
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
                    eligible(
                        home,
                        &RunState::load_under(&state.id, home)?,
                        now,
                        disk,
                        &_driver,
                        path,
                    )?;
                    unborrowed(home, &state, path, now)?;
                    let pending = serde_json::to_vec_pretty(&record)?;
                    let previous_bytes = if ledger.try_exists()? {
                        std::fs::metadata(&ledger)?.len()
                    } else {
                        0
                    };
                    ensure!(
                        eviction_storage(&artifacts)?
                            .saturating_sub(previous_bytes)
                            .saturating_add(pending.len() as u64)
                            <= EVIDENCE_BUDGET,
                        "source evidence per-run storage limit exceeded"
                    );
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
                        crate::fold_evidence::validate_execution_snapshot(archive).await?;
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

fn eviction_storage(artifacts: &Path) -> Result<u64> {
    let mut bytes = crate::fold_evidence::snapshot_storage(&artifacts.join("source-evidence"))?;
    for (count, entry) in std::fs::read_dir(artifacts)?.enumerate() {
        ensure!(count < 4096, "eviction record inspection limit exceeded");
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("eviction-") && s.ends_with(".json"))
        {
            let _ = read_record(&path)?;
            bytes = bytes.saturating_add(std::fs::metadata(path)?.len());
        }
    }
    Ok(bytes)
}

// A rematerialized seat starts another generation. Never replace a completed
// generation's ownership evidence; a pending generation must still match exactly.
fn eviction_ledger(artifacts: &Path, source: &Path) -> Result<PathBuf> {
    let seat = source.file_name().context("seat name")?.to_string_lossy();
    for generation in 1..=128 {
        let name = if generation == 1 {
            format!("eviction-{seat}.json")
        } else {
            format!("eviction-{seat}-{generation}.json")
        };
        let path = artifacts.join(name);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let previous: Eviction = serde_json::from_value(read_record(&path)?)?;
        ensure!(
            previous.source == source,
            "eviction generation source mismatch"
        );
        if previous.phase == "pending" {
            return Ok(path);
        }
        ensure!(
            previous.phase == "evicted",
            "unknown eviction generation phase"
        );
    }
    anyhow::bail!("eviction generation inspection limit exceeded")
}

fn validate_ledger_name(path: &Path, record: &Eviction) -> Result<()> {
    let seat = record
        .source
        .file_name()
        .and_then(|s| s.to_str())
        .context("seat name")?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("eviction record name")?;
    let prefix = format!("eviction-{seat}");
    if name == format!("{prefix}.json") {
        return Ok(());
    }
    let generation = name
        .strip_prefix(&format!("{prefix}-"))
        .and_then(|n| n.strip_suffix(".json"))
        .context("eviction generation name mismatch")?;
    let number: usize = generation.parse()?;
    ensure!(
        (2..=128).contains(&number) && generation == number.to_string(),
        "unknown eviction generation"
    );
    Ok(())
}

// Reconcile an interrupted removal only against valid immutable evidence and
// absence from BOTH the filesystem and Git registration. Never infer success
// from a missing path or overwrite a mismatched existing source.
async fn reconcile_pending(
    home: &Path,
    state: &RunState,
    disk: &crate::config::Disk,
    now: Timestamp,
    maintenance: &crate::driver::ExecutionLease,
) -> Result<()> {
    let artifacts = home.join("runs").join(&state.id).join("artifacts");
    if !artifacts.try_exists()? {
        return Ok(());
    }
    ensure!(
        plain_dir(&artifacts) && artifacts.canonicalize()? == artifacts,
        "noncanonical artifacts"
    );
    let mut entries = std::fs::read_dir(&artifacts)?
        .take(4097)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= 4096,
        "eviction record inspection limit exceeded"
    );
    entries.sort_by_key(|e| (e.file_name().len(), e.file_name()));
    for entry in entries {
        let path = entry.path();
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
        let mut record: Eviction = serde_json::from_value(read_record(&path)?)?;
        validate_ledger_name(&path, &record)?;
        validate_record(state, &artifacts, &record).await?;
        if record.phase == "pending" && !record.source.try_exists()? {
            let list = git::git(&state.repo, &["worktree", "list", "--porcelain"]).await?;
            if list
                .lines()
                .filter_map(|l| l.strip_prefix("worktree "))
                .any(|p| Path::new(p) == record.source)
            {
                eligible(home, state, now, disk, maintenance, &record.source)?;
                unborrowed(home, state, &record.source, now)?;
                for archive in record.history.values() {
                    crate::fold_evidence::validate_execution_snapshot(archive).await?;
                }
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

fn record_base(record: &Eviction) -> Result<String> {
    ensure!(
        matches!(record.head.len(), 40 | 64) && record.head.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid eviction head identity"
    );
    let base = match &record.base {
        Some(base) => base.clone(),
        None => {
            ensure!(record.version == 1, "missing eviction base identity");
            record
                .evidence
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix(&format!("{}-", record.head)))
                .context("missing historical eviction base identity")?
                .to_owned()
        }
    };
    ensure!(
        matches!(base.len(), 40 | 64) && base.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid eviction base identity"
    );
    Ok(base)
}

async fn validate_record(state: &RunState, artifacts: &Path, record: &Eviction) -> Result<()> {
    let base = record_base(record)?;
    ensure!(
        matches!(record.version, 1 | 2)
            && record.run == state.id
            && record.source.parent() == Some(state.worktree_root().as_path())
            && (read_seat(&record.source)
                || state.candidates.iter().any(|c| c.worktree == record.source))
            && record.evidence
                == artifacts
                    .join("source-evidence")
                    .join(format!("{}-{base}", record.head)),
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
    ensure!(
        reflog_heads(&record.admin_contents)?
            .iter()
            .all(|oid| record.history.contains_key(oid)),
        "incomplete reflog checkpoint inventory"
    );
    for (head, evidence) in &record.history {
        ensure!(
            evidence
                == &artifacts
                    .join("source-evidence")
                    .join(format!("{head}-{base}")),
            "checkpoint identity mismatch"
        );
        crate::fold_evidence::validate_identity(evidence, head, &base).await?;
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
    let mut entries = std::fs::read_dir(&artifacts)?
        .take(4097)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= 4096,
        "eviction record inspection limit exceeded"
    );
    entries.sort_by_key(|e| (e.file_name().len(), e.file_name()));
    for entry in entries {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("artifact name")?;
        if name.starts_with("eviction-") && name.ends_with(".json") {
            let record: Eviction = serde_json::from_value(read_record(&path)?)?;
            validate_ledger_name(&path, &record)?;
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

// Called under the execution/owner lease and repository coordination, before
// graph dispatch or continuation delivery. Existing entities are never replaced.
pub(super) async fn restore_execution_seats(home: &Path, state: &RunState) -> Result<()> {
    let mut latest = BTreeMap::new();
    for record in archived_records(home, &state.id).await? {
        if authority(state, &record.source) == Authority::ReviewSeat {
            latest.insert(record.source.clone(), record);
        }
    }
    for (path, record) in latest {
        let marker = home
            .join("runs")
            .join(&state.id)
            .join("artifacts")
            .join(format!(
                "restore-{}.pending",
                path.file_name().context("seat name")?.to_string_lossy()
            ));
        ensure!(
            matches!(std::fs::symlink_metadata(&marker), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "interrupted execution restoration requires inspection: {}",
            marker.display()
        );
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink() && path.canonicalize()? == path,
                    "archived seat replaced by noncanonical entity"
                );
                ensure!(
                    worktrees(state).await?.contains(&path),
                    "archived seat replaced by unregistered entity"
                );
                let existing_head = git::rev_parse(&path, "HEAD").await?;
                ensure!(
                    (existing_head == record.head
                        || existing_head == state.base_commit
                        || state.reviews.iter().any(|r| r.head == existing_head))
                        && !git::git_raw(&path, &["symbolic-ref", "-q", "HEAD"])
                            .await?
                            .ok(),
                    "archived seat identity changed; inspect existing checkout before execution"
                );
                ensure!(
                    git::git(&path, &["write-tree"]).await?
                        == git::rev_parse(&path, "HEAD^{tree}").await?,
                    "archived seat index changed before execution"
                );
                super::reconcile::execution_evidence(state, std::slice::from_ref(&path)).await?;
                tracked_base(&path, &existing_head).await?;
                let products = crate::worktree_setup::withheld_paths(&path).await;
                exact_inventory(&path, &existing_head, &products).await?;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        ensure!(
            record.phase == "evicted",
            "pending eviction must reconcile before execution restoration"
        );
        let root = state.worktree_root();
        if !root.try_exists()? {
            let parent = root.parent().context("run root parent")?;
            ensure!(
                plain_dir(parent) && parent.canonicalize()? == parent,
                "noncanonical restore parent"
            );
            std::fs::create_dir(&root)?;
        }
        ensure!(
            plain_dir(&root) && root.canonicalize()? == root,
            "noncanonical restore root"
        );
        let registered = git::git(&state.repo, &["worktree", "list", "--porcelain"]).await?;
        ensure!(
            !registered
                .lines()
                .filter_map(|l| l.strip_prefix("worktree "))
                .any(|p| Path::new(p) == path),
            "archived seat still has a Git registration"
        );
        crate::fold_evidence::import_snapshot(&record.evidence, &state.repo).await?;
        crate::fold_evidence::atomic_write(&marker, record.head.as_bytes())?;
        git::worktree_add_detached(&state.repo, &path, &record.head).await?;
        crate::worktree_setup::prepare(&state.config, &state.repo, &path).await?;
        ensure!(
            git::rev_parse(&path, "HEAD").await? == record.head,
            "restored seat HEAD changed during setup"
        );
        std::fs::remove_file(marker)?;
    }
    Ok(())
}

pub(super) async fn seat_heads(home: &Path, id: &str) -> Result<BTreeMap<String, String>> {
    let mut heads = BTreeMap::new();
    for record in archived_records(home, id).await? {
        for head in record.history.keys() {
            heads.insert(
                head.clone(),
                record
                    .source
                    .file_name()
                    .context("seat name")?
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok(heads)
}

pub(super) async fn seat_diff(home: &Path, id: &str, head: &str) -> Result<Vec<u8>> {
    let records = archived_records(home, id).await?;
    let archive = records
        .iter()
        .find_map(|r| r.history.get(head))
        .context("head not found in archived checkout evidence")?;
    super::source_diff(archive).await
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

// `reflog show %H` exposes only new OIDs. A non-rewritten deletion can
// leave an otherwise unreachable source exclusively on the old side.
fn reflog_heads(admin: &BTreeMap<PathBuf, Vec<u8>>) -> Result<std::collections::BTreeSet<String>> {
    let mut heads = std::collections::BTreeSet::new();
    let mut logs = 0;
    for (path, bytes) in admin.iter().filter(|(p, _)| p.ends_with("logs/HEAD")) {
        logs += 1;
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            let mut fields = line.split(|b| *b == b' ');
            for _ in 0..2 {
                let oid = std::str::from_utf8(fields.next().context("missing reflog OID")?)?;
                ensure!(
                    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid raw reflog OID: {}",
                    path.display()
                );
                if !oid.bytes().all(|b| b == b'0') {
                    heads.insert(oid.to_owned());
                }
                ensure!(heads.len() <= 128, "source checkpoint limit exceeded");
            }
        }
    }
    ensure!(logs == 1, "missing or ambiguous raw HEAD reflog");
    Ok(heads)
}
