//! Explicit, bounded reconciliation of historical blanket source retention.
use super::{EmptyProof, plain_dir, prove_empty, worktrees};
use crate::{
    git,
    queue::{Queue, Task},
    run::{Liveness, RunState},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
};

const VERSION: u32 = 1;
const LIMIT: usize = 5;

#[derive(Serialize, Deserialize)]
struct Plan {
    version: u32,
    id: String,
    task: Task,
    authority: String,
    at: jiff::Timestamp,
    history: BTreeMap<String, serde_json::Value>,
    snapshot: BTreeMap<PathBuf, String>,
    refs: String,
    candidates: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    run: String,
    paths: Vec<PathBuf>,
    evidence: BTreeMap<PathBuf, String>,
    refusal: Option<String>,
    selected: bool,
}

fn plain_file(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "not a plain file: {}",
        path.display()
    );
    ensure!(
        path.canonicalize()? == path,
        "noncanonical file: {}",
        path.display()
    );
    Ok(())
}

async fn fingerprint(repo: &Path, path: &Path) -> Result<String> {
    plain_file(path)?;
    git::git(
        repo,
        &[
            "hash-object",
            "--no-filters",
            path.to_str().context("non UTF-8 artifact")?,
        ],
    )
    .await
}

// Reports are never removed. Their exact inventory is nevertheless part of the
// operator's approval, so a new checkpoint or a changed report invalidates it.
async fn inventory(repo: &Path, root: &Path, out: &mut BTreeMap<PathBuf, String>) -> Result<()> {
    if !root.try_exists()? {
        return Ok(());
    }
    ensure!(
        plain_dir(root) && root.canonicalize()? == root,
        "noncanonical artifact directory"
    );
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        ensure!(out.len() < 100_000, "artifact inventory too large");
        out.insert(dir.clone(), "directory".into());
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            ensure!(
                !meta.file_type().is_symlink(),
                "symlink artifact: {}",
                path.display()
            );
            if meta.is_dir() {
                dirs.push(path);
            } else {
                // These permanent lock inodes carry no source or checkpoint.
                if path.parent() == Some(root)
                    && matches!(
                        path.file_name().and_then(|s| s.to_str()),
                        Some("driver.lock" | "pr-owner.lock")
                    )
                {
                    continue;
                }
                ensure!(out.len() < 100_000, "artifact inventory too large");
                out.insert(path.clone(), fingerprint(repo, &path).await?);
            }
        }
    }
    Ok(())
}

async fn snapshot(home: &Path, task: &Task) -> Result<BTreeMap<PathBuf, String>> {
    let mut out = BTreeMap::new();
    let task_path = Queue::at(home.join("queue")).path_of(&task.id);
    out.insert(
        task_path.clone(),
        fingerprint(&task.repo, &task_path).await?,
    );
    for id in &task.runs {
        inventory(&task.repo, &home.join("runs").join(id), &mut out).await?;
    }
    inventory(&task.repo, &home.join("questions"), &mut out).await?;
    Ok(out)
}

fn questions(home: &Path, task: &Task) -> Result<()> {
    let root = home.join("questions");
    if !root.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "json") {
            plain_file(&path)?;
            let q: crate::ask::Question = serde_json::from_slice(&std::fs::read(path)?)?;
            ensure!(
                !(q.status.open()
                    && (q.run == task.id
                        || task.runs.contains(&q.run)
                        || task.blocked_by.contains(&q.id))),
                "unanswered owner decision"
            );
        }
    }
    Ok(())
}

