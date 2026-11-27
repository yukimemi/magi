//! Read-only landing evidence for change tasks.
//!
//! The housekeeping worker runs bounded passes ([`audit_pass`]) to cache a
//! three-way verdict ([`State::Landed`], [`State::NotLanded`], [`State::Unknown`])
//! on each completed change task without changing its status or activity
//! timestamp, and promotes a task held for [`crate::queue::MachineHoldCause::AwaitingLanding`]
//! once its exact retained run is confirmed landed on its target base.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::config::{MergeMode, MergeStyle};
use crate::land::ExternalMerge;
use crate::notices::{Link, Notice, Notices};
use crate::queue::{CompletionActor, CompletionContext, Queue, Task, TaskStatus};
use crate::run::{Liveness, RunState, RunStatus};

/// What the audit could establish about landing on the run's target base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Confirmed landing on the run's target base.
    Landed,
    /// Confirmed absence of a landing.
    NotLanded,
    /// Evidence was unavailable or inconclusive.
    Unknown,
}

/// Cached verdict for a task's most recent execution run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingNote {
    /// The verdict.
    pub state: State,
    /// The evidence or the reason it was inconclusive.
    pub reason: String,
    /// The run ID this verdict describes.
    pub run: String,
    /// When the audit finished.
    pub checked_at: Timestamp,
}

/// Summary of one bounded [`audit_pass`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Tasks audited in this pass.
    pub audited: usize,
    /// Held awaiting-landing tasks promoted to `done` after confirmed landing.
    pub promoted: usize,
    /// Audited tasks with confirmed `NotLanded`.
    pub not_landed: usize,
}

/// Timeouts for external landing probes and each audited task.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Timeout for a single `git` invocation.
    pub git: Duration,
    /// Timeout for a single `gh` invocation.
    pub gh: Duration,
    /// Total budget for auditing one task before recording `Unknown`.
    pub per_item: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            git: Duration::from_secs(15),
            gh: Duration::from_secs(20),
            per_item: Duration::from_secs(30),
        }
    }
}

/// Maximum tasks audited in one housekeeping pass.
pub const MAX_PER_PASS: usize = 5;
/// Minimum interval before re-auditing a `NotLanded` or `Unknown` note.
pub const RECHECK_SECS: i64 = 30 * 60;
const NOTICE_PREFIX: &str = "landing:";
const PR_LIMIT: usize = 30;

#[derive(Clone, Copy)]
struct Head<'a> {
    branch: &'a str,
    sha: &'a str,
}

impl<'a> Head<'a> {
    fn recorded(run: &'a RunState) -> Option<Self> {
        let (branch, sha) = if let Some(sha) = run.pr_merged_head.as_deref() {
            (run.pr_branch()?, sha)
        } else if let Some(sha) = run.pr_pushed_head.as_deref() {
            (run.pr_branch()?, sha)
        } else {
            (run.winner()?.branch.as_str(), run.gate_head.as_deref()?)
        };
        (!branch.is_empty() && full_oid(sha)).then_some(Self { branch, sha })
    }
}

enum Branch {
    Empty,
    InBase,
    /// Not in the base at the exact tracking-ref commit SHA that was tested.
    OutsideBase(String),
}

enum Facts {
    Recorded,
    Unavailable(&'static str),
    Empty,
    InBase,
    Compared {
        git_error: Option<String>,
        forge: Result<Option<ExternalMerge>, String>,
    },
}

fn judge(base: &str, squash_without_pr: bool, facts: Facts) -> (State, String) {
    match facts {
        Facts::Recorded => (State::Landed, "the run recorded a confirmed landing".into()),
        Facts::Unavailable(reason) => (State::Unknown, reason.into()),
        Facts::Empty => (
            State::NotLanded,
            "the recorded head has the same tree as its recorded comparison base".into(),
        ),
        Facts::InBase => (
            State::Landed,
            format!("{base} contains the run's recorded head"),
        ),
        Facts::Compared { git_error, forge } => match (forge, git_error) {
            (Ok(Some(pr)), _) => (State::Landed, format!("merged into {base}: {}", pr.url)),
            (Ok(None), None) if squash_without_pr => (
                State::Unknown,
                "no exact landing evidence; a manual or local squash would not preserve the recorded head"
                    .into(),
            ),
            (Ok(None), None) => (
                State::NotLanded,
                format!(
                    "{base} does not contain the recorded head and no matching pull request merged there"
                ),
            ),
            (Ok(None), Some(reason)) | (Err(reason), None) => (State::Unknown, reason),
            (Err(forge_error), Some(git_error)) => {
                (State::Unknown, format!("{git_error}; {forge_error}"))
            }
        },
    }
}

fn full_oid(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64)
        && !sha.bytes().all(|b| b == b'0')
        && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn remote(run: &RunState) -> &str {
    run.fetched_base
        .as_ref()
        .map_or(&run.config.merge.remote, |base| &base.remote)
}

async fn probe(program: &str, args: &[&str], repo: &Path) -> Result<Output, String> {
    tokio::process::Command::new(program)
        .args(args)
        .current_dir(repo)
        .env_remove("GH_REPO")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_INDEX_FILE")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("{program}: {e}"))
}

fn stdout(out: &Output) -> Result<&str, String> {
    std::str::from_utf8(&out.stdout)
        .map(str::trim)
        .map_err(|e| format!("invalid UTF-8: {e}"))
}

fn git_failure(operation: &str, out: &Output) -> String {
    format!("git {operation} failed ({})", out.status)
}

async fn git_predicate(repo: &Path, args: &[&str]) -> Result<bool, String> {
    let out = probe("git", args, repo).await?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(args[0], &out)),
    }
}

async fn ancestor(repo: &Path, earlier: &str, later: &str) -> Result<bool, String> {
    git_predicate(repo, &["merge-base", "--is-ancestor", earlier, later]).await
}

async fn local_branch(run: &RunState, head: Head<'_>) -> Result<Branch, String> {
    let repo = &run.repo;
    let start = match &run.base_sync {
        Some(sync) if sync.conflict.is_none() => &sync.tip,
        Some(_) => return Err("the saved base synchronization did not finish".into()),
        None => &run.base_commit,
    };
    let base_ref = format!("refs/remotes/{}/{}", remote(run), run.base_branch);
    let resolved = probe(
        "git",
        &["show-ref", "--verify", "--hash", "--", &base_ref],
        repo,
    )
    .await?;
    if !resolved.status.success() {
        return Err(git_failure("show-ref (remote base)", &resolved));
    }
    let tested_base = stdout(&resolved)?;
    if !full_oid(tested_base) || !full_oid(start) {
        return Err("the recorded comparison base or remote base is not a full commit ID".into());
    }
    if !ancestor(repo, start, head.sha).await? {
        return Err("the recorded head does not descend from its recorded comparison base".into());
    }
    // Check emptiness before ancestry: a rebased empty branch equal to base is NotLanded.
    if git_predicate(
        repo,
        &[
            "diff",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=none",
            start,
            head.sha,
            "--",
        ],
    )
    .await?
    {
        return Ok(Branch::Empty);
    }
    if ancestor(repo, head.sha, tested_base).await? {
        return Ok(Branch::InBase);
    }
    let shallow = probe("git", &["rev-parse", "--is-shallow-repository"], repo).await?;
    if !shallow.status.success() || stdout(&shallow)? != "false" {
        return Err("local history may be incomplete; ancestry is unconfirmed".into());
    }
    Ok(Branch::OutsideBase(tested_base.to_owned()))
}