fn marker(home: &Path, task: &Task, id: &str) -> Result<()> {
    let path = home.join("runs").join(id).join("retain-recovery-sources");
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            plain_file(&path)?;
            ensure!(
                std::fs::read(&path)? == task.id.as_bytes(),
                "retention marker is not this task's historical blanket marker"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn dead(home: &Path, state: &RunState) -> Result<()> {
    ensure!(
        state.liveness_under(
            home,
            crate::daemon::is_working_on(home, &state.id, jiff::Timestamp::now())
        ) == Liveness::Dead,
        "run live or ownership unknown"
    );
    Ok(())
}

fn eligible(home: &Path, task: &Task, state: &RunState, authority: &str) -> Result<()> {
    let raw: serde_json::Value = read(&home.join("runs").join(&state.id).join("run.json"))?;
    let known = serde_json::to_value(state)?;
    ensure!(
        raw.get("schema").and_then(|v| v.as_u64()) == Some(u64::from(crate::run::SCHEMA)),
        "unknown or legacy on-disk run schema"
    );
    ensure!(
        raw.as_object()
            .context("run must be an object")?
            .iter()
            .all(|(key, value)| known.get(key) == Some(value)),
        "unknown or changed run fields"
    );
    ensure!(
        state.id != authority && state.repo == task.repo,
        "authority or foreign repository"
    );
    ensure!(state.schema == crate::run::SCHEMA, "unsupported run schema");
    ensure!(
        state.prep_complete != Some(true)
            && matches!(
                state.status,
                crate::run::RunStatus::Prep
                    | crate::run::RunStatus::Failed
                    | crate::run::RunStatus::Blocked
            ),
        "not an obsolete startup attempt"
    );

    ensure!(
        !task
            .resume_override
            .as_ref()
            .is_some_and(|p| p.pinned_run.as_ref() == Some(&state.id))
            && !task.replacement.as_ref().is_some_and(|p| p.run == state.id),
        "operator-pinned source"
    );
    if let Some(s) = &task.pr_recovery {
        ensure!(
            s.source_run.as_ref() != Some(&state.id)
                && !s.source_proof.as_ref().is_some_and(|p| p.run == state.id),
            "selected provenance source"
        );
    }
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
            && state.reviews.is_empty()
            && state.reviewed_commits.is_none()
            && state.recovery.is_none()
            && state.base_sync.is_none()
            && state.graft.is_none()
            && state.tally.is_none()
            && state.merge.is_none()
            && state.merge_repairs.is_empty()
            && state.candidate_preservation.is_empty()
            && state.interrupted_implementations.is_empty()
            && state.withheld.is_empty()
            && state.operator_fixes.is_empty()
            && state.seats.is_empty()
            && state.active.is_empty()
            && state.jobs.is_empty()
            && state.gate.is_empty()
            && !state.gate_ran
            && state.gate_head.is_none()
            && state.pre_gate.is_empty()
            && state.pre_gate_commit.is_none()
            && state.recovery_config.is_none()
            && state.seat_history.is_empty()
            && state.gate_fixes.is_empty()
            && state.rebase_fixes.is_empty()
            && state.change_summary.is_none()
            && state.replaces.is_none()
            && state.github_text.is_none()
            && state.contested_handoff.is_none()
            && state.land_reruns.is_none()
            && state.release_bump.is_none()
            && state.bump_decision_attempts.is_empty()
            && state.judgements.is_empty()
            && state.deliberation.is_empty()
            && state.votes.is_empty()
            && !state.released(),
        "implementation, PR ownership or recovery checkpoint"
    );
    ensure!(
        state
            .candidates
            .iter()
            .all(|c| c.commits == 0 && c.empty && c.files == 0 && !c.folded),
        "recorded implementation"
    );
    marker(home, task, &state.id)?;
    Ok(())
}

async fn strict_proof(
    home: &Path,
    state: &RunState,
    authority: &str,
    previous: Option<&[PathBuf]>,
) -> Result<EmptyProof> {
    let paths = worktrees(state).await?;
    let root = state.worktree_root();
    ensure!(
        plain_dir(&root) && root.canonicalize()? == root,
        "missing or noncanonical startup root"
    );
    let list = git::git(&state.repo, &["worktree", "list", "--porcelain"]).await?;
    for path in list
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
    {
        if path.starts_with(&root) {
            ensure!(
                paths.contains(&path),
                "unmatched or missing worktree registration"
            );
        }
    }
    for entry in std::fs::read_dir(&root)? {
        ensure!(
            paths.contains(&entry?.path()),
            "unknown or unregistered startup artifact"
        );
    }
    let mut recorded: Vec<PathBuf> = state
        .candidates
        .iter()
        .map(|c| c.worktree.clone())
        .collect();
    for event in state.events.iter().filter(|e| e.node == "prep-intent") {
        let intent: serde_json::Value = serde_json::from_str(&event.message)?;
        let path: PathBuf =
            serde_json::from_value(intent.get("worktree").context("missing prep path")?.clone())?;
        ensure!(
            path.parent() == Some(root.as_path()),
            "prep path outside run root"
        );
        let branch = intent
            .get("branch")
            .and_then(|s| s.as_str())
            .context("missing prep branch")?;
        ensure!(
            branch.starts_with(&format!("magi/{}/", state.short())),
            "unknown prep branch"
        );
        ensure!(
            git::rev_parse(&state.repo, branch).await? == state.base_commit,
            "prep branch retains source"
        );
        recorded.push(path);
    }
    ensure!(
        paths.iter().all(|p| recorded.contains(p)),
        "worktree without startup evidence"
    );
    if let Some(previous) = previous {
        ensure!(
            paths.iter().all(|p| previous.contains(p)),
            "new worktree since approval"
        );
    } else {
        ensure!(!paths.is_empty(), "no worktrees to prove empty");
    }
    // Branch reflogs can retain an implementation even when HEAD was reset.
    let prefix = format!("refs/heads/magi/{}/", state.short());
    let refs = git::git(
        &state.repo,
        &["for-each-ref", "--format=%(refname)", &prefix],
    )
    .await?;
    for name in refs.lines() {
        ensure!(
            git::rev_parse(&state.repo, name).await? == state.base_commit,
            "candidate ref retains implementation"
        );
        for head in git::git(&state.repo, &["reflog", "show", "--format=%H", name])
            .await?
            .lines()
        {
            ensure!(
                git::ancestry(&state.repo, head, &state.base_commit).await?,
                "branch reflog retains implementation"
            );
        }
    }
    for c in &state.candidates {
        ensure!(
            git::rev_parse(&state.repo, &c.branch).await? == state.base_commit,
            "candidate source changed"
        );
    }
    prove_empty(home, state, authority).await
}

async fn evidence(state: &RunState, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, String>> {
    let mut result = BTreeMap::new();
    for path in paths {
        let admin = git::git(path, &["rev-parse", "--absolute-git-dir"]).await?;
        let admin = PathBuf::from(admin.trim());
        result.insert(
            path.join(".git"),
            fingerprint(&state.repo, &path.join(".git")).await?,
        );
        for required in ["HEAD", "index", "commondir", "gitdir", "logs/HEAD"] {
            plain_file(&admin.join(required)).context("missing startup Git evidence")?;
        }
        let common = git::git(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        ensure!(
            common
                == git::git(
                    &state.repo,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"]
                )
                .await?,
            "worktree repository identity changed"
        );
        for entry in std::fs::read_dir(&admin)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "refs" {
                ensure!(
                    plain_dir(&entry.path()) && std::fs::read_dir(entry.path())?.next().is_none(),
                    "retained worktree-local refs"
                );
            } else if name == "logs" {
                ensure!(
                    plain_dir(&entry.path()),
                    "unknown Git administration directory"
                );
                for log in std::fs::read_dir(entry.path())? {
                    ensure!(log?.file_name() == "HEAD", "unknown Git reflog artifact");
                }
            } else {
                ensure!(
                    matches!(
                        name.to_str(),
                        Some(
                            "HEAD"
                                | "ORIG_HEAD"
                                | "index"
                                | "commondir"
                                | "gitdir"
                                | "config.worktree"
                                | "magi-output.lock"
                                | "magi-setup-paths"
                                | "magi-setup-exclude"
                        )
                    ),
                    "unknown Git administration artifact: {}",
                    name.to_string_lossy()
                );
            }
        }
        inventory(&state.repo, &admin, &mut result).await?;
    }
    let prefix = format!("refs/heads/magi/{}/", state.short());
    for name in git::git(
        &state.repo,
        &["for-each-ref", "--format=%(refname)", &prefix],
    )
    .await?
    .lines()
    {
        result.insert(
            PathBuf::from(name),
            git::git(
                &state.repo,
                &["reflog", "show", "--format=%H %gD %gs", name],
            )
            .await?,
        );
    }
    Ok(result)
}

fn durable_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    std::fs::File::open(path.parent().context("missing audit parent")?)?.sync_all()?;
    Ok(())
}

fn read<T: serde::de::DeserializeOwned + Serialize>(path: &Path) -> Result<T> {
    plain_file(path)?;
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let value: T = serde_json::from_value(raw.clone())?;
    ensure!(
        serde_json::to_value(&value)? == raw,
        "unknown or incomplete audit format: {}",
        path.display()
    );
    Ok(value)
}

async fn refs(repo: &Path) -> Result<String> {
    git::git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]).await
}

async fn plan_id(repo: &Path, plan: &Plan) -> Result<String> {
    use tokio::io::AsyncWriteExt as _;
    let mut canonical = serde_json::to_value(plan)?;
    canonical["id"] = serde_json::Value::String(String::new());
    let mut child = tokio::process::Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().context("missing plan hash input")?;
    input.write_all(&serde_json::to_vec(&canonical)?).await?;
    drop(input);
    let output = child.wait_with_output().await?;
    ensure!(output.status.success(), "cannot fingerprint approval plan");
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Save a reviewable plan, or apply exactly one previously saved approval.
/// This does not release the task, change its selection, or contact the forge.
pub async fn run(
    home: &Path,
    task_id: &str,
    authority: &str,
    apply: Option<&str>,
) -> Result<PathBuf> {
    ensure!(cfg!(unix), "retention reconciliation requires Unix leases");
    ensure!(
        crate::run::is_run_id(authority),
        "authority must be a full run id"
    );
    let queue = Queue::at(home.join("queue"));
    let id = queue.resolve_id(task_id)?;
    let _claim = queue
        .claim(&id)
        .context("task is claimed; park it before maintenance")?;
    let task = queue.get(&id)?;
    let raw: serde_json::Value = read(&queue.path_of(&id))?;
    let known = serde_json::to_value(&task)?;
    ensure!(
        raw.get("schema").and_then(|s| s.as_u64()) == Some(u64::from(crate::queue::SCHEMA))
            && raw
                .as_object()
                .context("task must be an object")?
                .iter()
                .all(|(key, value)| known.get(key) == Some(value)),
        "unknown task format; refusing reconciliation"
    );
    ensure!(
        task.status != crate::queue::TaskStatus::Running,
        "park task before maintenance"
    );
    ensure!(
        task.runs.contains(&authority.to_owned()),
        "authority is not in task history"
    );
    for id in &task.runs {
        ensure!(crate::run::is_run_id(id), "invalid historical run id");
    }
    questions(home, &task)?;
    let source = RunState::load_under(authority, home)?;
    ensure!(
        source.repo == task.repo
            && source.retired_to.is_none()
            && !source.released()
            && source.candidates.iter().any(|c| c.commits > 0 && !c.empty),
        "authority must be a retained implementation in this repository"
    );
    let mut implementation = false;
    for candidate in &source.candidates {
        let head = git::rev_parse(&task.repo, &candidate.branch).await?;
        implementation |= head != source.base_commit
            && git::ancestry(&task.repo, &source.base_commit, &head).await?;
    }
    ensure!(
        implementation,
        "authority has no verifiable retained implementation"
    );
    if let Some(selected) = &task.pr_recovery {
        if let Some(run) = &selected.source_run {
            ensure!(
                run == authority,
                "authority differs from selected source lineage"
            );
        }
        let head = selected.source_head.as_ref().unwrap_or(&selected.head);
        git::rev_parse(&task.repo, head)
            .await
            .context("selected source is unavailable")?;
        let mut related = false;
        for candidate in &source.candidates {
            let tip = git::rev_parse(&task.repo, &candidate.branch).await?;
            related |= git::ancestry(&task.repo, &tip, head).await?
                || git::ancestry(&task.repo, head, &tip).await?;
        }
        ensure!(
            related,
            "authority has no candidate related to selected source"
        );
    }
    let root = home.join("retention-reconciliations");
    std::fs::create_dir_all(&root)?;
    ensure!(
        plain_dir(&root) && root.canonicalize()? == root,
        "noncanonical audit root"
    );
    if let Some(operation) = apply {
        ensure!(
            matches!(operation.len(), 40 | 64) && operation.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid plan id"
        );
        let dir = root.join(operation);
        let plan: Plan = read(&dir.join("plan.json"))?;
        ensure!(
            plan_id(&task.repo, &plan).await? == operation,
            "saved plan content changed"
        );
        ensure!(
            plan.version == VERSION
                && plan.id == operation
                && plan.task.id == id
                && plan.authority == authority,
            "plan identity mismatch"
        );
        apply_plan(home, &queue, &task, &plan, &dir, || Ok(())).await?;
        return Ok(dir);
    }
    let mut plan = Plan {
        version: VERSION,
        id: String::new(),
        task: task.clone(),
        authority: authority.into(),
        at: jiff::Timestamp::now(),
        history: BTreeMap::new(),
        snapshot: snapshot(home, &task).await?,
        refs: refs(&task.repo).await?,
        candidates: Vec::new(),
    };
    let mut count = 0;
    for id in task.runs.iter().collect::<std::collections::BTreeSet<_>>() {
        let state = RunState::load_under(id, home)?;
        ensure!(
            &state.id == id && state.repo == task.repo,
            "historical identity mismatch"
        );
        plan.history.insert(
            id.clone(),
            read(&home.join("runs").join(id).join("run.json"))?,
        );
        let result = async {
            ensure!(state.retired_to.is_none(), "already retired");
            let _owner = crate::pr_head::owner_lock(home, id)?;
            dead(home, &state)?;
            let _driver = crate::driver::maintenance(home, id)?;
            eligible(home, &task, &state, authority)?;
            let proof = strict_proof(home, &state, &plan.authority, None).await?;
            let evidence = evidence(&state, &proof.paths).await?;
            Ok::<_, anyhow::Error>((proof.paths, evidence))
        }
        .await;
        let (paths, evidence, refusal) = match result {
            Ok((paths, evidence)) => (paths, evidence, None),
            Err(e) => (Vec::new(), BTreeMap::new(), Some(format!("{e:#}"))),
        };
        let selected = refusal.is_none() && count < LIMIT;
        count += usize::from(selected);
        plan.candidates.push(Entry {
            run: id.clone(),
            paths,
            evidence,
            refusal,
            selected,
        });
    }
    ensure!(
        snapshot(home, &task).await? == plan.snapshot && refs(&task.repo).await? == plan.refs,
        "artifacts changed while planning"
    );
    ensure!(
        serde_json::to_value(queue.get(&id)?)? == serde_json::to_value(&task)?,
        "task changed while planning"
    );
    plan.id = plan_id(&task.repo, &plan).await?;
    let dir = root.join(&plan.id);
    std::fs::create_dir(&dir)?;
    durable_new(&dir.join("plan.json"), &plan)?;
    Ok(dir.join("plan.json"))
}

// Metadata witnesses close the gap between expensive hashing/Git inspection
// and the short task-write section, without launching Git while holding it.
#[cfg(unix)]
fn witnesses(roots: &[PathBuf]) -> Result<BTreeMap<PathBuf, Vec<u64>>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut result = BTreeMap::new();
    let mut todo = roots.to_vec();
    while let Some(path) = todo.pop() {
        let m = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        ensure!(result.len() < 200_000, "final artifact witness too large");
        result.insert(
            path.clone(),
            vec![
                m.dev(),
                m.ino(),
                m.mode().into(),
                m.size(),
                m.mtime() as u64,
                m.mtime_nsec() as u64,
                m.ctime() as u64,
                m.ctime_nsec() as u64,
            ],
        );
        if m.is_dir() {
            for entry in std::fs::read_dir(path)? {
                todo.push(entry?.path());
            }
        }
    }
    Ok(result)
}