fn repo_component(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn remote_repository(url: &str) -> Option<String> {
    let (authority, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
    {
        rest.split_once('/')?
    } else {
        let (user_host, path) = url.split_once(':')?;
        if !user_host.contains('@') {
            return None;
        }
        (user_host, path)
    };
    let host = authority.rsplit('@').next()?;
    let (owner, repo) = path.trim_end_matches('/').split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    [host, owner, repo]
        .into_iter()
        .all(repo_component)
        .then(|| format!("{host}/{owner}/{repo}"))
}

fn pr_repository(url: &str, number: u64) -> Option<&str> {
    let (repository, pr) = url.strip_prefix("https://")?.rsplit_once("/pull/")?;
    let (host, path) = repository.split_once('/')?;
    let (owner, repo) = path.split_once('/')?;
    (number != 0
        && pr.bytes().all(|b| b.is_ascii_digit())
        && pr.parse::<u64>().ok() == Some(number)
        && [host, owner, repo].into_iter().all(repo_component))
    .then_some(repository)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergedPr {
    url: String,
    number: u64,
    state: String,
    merged_at: Timestamp,
    base_ref_name: String,
    head_ref_name: String,
    head_ref_oid: String,
    is_cross_repository: bool,
}

fn merged_pr(
    json: &str,
    repository: &str,
    base: &str,
    since: Timestamp,
    head: Head<'_>,
) -> Result<Option<ExternalMerge>, String> {
    let prs: Vec<MergedPr> =
        serde_json::from_str(json).map_err(|e| format!("invalid forge response: {e}"))?;
    if prs.len() >= PR_LIMIT {
        return Err("the forge response reached the pull request limit".into());
    }
    let mut found = None;
    for pr in prs {
        if pr.state != "MERGED"
            || pr.base_ref_name.is_empty()
            || pr.head_ref_name != head.branch
            || !full_oid(&pr.head_ref_oid)
            || !pr_repository(&pr.url, pr.number)
                .is_some_and(|r| r.eq_ignore_ascii_case(repository))
        {
            return Err("the forge returned incomplete or unexpected pull request evidence".into());
        }
        if pr.base_ref_name == base
            && pr.merged_at >= since
            && pr.head_ref_oid.eq_ignore_ascii_case(head.sha)
            && !pr.is_cross_repository
        {
            if found.is_some() {
                return Err("multiple pull requests match the run's head".into());
            }
            found = Some(ExternalMerge {
                url: pr.url,
                number: pr.number,
            });
        }
    }
    Ok(found)
}

async fn forge_merge(
    run: &RunState,
    head: Head<'_>,
    limits: Limits,
) -> Result<Option<ExternalMerge>, String> {
    let out = tokio::time::timeout(
        limits.git,
        probe("git", &["remote", "get-url", "--", remote(run)], &run.repo),
    )
    .await
    .map_err(|_| "git remote lookup timed out")??;
    if !out.status.success() {
        return Err(git_failure("remote get-url", &out));
    }
    let repository = remote_repository(stdout(&out)?)
        .ok_or("the run's remote is not a known forge repository")?;
    if let Some(pr) = &run.pr
        && !pr_repository(&pr.url, pr.number).is_some_and(|r| r.eq_ignore_ascii_case(&repository))
    {
        return Err("the recorded pull request belongs to a different repository".into());
    }
    let limit = PR_LIMIT.to_string();
    let args = [
        "pr",
        "list",
        "--repo",
        &repository,
        "--head",
        head.branch,
        "--state",
        "merged",
        "--limit",
        &limit,
        "--json",
        "url,number,state,mergedAt,baseRefName,headRefName,headRefOid,isCrossRepository",
    ];
    let out = tokio::time::timeout(limits.gh, probe("gh", &args, &run.repo))
        .await
        .map_err(|_| "forge lookup timed out")??;
    if !out.status.success() || !out.stderr.is_empty() {
        return Err(format!(
            "gh pr list did not return a clean result ({})",
            out.status
        ));
    }
    merged_pr(
        stdout(&out)?,
        &repository,
        &run.base_branch,
        run.created_at,
        head,
    )
}

/// Did the run itself record a confirmed remote landing on its target base?
///
/// Local merges (`MergeMode::Local`) are deliberately excluded: a local merge
/// does not prove the commits reached the remote base branch and must be
/// verified against the remote tracking ref or forge.
pub(crate) fn recorded_landing(run: &RunState) -> bool {
    match run.status {
        RunStatus::AlreadyInBase => run.base_sync.as_ref().is_some_and(|sync| {
            sync.conflict.is_none()
                && sync.already_in.as_ref().is_some_and(|evidence| {
                    evidence.tip == sync.tip
                        && full_oid(&evidence.tip)
                        && evidence.commits.iter().all(|commit| full_oid(commit))
                })
        }),
        RunStatus::Merged => run
            .merge
            .as_ref()
            .is_some_and(|m| m.ok && !m.empty && m.mode == MergeMode::Pr),
        _ => false,
    }
}

/// Whether a `Ready` run already carries confirmed merge evidence during
/// settlement (either a recorded remote landing or a merged PR record for its
/// published head).
pub(crate) fn run_landed_for_settle(run: &RunState) -> bool {
    if recorded_landing(run) {
        return true;
    }
    run.status == RunStatus::Ready
        && !run.merge.as_ref().is_some_and(|m| m.empty)
        && run.pr.as_ref().is_some_and(|pr| pr.state == "merged")
        && run.pr_merged_head.as_deref().is_some_and(full_oid)
}

/// Compare the exact tracking-ref SHA that `local_branch` audited against the
/// remote's live base tip. Never re-reads `refs/remotes/...`, so a concurrent
/// `git fetch` cannot mask a stale ancestry check.
async fn base_is_stale(run: &RunState, tested: &str, limits: Limits) -> Option<String> {
    let remote = remote(run);
    let reference = format!("refs/heads/{}", run.base_branch);
    let live = tokio::time::timeout(
        limits.git,
        probe(
            "git",
            &["ls-remote", "--exit-code", "--", remote, &reference],
            &run.repo,
        ),
    )
    .await;
    let live = match live {
        Ok(Ok(out)) if out.status.success() => stdout(&out)
            .ok()
            .and_then(|text| text.split_whitespace().next())
            .filter(|sha| full_oid(sha))
            .map(str::to_owned),
        _ => None,
    };
    let Some(live) = live else {
        return Some("the remote base could not be read to confirm the tracking ref".into());
    };
    if tested.eq_ignore_ascii_case(&live) {
        None
    } else {
        Some("the tracking ref of the base is stale; fetch it before judging".into())
    }
}

async fn gather_facts(run: &RunState, limits: Limits) -> Facts {
    if recorded_landing(run) {
        return Facts::Recorded;
    }
    if run.merge.as_ref().is_some_and(|m| m.empty)
        || run.winner().is_some_and(|winner| winner.empty)
    {
        return Facts::Empty;
    }
    let Some(head) = Head::recorded(run) else {
        return Facts::Unavailable("the run did not record an exact head to audit");
    };
    if !run.repo.join(".git").exists() {
        return Facts::Unavailable("the run's repository checkout is unavailable");
    }
    match tokio::time::timeout(limits.git, local_branch(run, head)).await {
        Ok(Ok(Branch::Empty)) => Facts::Empty,
        Ok(Ok(Branch::InBase)) => Facts::InBase,
        Ok(Ok(Branch::OutsideBase(tested))) => {
            // Query remote freshness and forge in parallel so `ls-remote` does
            // not extend worst-case audit latency beyond the forge timeout.
            let (git_error, forge) = tokio::join!(
                base_is_stale(run, &tested, limits),
                forge_merge(run, head, limits),
            );
            Facts::Compared { git_error, forge }
        }
        Ok(Err(e)) => Facts::Compared {
            git_error: Some(e),
            forge: forge_merge(run, head, limits).await,
        },
        Err(_) => Facts::Compared {
            git_error: Some("local Git lookup timed out".into()),
            forge: forge_merge(run, head, limits).await,
        },
    }
}

/// Evaluate landing state for `run` under `limits`.
pub async fn verdict(run: &RunState, limits: Limits) -> (State, String) {
    let squash_without_pr = matches!(run.config.merge.mode, MergeMode::None | MergeMode::Local)
        && run.config.merge.style == MergeStyle::Squash;
    judge(
        &run.base_branch,
        squash_without_pr,
        gather_facts(run, limits).await,
    )
}

/// The run audited for a completed change task, if any.
/// Investigations and explicitly discarded tasks are exempt.
pub(crate) fn audited_run(task: &Task) -> Option<&str> {
    if task.status != TaskStatus::Done || task.investigation || task.completed_by_discard() {
        return None;
    }
    task.execution_runs().last().map(String::as_str)
}

/// The retained run of a task held for awaiting landing, if it still matches
/// the latest execution run in the current assignment.
pub(crate) fn awaiting_landing_run(task: &Task) -> Option<&str> {
    if task.status != TaskStatus::Held || task.investigation {
        return None;
    }
    let run = task.awaiting_landing_run()?;
    (task.execution_runs().last().map(String::as_str) == Some(run)).then_some(run)
}

fn target_run(task: &Task) -> Option<&str> {
    audited_run(task).or_else(|| awaiting_landing_run(task))
}

/// Load `id` and verify that it belongs to `task`'s current repository
/// assignment and current authored instruction generation.
pub(crate) fn load_run(home: &Path, task: &Task, id: &str) -> anyhow::Result<RunState> {
    anyhow::ensure!(crate::run::is_run_id(id), "invalid execution run ID");
    let run = RunState::load_under(id, home)?;
    let owner = run
        .origin
        .as_ref()
        .and_then(|origin| origin.task.as_deref());
    let same_repo = run.repo == task.repo
        || run
            .repo
            .canonicalize()
            .ok()
            .zip(task.repo.canonicalize().ok())
            .is_some_and(|(run, task)| run == task);
    let implicit_repo = task.repo.as_os_str().is_empty() || task.repo == Path::new(".");
    anyhow::ensure!(
        run.id == id
            && run.task_assignment == task.reassignments.len()
            && owner.is_none_or(|owner| owner == task.id)
            && (same_repo || (implicit_repo && owner == Some(task.id.as_str())))
            && task.assignment_accepts(&run),
        "the run does not belong to the task's current repository assignment"
    );
    anyhow::ensure!(
        crate::daemon::compatible_run_instruction(&run, task),
        "the run does not match the current task instruction"
    );
    Ok(run)
}

/// The cached verdict, if it still describes the task's current audited run.
pub fn current_note(task: &Task) -> Option<&LandingNote> {
    let run = audited_run(task)?;
    task.landing.as_ref().filter(|n| n.run == run)
}

fn current_target_note(task: &Task) -> Option<&LandingNote> {
    let run = target_run(task)?;
    task.landing.as_ref().filter(|n| n.run == run)
}

/// One-line annotation for CLI/web listings; `None` when landing was confirmed
/// or the task is exempt.
pub fn annotation(task: &Task) -> Option<String> {
    let note = current_note(task)?;
    match note.state {
        State::Landed => None,
        State::NotLanded => Some(format!("NOT LANDED: {}", note.reason)),
        State::Unknown => Some(format!("landing unconfirmed: {}", note.reason)),
    }
}

fn due(task: &Task, now: Timestamp) -> bool {
    target_run(task).is_some()
        && current_target_note(task).is_none_or(|note| {
            let confirmed_done = task.status == TaskStatus::Done && note.state == State::Landed;
            !confirmed_done && now.as_second() - note.checked_at.as_second() >= RECHECK_SECS
        })
}

fn try_promote_awaiting_landing(
    home: &Path,
    queue: &Queue,
    observed: &Task,
    run_id: &str,
    run_state: &RunState,
    reason: &str,
) -> bool {
    let Ok(_claim) = queue.claim(&observed.id) else {
        return false;
    };
    let Ok(mut current) = queue.get(&observed.id) else {
        return false;
    };
    if current.status != TaskStatus::Held
        || current.investigation
        || current.updated_at != observed.updated_at
        || current.machine_hold_cause != observed.machine_hold_cause
        || current.instruction != observed.instruction
        || current.instruction_rev != observed.instruction_rev
        || current.repo != observed.repo
        || current.reassignments != observed.reassignments
        || awaiting_landing_run(&current) != Some(run_id)
    {
        return false;
    }
    let Ok(_fix) = crate::graph::FixClaim::acquire(&home.join("runs").join(run_id)) else {
        return false;
    };
    let Ok(_owner) = crate::pr_head::owner_lock(home, run_id) else {
        return false;
    };
    let Ok(reloaded) = load_run(home, &current, run_id) else {
        return false;
    };
    let claims = crate::daemon::is_working_on(home, run_id, Timestamp::now());
    if reloaded.liveness_under(home, claims) != Liveness::Dead || !reloaded.active.is_empty() {
        return false;
    }
    let Ok(_lease) = crate::driver::maintenance(home, run_id) else {
        return false;
    };
    let expected = current.clone();
    current.succeed(CompletionContext::from_run(
        CompletionActor::DaemonSettle,
        Some(format!("Landing confirmed: {reason}")),
        &current,
        run_state,
    ));
    if let Err(e) = queue.commit_completion(&mut current, &expected) {
        tracing::warn!("landing audit: promote {}: {e:#}", observed.id);
        return false;
    }
    crate::daemon::supersede_prior_runs(&current, home);
    true
}

/// Bounded housekeeping pass: audits at most `max` overdue tasks, saves each
/// verdict without bumping `updated_at` (including timeouts as `Unknown` so no
/// slow item starves later tasks), promotes awaiting-landing tasks once
/// confirmed landed, and reconciles per-repository notices for confirmed
/// non-landings on `done` tasks.
pub async fn audit_pass(home: &Path, limits: Limits, max: usize) -> Summary {
    let queue = Queue::at(home.join("queue"));
    let now = Timestamp::now();
    let mut todo: Vec<_> = queue.list().into_iter().filter(|t| due(t, now)).collect();
    todo.sort_by(|a, b| {
        current_target_note(a)
            .map(|n| n.checked_at)
            .cmp(&current_target_note(b).map(|n| n.checked_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut summary = Summary::default();
    for task in todo.into_iter().take(max) {
        let Some(run) = target_run(&task) else {
            continue;
        };
        let loaded = load_run(home, &task, run);
        let (state, reason) = match &loaded {
            Ok(run_state) => {
                match tokio::time::timeout(limits.per_item, verdict(run_state, limits)).await {
                    Ok(result) => result,
                    Err(_) => (
                        State::Unknown,
                        format!(
                            "landing audit timed out after {}s",
                            limits.per_item.as_secs()
                        ),
                    ),
                }
            }
            Err(e) => (State::Unknown, format!("run record unavailable: {e}")),
        };
        summary.audited += 1;
        if state == State::NotLanded && task.status == TaskStatus::Done {
            summary.not_landed += 1;
        }
        if task.status == TaskStatus::Held
            && state == State::Landed
            && let Ok(run_state) = &loaded
            && try_promote_awaiting_landing(home, &queue, &task, run, run_state, &reason)
        {
            summary.promoted += 1;
            continue;
        }
        let note = LandingNote {
            state,
            reason,
            run: run.to_owned(),
            checked_at: Timestamp::now(),
        };
        if let Err(e) = queue.cache_landing(&task, note) {
            tracing::warn!("landing audit: save note for {}: {e:#}", task.id);
        }
    }
    reconcile_notices(home, &queue.list());
    summary
}

fn reconcile_notices(home: &Path, tasks: &[Task]) {
    use std::collections::{BTreeMap, BTreeSet};
    let mut by_repo = BTreeMap::<_, Vec<_>>::new();
    for task in tasks {
        if current_note(task).is_some_and(|n| n.state == State::NotLanded) {
            by_repo
                .entry(task.repo.to_string_lossy().replace('\\', "/"))
                .or_default()
                .push(task.id.as_str());
        }
    }
    let store = Notices::at(home.join("notifications"));
    let mut live = BTreeSet::new();
    for (repo, mut ids) in by_repo {
        ids.sort_unstable();
        let key = format!("{NOTICE_PREFIX}{repo}");
        let shown = &ids[..ids.len().min(5)];
        let name = Path::new(&repo)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&repo);
        let mut message = format!(
            "{} done task(s) in {name} have not landed on their base branch: {}",
            ids.len(),
            shown.join(", ")
        );
        if ids.len() > shown.len() {
            message.push_str(&format!(" and {} more", ids.len() - shown.len()));
        }
        live.insert(crate::notices::id_of(&key));
        crate::notices::raise_in(
            home,
            Notice::warn(&key, message)
                .link(Link::Task {
                    id: ids[0].to_owned(),
                })
                .about(ids.iter().map(|id| (*id).to_owned()))
                .cause(crate::notices::id_of(&ids.join(","))),
        );
    }
    for notice in store.list() {
        if notice.key.starts_with(NOTICE_PREFIX)
            && !live.contains(&notice.id)
            && let Err(e) = store.dismiss(&notice.id)
        {
            tracing::warn!("landing audit: dismiss {}: {e:#}", notice.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::queue::{InvestigationResult, Source};
    use crate::run::Origin;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::process::Command;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const BRANCH: &str = "magi/test/A";
    const REPOSITORY: &str = "github.com/owner/repo";

    fn pr() -> Value {
        json!({
            "url": "https://github.com/Owner/Repo/pull/9", "number": 9,
            "state": "MERGED", "mergedAt": "2026-09-20T10:00:00Z",
            "baseRefName": "main", "headRefName": BRANCH,
            "headRefOid": HEAD, "isCrossRepository": false,
        })
    }

    fn parse_prs(prs: Value) -> Result<Option<ExternalMerge>, String> {
        merged_pr(
            &prs.to_string(),
            REPOSITORY,
            "main",
            "2026-09-19T00:00:00Z".parse().unwrap(),
            Head {
                branch: BRANCH,
                sha: HEAD,
            },
        )
    }

    #[test]
    fn judge_lands_on_exact_forge_proof_even_when_git_is_unavailable() {
        let found = parse_prs(json!([pr()])).unwrap().unwrap();
        let facts = Facts::Compared {
            git_error: Some("tracking base is missing".into()),
            forge: Ok(Some(found)),
        };
        let (state, reason) = judge("main", true, facts);
        assert_eq!(state, State::Landed);
        assert!(reason.contains("/pull/9"));
    }

    #[test]
    fn judge_requires_both_negative_probes_to_rule_out_landing() {
        let neither = Facts::Compared {
            git_error: None,
            forge: Ok(None),
        };
        assert_eq!(judge("main", false, neither).0, State::NotLanded);
        let manual_squash = Facts::Compared {
            git_error: None,
            forge: Ok(None),
        };
        assert_eq!(judge("main", true, manual_squash).0, State::Unknown);

        let forge_only = Facts::Compared {
            git_error: Some("tracking base is missing".into()),
            forge: Ok(None),
        };
        assert_eq!(judge("main", false, forge_only).0, State::Unknown);

        let git_only = Facts::Compared {
            git_error: None,
            forge: Err("forge timed out".into()),
        };
        assert_eq!(judge("main", false, git_only).0, State::Unknown);
    }

    #[test]
    fn forge_requires_a_same_repository_merge_of_the_exact_head_into_the_original_base() {
        assert_eq!(parse_prs(json!([pr()])).unwrap().unwrap().number, 9);
        assert_eq!(parse_prs(json!([])).unwrap(), None);
        for (field, value) in [
            ("baseRefName", json!("release")),
            ("headRefOid", json!(OTHER_HEAD)),
            ("isCrossRepository", json!(true)),
            ("mergedAt", json!("2026-09-18T10:00:00Z")),
        ] {
            let mut unrelated = pr();
            unrelated[field] = value;
            assert_eq!(parse_prs(json!([unrelated])).unwrap(), None, "{field}");
        }
        assert_eq!(
            remote_repository("git@github.com:Owner/Repo.git").as_deref(),
            Some("github.com/Owner/Repo")
        );
    }

    #[test]
    fn bad_or_bounded_forge_results_do_not_count_even_with_an_exact_match() {
        for (field, value) in [
            ("url", json!("https://github.com/Other/Repo/pull/9")),
            (
                "url",
                json!("https://github.com/Owner/Repo/pull/not-a-number"),
            ),
            ("headRefName", json!("different-branch")),
            ("headRefOid", json!("cafebabe")),
            ("mergedAt", json!("no timestamp")),
            ("isCrossRepository", Value::Null),
            ("state", json!("OPEN")),
            ("baseRefName", json!("")),
        ] {
            let mut invalid = pr();
            invalid[field] = value;
            assert!(parse_prs(json!([pr(), invalid])).is_err(), "{field}");
        }
        let mut incomplete = pr();
        incomplete.as_object_mut().unwrap().remove("mergedAt");
        assert!(parse_prs(json!([incomplete])).is_err());
        assert!(parse_prs(Value::Array(vec![pr(); PR_LIMIT])).is_err());
        assert!(parse_prs(json!([pr(), pr()])).is_err());
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=Landing Test",
                "-c",
                "user.email=landing@example.invalid",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout(&out).unwrap().to_owned()
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        home: PathBuf,
        repo: PathBuf,
        base: String,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            let home = temp.path().join("home");
            std::fs::create_dir(&repo).unwrap();
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["commit", "--allow-empty", "-m", "base"]);
            let base = git(&repo, &["rev-parse", "HEAD"]);
            git(&repo, &["update-ref", "refs/remotes/origin/main", &base]);
            Self {
                _temp: temp,
                home,
                repo,
                base,
            }
        }

        fn head(&self, empty: bool) -> String {
            git(&self.repo, &["switch", "-c", BRANCH]);
            if !empty {
                std::fs::write(self.repo.join("work.txt"), "candidate work").unwrap();
                git(&self.repo, &["add", "work.txt"]);
            }
            git(&self.repo, &["commit", "--allow-empty", "-m", "candidate"]);
            let sha = git(&self.repo, &["rev-parse", "HEAD"]);
            git(&self.repo, &["switch", "main"]);
            sha
        }

        fn run(&self, head: &str) -> RunState {
            let mut run = RunState::new(
                self.repo.clone(),
                "main".into(),
                self.base.clone(),
                "work".into(),
                Config::default(),
            );
            run.status = RunStatus::Ready;
            run.pr_head = Some(BRANCH.into());
            run.pr_pushed_head = Some(head.into());
            run
        }

        fn done_task(&self, queue: &Queue, mut run: RunState, investigation: bool) -> Task {
            let mut task = Task::new(
                "task".into(),
                "work".into(),
                self.repo.clone(),
                Source::Human,
            );
            task.investigation = investigation;
            if investigation {
                task.completion_result = Some(InvestigationResult {
                    text: "Finding recorded".into(),
                    instruction_rev: task.instruction_rev,
                    recorded_at: Timestamp::now(),
                });
            }
            task.link_run(&run.id);
            run.task_instruction = Some(task.instruction.clone());
            run.task_instruction_rev = Some(task.instruction_rev);
            if run.origin.is_none() {
                run.origin = Some(Origin::queue(&task.id));
            }
            run.save_under(&self.home).unwrap();
            task.succeed(CompletionContext::from_run(
                CompletionActor::DaemonSettle,
                None,
                &task,
                &run,
            ));
            queue.put(&mut task).unwrap();
            task
        }
    }

    #[tokio::test]
    async fn ancestry_checks_the_pinned_head_instead_of_the_branch_name() {
        let f = Fixture::new();
        let sha = f.head(false);
        let mut run = f.run(&sha);
        git(&f.repo, &["update-ref", "refs/remotes/origin/main", &sha]);
        git(&f.repo, &["branch", "-f", BRANCH, &f.base]);
        assert_eq!(verdict(&run, Limits::default()).await.0, State::Landed);

        git(&f.repo, &["branch", "-f", BRANCH, &sha]);
        run.pr_pushed_head = None;
        assert_eq!(verdict(&run, Limits::default()).await.0, State::Unknown);

        run.pr_pushed_head = Some(OTHER_HEAD.into());
        let (state, why) = verdict(&run, Limits::default()).await;
        assert_eq!(state, State::Unknown);
        assert!(why.contains("git merge-base failed"), "{why}");
    }

    #[tokio::test]
    async fn local_merge_does_not_count_as_remote_landing_without_push() {
        let f = Fixture::new();
        let sha = f.head(false);
        let remote = f._temp.path().join("origin.git");
        git(
            f._temp.path(),
            &["init", "--bare", "-b", "main", "origin.git"],
        );
        git(
            &f.repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(
            &f.repo,
            &["push", "origin", &format!("{}:refs/heads/main", f.base)],
        );
        // Merge into local main without pushing to origin.
        git(&f.repo, &["merge", "--no-ff", BRANCH, "-m", "local merge"]);
        let mut run = f.run(&sha);
        run.status = RunStatus::Merged;
        run.config.merge.mode = MergeMode::Local;
        run.merge = Some(crate::run::MergeOutcome {
            mode: MergeMode::Local,
            ok: true,
            detail: "merged locally".into(),
            empty: false,
        });
        assert!(!recorded_landing(&run));
        // Remote tracking ref still points at base, not the local merge.
        assert_ne!(verdict(&run, Limits::default()).await.0, State::Landed);
        // Once pushed to origin/main, exact ancestry confirms Landed.
        let pushed = git(&f.repo, &["rev-parse", "HEAD"]);
        git(&f.repo, &["push", "origin", "main"]);
        git(
            &f.repo,
            &["update-ref", "refs/remotes/origin/main", &pushed],
        );
        assert_eq!(verdict(&run, Limits::default()).await.0, State::Landed);
    }

    #[tokio::test]
    async fn a_squash_without_forge_evidence_is_unknown_and_does_not_write_git_objects() {
        let f = Fixture::new();
        let sha = f.head(false);
        git(&f.repo, &["merge", "--squash", BRANCH]);
        git(&f.repo, &["commit", "-m", "squashed"]);
        git(&f.repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let objects = git(&f.repo, &["count-objects", "-v"]);
        assert_eq!(
            verdict(&f.run(&sha), Limits::default()).await.0,
            State::Unknown
        );
        assert_eq!(git(&f.repo, &["count-objects", "-v"]), objects);
    }

    #[tokio::test]
    async fn a_rebased_head_equal_to_its_recorded_base_does_not_count_upstream_work() {
        let f = Fixture::new();
        let upstream = f.head(false);
        git(
            &f.repo,
            &["update-ref", "refs/remotes/origin/main", &upstream],
        );
        let remote = f._temp.path().join("origin.git");
        git(
            f._temp.path(),
            &["init", "--bare", "-b", "main", "origin.git"],
        );
        git(
            &f.repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(
            &f.repo,
            &["push", "origin", &format!("{upstream}:refs/heads/main")],
        );
        let mut run = f.run(&upstream);
        run.base_sync = Some(crate::run::BaseSync {
            tip: upstream,
            behind: 0,
            attempts: 1,
            conflict: None,
            already_in: None,
        });
        assert_eq!(verdict(&run, Limits::default()).await.0, State::NotLanded);
    }

    #[tokio::test]
    async fn an_unreadable_or_stale_remote_base_is_unknown_not_a_non_landing() {
        let f = Fixture::new();
        let upstream = f.head(false);
        let remote = f._temp.path().join("origin.git");
        git(
            f._temp.path(),
            &["init", "--bare", "-b", "main", "origin.git"],
        );
        git(
            &f.repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(
            &f.repo,
            &["push", "origin", &format!("{upstream}:refs/heads/main")],
        );
        git(
            &f.repo,
            &["update-ref", "refs/remotes/origin/main", &f.base],
        );
        let run = f.run(&upstream);
        let (state, why) = verdict(&run, Limits::default()).await;
        assert_eq!(state, State::Unknown, "{why}");
        assert!(why.contains("stale"), "{why}");
    }

    #[tokio::test]
    async fn concurrent_fetch_updating_tracking_ref_still_compares_against_the_tested_base() {
        let f = Fixture::new();
        let upstream = f.head(false);
        let remote = f._temp.path().join("origin.git");
        git(
            f._temp.path(),
            &["init", "--bare", "-b", "main", "origin.git"],
        );
        git(
            &f.repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(
            &f.repo,
            &["push", "origin", &format!("{upstream}:refs/heads/main")],
        );
        // Simulate local_branch having tested the old tracking ref (`f.base`),
        // then a concurrent fetch advancing `refs/remotes/origin/main` to `upstream`.
        git(
            &f.repo,
            &["update-ref", "refs/remotes/origin/main", &upstream],
        );
        let run = f.run(&upstream);
        let stale = base_is_stale(&run, &f.base, Limits::default()).await;
        assert!(
            stale.as_deref().is_some_and(|why| why.contains("stale")),
            "expected stale error comparing tested base against live remote, got {stale:?}"
        );
    }

    #[tokio::test]
    async fn terminal_labels_without_recorded_evidence_are_not_landed() {
        let f = Fixture::new();
        let mut run = f.run(OTHER_HEAD);
        run.pr_pushed_head = None;
        run.status = RunStatus::AlreadyInBase;
        assert_eq!(verdict(&run, Limits::default()).await.0, State::Unknown);
        run.status = RunStatus::Merged;
        run.merge = Some(crate::run::MergeOutcome {
            mode: MergeMode::None,
            ok: true,
            detail: String::new(),
            empty: false,
        });
        assert_eq!(verdict(&run, Limits::default()).await.0, State::Unknown);
        run.merge.as_mut().unwrap().mode = MergeMode::Local;
        assert_eq!(verdict(&run, Limits::default()).await.0, State::Unknown);
        run.merge.as_mut().unwrap().mode = MergeMode::Pr;
        assert_eq!(verdict(&run, Limits::default()).await.0, State::Landed);
    }

    #[test]
    fn an_audit_snapshot_cannot_change_a_reopened_task() {
        let f = Fixture::new();
        let queue = Queue::at(f.home.join("queue"));
        let snapshot = f.done_task(&queue, f.run(&f.base), false);
        let note = LandingNote {
            state: State::NotLanded,
            reason: "the test head had no change".into(),
            run: audited_run(&snapshot).unwrap().to_owned(),
            checked_at: Timestamp::now(),
        };
        queue
            .modify(&snapshot.id, |task| {
                task.release();
                true
            })
            .unwrap();
        queue.cache_landing(&snapshot, note).unwrap();
        let current = queue.get(&snapshot.id).unwrap();
        assert_eq!(current.status, TaskStatus::Queued);
        assert!(current.landing.is_none());
    }

    #[tokio::test]
    async fn finite_passes_record_notices_only_for_confirmed_non_landings() {
        let f = Fixture::new();
        let sha = f.head(true);
        let queue = Queue::at(f.home.join("queue"));
        let empty = f.done_task(&queue, f.run(&sha), false);
        let investigation = f.done_task(&queue, f.run(&sha), true);
        let mut foreign = f.run(&sha);
        foreign.status = RunStatus::Merged;
        foreign.merge = Some(crate::run::MergeOutcome {
            mode: MergeMode::Pr,
            ok: true,
            detail: String::new(),
            empty: false,
        });
        foreign.origin = Some(Origin::queue("another-task"));
        let unrelated = f.done_task(&queue, foreign, false);
        let notices = Notices::at(f.home.join("notifications"));

        audit_pass(&f.home, Limits::default(), 1).await;
        assert_eq!(
            queue
                .list()
                .iter()
                .filter(|task| current_note(task).is_some())
                .count(),
            1
        );
        audit_pass(&f.home, Limits::default(), 1).await;
        let read_empty = queue.get(&empty.id).unwrap();
        assert_eq!(
            read_empty.status,
            TaskStatus::Done,
            "audit never transitions a done task"
        );
        assert_eq!(read_empty.updated_at, empty.updated_at);
        assert!(annotation(&read_empty).unwrap().starts_with("NOT LANDED"));
        assert!(
            annotation(&queue.get(&unrelated.id).unwrap())
                .unwrap()
                .starts_with("landing unconfirmed")
        );
        assert!(current_note(&queue.get(&investigation.id).unwrap()).is_none());

        let seen = notices.list();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].message.contains(&empty.id));
        let generation = seen[0].generation;
        queue
            .modify(&empty.id, |task| {
                task.landing.as_mut().unwrap().checked_at = "2020-01-01T00:00:00Z".parse().unwrap();
                true
            })
            .unwrap();
        audit_pass(&f.home, Limits::default(), MAX_PER_PASS).await;
        assert_eq!(notices.list()[0].generation, generation);

        queue
            .modify(&empty.id, |task| {
                task.investigation = true;
                true
            })
            .unwrap();
        audit_pass(&f.home, Limits::default(), MAX_PER_PASS).await;
        assert!(notices.list().is_empty());
    }

    #[tokio::test]
    async fn audit_promotes_held_awaiting_landing_only_when_snapshot_is_unchanged() {
        let f = Fixture::new();
        let sha = f.head(false);
        let queue = Queue::at(f.home.join("queue"));
        let mut run = f.run(&sha);
        run.driver_exited = true;
        let mut task = Task::new(
            "awaiting".into(),
            "work".into(),
            f.repo.clone(),
            Source::Human,
        );
        task.link_run(&run.id);
        run.origin = Some(Origin::queue(&task.id));
        run.task_instruction = Some(task.instruction.clone());
        run.task_instruction_rev = Some(task.instruction_rev);
        run.save_under(&f.home).unwrap();
        task.hold_awaiting_landing(run.id.clone(), "Ready but not landed".into());
        queue.put(&mut task).unwrap();

        // Landing on origin/main promotes the held awaiting-landing task to Done.
        git(&f.repo, &["update-ref", "refs/remotes/origin/main", &sha]);
        let summary = audit_pass(&f.home, Limits::default(), MAX_PER_PASS).await;
        assert_eq!(summary.promoted, 1);
        let promoted = queue.get(&task.id).unwrap();
        assert_eq!(promoted.status, TaskStatus::Done);
        assert!(promoted.machine_hold_cause.is_none());

        // If the task instruction was edited after the run, landing does NOT promote it.
        let mut edited = Task::new(
            "edited".into(),
            "work".into(),
            f.repo.clone(),
            Source::Human,
        );
        let mut run2 = f.run(&sha);
        run2.driver_exited = true;
        edited.link_run(&run2.id);
        run2.origin = Some(Origin::queue(&edited.id));
        run2.task_instruction = Some(edited.instruction.clone());
        run2.task_instruction_rev = Some(edited.instruction_rev);
        run2.save_under(&f.home).unwrap();
        edited.hold_awaiting_landing(run2.id.clone(), "Ready but not landed".into());
        edited
            .edit("edited".into(), "different work".into())
            .unwrap();
        edited.edit("edited".into(), "work".into()).unwrap();
        assert!(load_run(&f.home, &edited, &run2.id).is_err());
        run2.task_instruction_rev = None;
        run2.save_under(&f.home).unwrap();
        assert!(load_run(&f.home, &edited, &run2.id).is_err());
        queue.put(&mut edited).unwrap();

        let summary = audit_pass(&f.home, Limits::default(), MAX_PER_PASS).await;
        assert_eq!(summary.promoted, 0);
        assert_eq!(queue.get(&edited.id).unwrap().status, TaskStatus::Held);
    }

    #[tokio::test]
    async fn timed_out_audit_item_records_unknown_and_does_not_starve_later_tasks() {
        let f = Fixture::new();
        let sha_empty = f.head(true);
        let queue = Queue::at(f.home.join("queue"));
        let mut a = f.done_task(&queue, f.run(&sha_empty), false);
        let mut b = f.done_task(&queue, f.run(&sha_empty), false);
        if a.id > b.id {
            std::mem::swap(&mut a, &mut b);
        }

        let zero = Limits {
            git: Duration::from_secs(15),
            gh: Duration::from_secs(20),
            per_item: Duration::ZERO,
        };
        audit_pass(&f.home, zero, 1).await;
        let first_after = queue.get(&a.id).unwrap();
        let note = current_note(&first_after).expect("timeout still records a note");
        assert_eq!(note.state, State::Unknown);
        assert!(note.reason.contains("timed out"), "{}", note.reason);

        // Next pass with limit 1 audits the second (never-audited) task instead of starving it.
        audit_pass(&f.home, Limits::default(), 1).await;
        let second_after = queue.get(&b.id).unwrap();
        assert_eq!(
            current_note(&second_after).map(|n| n.state),
            Some(State::NotLanded)
        );
    }
}