#[cfg(not(unix))]
fn witnesses(_: &[PathBuf]) -> Result<BTreeMap<PathBuf, Vec<u64>>> {
    anyhow::bail!("artifact witnesses require Unix")
}

fn fenced_state(plan: &Plan, id: &str) -> Result<RunState> {
    let mut state: RunState = serde_json::from_value(
        plan.history
            .get(id)
            .context("missing approved run")?
            .clone(),
    )?;
    state.retired_to = Some(plan.authority.clone());
    state.updated_at = plan.at;
    state.events.push(crate::run::Event {
        at: plan.at,
        node: "retention-reconciliation".into(),
        message: format!(
            "approved operation {}; authority {}; all task sources retained",
            plan.id, plan.authority
        ),
    });
    Ok(state)
}

async fn apply_plan(
    home: &Path,
    queue: &Queue,
    task: &Task,
    plan: &Plan,
    dir: &Path,
    before_removal: impl FnOnce() -> Result<()>,
) -> Result<()> {
    ensure!(
        serde_json::to_value(task)? == serde_json::to_value(&plan.task)?,
        "task changed; create a new plan"
    );
    ensure!(
        plan.candidates.iter().filter(|e| e.selected).count() <= LIMIT,
        "plan exceeds batch limit"
    );
    let chosen: Vec<_> = plan.candidates.iter().filter(|e| e.selected).collect();
    let mut owners = Vec::new();
    let mut drivers = Vec::new();
    for e in &chosen {
        ensure!(
            e.refusal.is_none() && e.run != plan.authority && task.runs.contains(&e.run),
            "invalid selected entry"
        );
        owners.push(crate::pr_head::owner_lock(home, &e.run)?);
        dead(home, &RunState::load_under(&e.run, home)?)?;
        drivers.push(crate::driver::maintenance(home, &e.run)?);
    }
    let mut witness_roots: Vec<_> = task
        .runs
        .iter()
        .map(|id| home.join("runs").join(id))
        .collect();
    witness_roots.push(home.join("questions"));
    witness_roots.push(queue.path_of(&task.id));
    let common = PathBuf::from(
        git::git(
            &task.repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?
        .trim(),
    );
    for name in ["refs", "packed-refs", "logs"] {
        witness_roots.push(common.join(name));
    }
    for e in &chosen {
        let state = RunState::load_under(&e.run, home)?;
        witness_roots.push(state.worktree_root());
        for path in worktrees(&state).await? {
            witness_roots.push(PathBuf::from(
                git::git(&path, &["rev-parse", "--absolute-git-dir"])
                    .await?
                    .trim(),
            ));
        }
    }
    let before = witnesses(&witness_roots)?;
    let mut expected = plan.snapshot.clone();
    let mut states = Vec::new();
    let mut proofs = Vec::new();
    for e in &chosen {
        let fence = dir.join(format!("{}-fence.json", e.run));
        let state = RunState::load_under(&e.run, home)?;
        let resumed = fence.try_exists()?;
        if resumed {
            let approved: RunState = read(&fence)?;
            ensure!(
                serde_json::to_value(&approved)?
                    == serde_json::to_value(fenced_state(plan, &e.run)?)?,
                "approved retirement fence changed"
            );
            let current = serde_json::to_value(&state)?;
            let original = plan
                .history
                .get(&e.run)
                .context("missing historical source")?;
            ensure!(
                current == serde_json::to_value(&approved)? || &current == original,
                "run changed after approval"
            );
            let state_path = home.join("runs").join(&e.run).join("run.json");
            if current == serde_json::to_value(&approved)? {
                expected.insert(state_path, fingerprint(&task.repo, &fence).await?);
            }
        }
        eligible(home, task, &state, &plan.authority)?;
        ensure!(
            state.retired_to.is_none()
                || resumed && state.retired_to.as_ref() == Some(&plan.authority),
            "different retirement owner"
        );
        // A fenced run may have lost some worktrees during an interrupted apply.
        let was_retired = state.retired_to.is_some();
        let proof = strict_proof(
            home,
            &state,
            &plan.authority,
            was_retired.then_some(e.paths.as_slice()),
        )
        .await?;
        let current_evidence = evidence(&state, &proof.paths).await?;
        let admins: Vec<_> = e
            .evidence
            .keys()
            .filter(|p| p.file_name().is_some_and(|n| n == "gitdir"))
            .filter_map(|p| p.parent())
            .collect();
        let expected_evidence: BTreeMap<_, _> = e
            .evidence
            .iter()
            .filter(|(path, _)| {
                if let Some(admin) = admins.iter().find(|admin| path.starts_with(admin)) {
                    current_evidence.contains_key(*admin)
                } else if path.file_name().is_some_and(|n| n == ".git") {
                    path.parent()
                        .is_some_and(|p| proof.paths.iter().any(|wt| wt == p))
                } else {
                    true
                }
            })
            .map(|(path, value)| (path.clone(), value.clone()))
            .collect();
        ensure!(
            current_evidence == expected_evidence,
            "Git artifacts changed since planning"
        );
        proofs.push(proof);
        states.push(fenced_state(plan, &e.run)?);
    }
    // Expensive Git proofs run outside the short task-write critical section.
    // All owner, driver, output and Cargo leases remain held through deletion.
    let verified_snapshot = snapshot(home, task).await?;
    let verified_refs = refs(&task.repo).await?;
    {
        let _write = queue.lock_task(&task.id)?;
        ensure!(
            serde_json::to_value(queue.get(&task.id)?)? == serde_json::to_value(task)?,
            "task changed during proof"
        );
        questions(home, task)?;
        let after = witnesses(&witness_roots)?;
        ensure!(
            after == before,
            "artifacts changed before retirement fence: {:?}",
            before
                .keys()
                .chain(after.keys())
                .filter(|p| before.get(*p) != after.get(*p))
                .collect::<std::collections::BTreeSet<_>>()
        );
        ensure!(
            verified_snapshot == expected && verified_refs == plan.refs,
            "artifacts changed; refusing stale plan"
        );
        if !dir.join("approved.json").try_exists()? {
            durable_new(
                &dir.join("approved.json"),
                &serde_json::json!({"version":VERSION,"plan":plan.id,"task":task.id,"authority":plan.authority,"sources":plan.history,"approved_at":jiff::Timestamp::now()}),
            )?;
        }
        let approval: serde_json::Value = read(&dir.join("approved.json"))?;
        ensure!(
            approval["version"] == VERSION
                && approval["plan"] == plan.id
                && approval["task"] == task.id
                && approval["authority"] == plan.authority
                && approval["sources"] == serde_json::to_value(&plan.history)?
                && approval.as_object().is_some_and(|o| o.len() == 6),
            "approval audit changed"
        );
        for state in &mut states {
            let fence = dir.join(format!("{}-fence.json", state.id));
            if !fence.try_exists()? {
                durable_new(&fence, state)?;
            }
            state.save_under_at(home, plan.at)?;
            // Persist the fence before any source-directory removal.
            std::fs::File::open(home.join("runs").join(&state.id).join("run.json"))?.sync_all()?;
            std::fs::File::open(home.join("runs").join(&state.id))?.sync_all()?;
        }
    }
    before_removal()?;
    for (state, proof) in states.iter().zip(&proofs) {
        for path in &proof.paths {
            ensure!(
                git::worktree_remove_clean(&state.repo, path).await?,
                "Git refused clean removal; repeat the same apply after inspection"
            );
        }
    }
    if !dir.join("completed.json").try_exists()? {
        durable_new(
            &dir.join("completed.json"),
            &serde_json::json!({"plan":plan.id,"completed_at":jiff::Timestamp::now()}),
        )?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) async fn interrupt_after_fence(home: &Path, task: &Task, operation: &str) -> Result<()> {
    let queue = Queue::at(home.join("queue"));
    let _claim = queue.claim(&task.id)?;
    let dir = home.join("retention-reconciliations").join(operation);
    let plan: Plan = read(&dir.join("plan.json"))?;
    apply_plan(home, &queue, task, &plan, &dir, || {
        anyhow::bail!("simulated interruption after durable fence")
    })
    .await
}
