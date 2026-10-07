//! Duplicate-work detection at the moment work is filed.
//!
//! Two tasks once chased the same commit without knowing about each other: an
//! open task and its open pull request already covered a change when a chat
//! filed a second task that named the same branch and commit. Nothing at
//! filing time said so. This module is that check, and it is deliberately
//! small: **concrete identifiers only** - a branch name, a commit SHA, a pull
//! request number - never text similarity. A false positive costs one
//! `--force`; a false negative is the behaviour before this module existed.
//!
//! A *claim* is a thing an unfinished piece of work owns: the branches of its
//! candidates and the pull request it opened. The claims come from
//!
//! - every task that is not `done`: its `review_branch`, and each run in
//!   `Task::runs`;
//! - every run that has not reached a terminal status;
//! - every run that is terminal but whose pull request is still open (the
//!   shape of the original accident: the run ended, the PR did not).
//!
//! Run records are read through a tolerant view rather than `RunState::load`:
//! a schema bump must not turn into a silent false negative (the same lesson
//! as `clean::fold_due`).
//!
//! Everything here is synchronous, read-only and takes its roots as
//! arguments, so tests need no process-global home.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::agent;
use crate::config::Config;
use crate::prompt;

use crate::land::PrLifecycle;
use crate::proc::Quiet as _;
use crate::queue::{Queue, Task, TaskStatus};
use crate::run::RunStatus;

/// What kind of identifier matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A branch name.
    Branch,
    /// A commit SHA.
    Sha,
    /// A pull request number or URL.
    Pr,
}

/// Who owns the thing that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A queued task.
    Task,
    /// A recorded run.
    Run,
    /// A pull request the forge reports open that no record here owns.
    Pr,
}

/// One reason a new piece of work looks like one already under way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Task or run.
    pub owner: Owner,
    /// Full id of the task or run.
    pub id: String,
    /// The owner's status word (`queued`, `reviewing`, ...).
    pub status: String,
    /// What kind of identifier matched.
    pub signal: Signal,
    /// The identifier as it matched: a branch, a SHA, `#48`.
    pub token: String,
    /// How the owner is tied to it, e.g. `produced by its run c9eb`.
    pub via: String,
    /// The owner's title and instruction (collapsed, cut), for the
    /// judge only: never shown in [`Display`]. Empty when no record has it.
    pub about: String,
}

impl fmt::Display for Hit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.owner {
            Owner::Task => "task",
            Owner::Run => "run",
            Owner::Pr => "pull request",
        };
        let what = match self.signal {
            Signal::Branch => "names branch",
            Signal::Sha => "names commit",
            Signal::Pr => "names pull request",
        };
        write!(
            f,
            "{kind} {} ({}): this work {what} {}, {}",
            crate::queue::short(&self.id),
            self.status,
            self.token,
            self.via
        )
    }
}

/// The refusal: one or more [`Hit`]s and nothing filed.
#[derive(Debug, Clone)]
pub struct Duplicate {
    /// What matched.
    pub hits: Vec<Hit>,
    /// The judge's word, when one was asked: a ready-made line for the
    /// refusal text. `None` when no judge ran.
    pub judge: Option<String>,
}

impl Duplicate {
    /// A refusal on the identifier match alone.
    pub fn new(hits: Vec<Hit>) -> Self {
        Self { hits, judge: None }
    }

    /// The refusal text, ending in how to override it.
    pub fn render(&self, override_hint: &str) -> String {
        let mut out = String::from("this looks like work that is already in flight:");
        for h in &self.hits {
            out.push_str("\n  - ");
            out.push_str(&h.to_string());
        }
        if let Some(j) = &self.judge {
            out.push('\n');
            out.push_str(j);
        }
        out.push('\n');
        out.push_str(override_hint);
        out
    }
}

impl fmt::Display for Duplicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(
            "If it is not a duplicate, pass --force to file it anyway \
             (an agent should report this to the operator instead).",
        ))
    }
}

impl std::error::Error for Duplicate {}

/// The judge's answer, with the agent that gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    /// True when the new work is genuinely a duplicate of the matched claims.
    pub duplicate: bool,
    /// One line, as the agent wrote it.
    pub reason: String,
    /// Id of the agent that answered.
    pub agent: String,
}

/// How [`screen`] let work through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screened {
    /// No identifier matched; no judge was asked.
    Clean,
    /// A match was found and the judge said it is not a duplicate.
    Cleared(Judgement),
    /// A match was found and nobody could judge it (the reason): let through.
    Unjudged(String),
}

/// A judge: given the instruction and the matched claims, rules on them.
/// Owned arguments, so a boxed future needs no borrow.
pub type JudgeFuture = Pin<Box<dyn Future<Output = Result<Judgement>> + Send>>;

/// Whole-chain wall-clock budget for the judge call.
const JUDGE_BUDGET: Duration = Duration::from_secs(45);
/// Per-agent cap inside that budget.
const JUDGE_TURN: Duration = Duration::from_secs(30);

/// Read the judge's reply. Strict: anything but one JSON object with a bool
/// `duplicate` and a non-empty reason is an error (the caller lets the work
/// through unjudged, see [`screen`]).
fn parse_judgement(text: &str) -> Result<(bool, String)> {
    #[derive(Deserialize)]
    struct Raw {
        duplicate: bool,
        reason: String,
    }
    // The whole reply must be one object (optionally in a code fence): hunting
    // for an object inside prose or a second answer could recover a verdict
    // from a reply that is not itself a single valid one.
    let mut body = text.trim();
    if let Some(rest) = body.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        body = rest.trim().strip_suffix("```").unwrap_or(rest).trim();
    }
    let raw: Raw =
        serde_json::from_str(body).context("the judge's reply is not a single JSON object")?;
    let reason = raw.reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.is_empty() {
        bail!("the judge gave no reason");
    }
    Ok((raw.duplicate, reason.chars().take(300).collect()))
}

/// Ask the `[roles] chatter` chain (or the default agent) once. Each agent is
/// tried at most once, with a fresh seat; only an error, quota or unusable
/// answer moves on. A usable answer that does not parse ends the chain.
pub async fn chain_judge(
    cfg: &Config,
    repo: &Path,
    instruction: String,
    hits: Vec<Hit>,
) -> Result<Judgement> {
    let chain = agent::pick_chain(
        &cfg.agents,
        cfg.roles.chatter.as_ref(),
        &agent::installed,
        "dupes judge",
    )?;
    let claims: Vec<(String, String)> = hits
        .iter()
        .map(|h| (h.to_string(), h.about.clone()))
        .collect();
    let body = prompt::dupes_judge(&instruction, &claims);
    let artifacts = std::env::temp_dir().join(format!("magi-dupes-{:016x}", crate::rng::entropy()));
    let started = Instant::now();
    let mut last: anyhow::Error = anyhow::anyhow!("no judge agent ran");
    let mut result = None;
    for spec in &chain {
        let left = JUDGE_BUDGET.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        let mut seat = agent::SeatState::new("dupes", &spec.id, crate::rng::entropy());
        let inv = agent::Invocation {
            cwd: repo,
            prompt: &body,
            timeout: left.min(JUDGE_TURN),
            allow_write: false,
            sessions: false,
            artifacts: &artifacts,
            stem: &format!("judge-{}", spec.id),
            run: "dupes",
            node: "dupes",
            cache_dir: None,
            attachments: &[],
            writable: &[],
        };
        let out = agent::invoke(spec, &mut seat, &inv).await;
        if agent::chain_advances(&out) {
            last = match out {
                Err(e) => e.context(format!("judge `{}` failed", spec.id)),
                Ok(o) => anyhow::anyhow!(
                    "judge `{}` gave no usable reply (exit {:?}, timed out {}, quota {})",
                    spec.id,
                    o.exit_code,
                    o.timed_out,
                    o.quota_exhausted()
                ),
            };
            continue;
        }
        result = Some(
            out.and_then(|o| parse_judgement(&o.text))
                .map(|(duplicate, reason)| Judgement {
                    duplicate,
                    reason,
                    agent: spec.id.clone(),
                }),
        );
        break;
    }
    let _ = std::fs::remove_dir_all(&artifacts);
    result.unwrap_or(Err(last))
}

/// The one decision point every caller shares. `hits` is [`check`]'s output:
/// empty passes without the judge being asked. Otherwise the judge is asked
/// once, and only a `duplicate: true` answer refuses (with its reason).
///
/// **Fail mode: open.** If the judge cannot answer (agent error, timeout,
/// quota, unparseable reply, no readable config) or the text is too long to
/// show it in full, the work is let through as [`Screened::Unjudged`] and a
/// `tracing::warn` is logged; callers also say so on stderr. A mechanical
/// match cannot tell "continue from PR #28" from a real duplicate, so refusing
/// whenever the judge is away would bring back exactly the false positives the
/// judge exists to remove. The cost is that a real duplicate slips through
/// while no agent is available, which is why it is never silent.
///
/// For a review-only request `text` is empty, so `review_branch` is put in
/// front of the judge as the thing the work is about.
pub async fn screen(
    hits: Vec<Hit>,
    text: &str,
    review_branch: Option<&str>,
    judge: &(dyn Fn(String, Vec<Hit>) -> JudgeFuture + Sync),
) -> Result<Screened, Duplicate> {
    if hits.is_empty() {
        return Ok(Screened::Clean);
    }
    let unjudged = |why: String| {
        tracing::warn!(hits = hits.len(), %why, "duplicate check: judgement unavailable, letting the work through");
        Ok(Screened::Unjudged(why))
    };
    // A judge that has not seen the whole text cannot rule on it.
    if text.chars().count() > prompt::DUPES_JUDGE_MAX_CHARS {
        return unjudged(format!(
            "the text is longer than {} characters, too long to judge in full",
            prompt::DUPES_JUDGE_MAX_CHARS
        ));
    }
    let mut subject = text.to_owned();
    if let Some(b) = review_branch {
        if !subject.is_empty() {
            subject.push_str("\n\n");
        }
        subject.push_str(&format!(
            "(This is a review-only request for branch `{b}`: it would do work on that branch.)"
        ));
    }
    match judge(subject, hits.clone()).await {
        Ok(j) if !j.duplicate => {
            tracing::info!(
                agent = %j.agent,
                reason = %j.reason,
                hits = hits.len(),
                "duplicate check: the judge says this is not duplicate work"
            );
            Ok(Screened::Cleared(j))
        }
        Ok(j) => Err(Duplicate {
            hits,
            judge: Some(format!("judge ({}): duplicate - {}", j.agent, j.reason)),
        }),
        Err(e) => unjudged(format!("{e:#}")),
    }
}

/// [`screen`] with the production judge: the `[roles] chatter` chain of `cfg`.
/// No config (`None`) means no judge, so a hit is let through unjudged.
pub async fn screen_with_config(
    hits: Vec<Hit>,
    text: &str,
    review_branch: Option<&str>,
    repo: &Path,
    cfg: Option<&Config>,
) -> Result<Screened, Duplicate> {
    let judge = |instruction: String, hits: Vec<Hit>| -> JudgeFuture {
        let cfg = cfg.cloned();
        let repo = repo.to_path_buf();
        Box::pin(async move {
            match cfg {
                Some(cfg) => {
                    let dir = repo.clone();
                    let hits = tokio::task::spawn_blocking(move || {
                        let mut hits = hits;
                        describe_forge_hits(&dir, &mut hits);
                        hits
                    })
                    .await
                    .context("describing the pull request")?;
                    chain_judge(&cfg, &repo, instruction, hits).await
                }
                None => bail!("no readable configuration to resolve a judge agent from"),
            }
        })
    };
    screen(hits, text, review_branch, &judge).await
}

/// The slice of `run.json` this module needs. Every field is optional so a
/// record from any schema still yields what it has.
#[derive(Debug, Default, Deserialize)]
struct RunView {
    #[serde(default)]
    id: String,
    #[serde(default)]
    repo: PathBuf,
    #[serde(default)]
    status: String,
    #[serde(default)]
    base_commit: String,
    #[serde(default)]
    instruction: String,
    #[serde(default)]
    candidates: Vec<CandView>,
    #[serde(default)]
    pr: Option<PrView>,
    /// The run that took this one's worktree (and pull request) over.
    #[serde(default)]
    released_to: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CandView {
    #[serde(default)]
    branch: String,
}

#[derive(Debug, Default, Deserialize)]
struct PrView {
    #[serde(default)]
    url: String,
    #[serde(default)]
    number: u64,
    #[serde(default)]
    state: String,
}

impl RunView {
    fn read(runs_root: &Path, id: &str) -> Option<Self> {
        let raw = std::fs::read_to_string(runs_root.join(id).join("run.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// An unknown status word counts as unfinished: claiming too much is the
    /// cheap error here.
    fn terminal(&self) -> bool {
        serde_json::from_value::<RunStatus>(serde_json::Value::String(self.status.clone()))
            .map(RunStatus::done)
            .unwrap_or(false)
    }

    fn pr_open(&self) -> bool {
        self.pr.as_ref().is_some_and(|p| p.state == "open")
    }
}

/// Decides whether the pull request a run's record still calls open is in fact
/// settled, so a stale record stops claiming it.
///
/// A record freezes the last state its land loop polled. A run that was handed
/// over (`released_to`) or left blocked while another run landed the same pull
/// request keeps saying `open` forever, so the record alone cannot be trusted
/// to *keep* a claim alive. Two answers, cheapest first:
///
/// 1. a released run does not own the pull request any more: ownership follows
///    `released_to`, and the successor's own record (read like any other run's)
///    decides - provided that record is readable;
/// 2. otherwise the forge is asked, once per number, at most
///    [`MAX_FORGE_LOOKUPS`] numbers, and never again after one lookup failed
///    (offline stays fast). Merged or closed means no claim; open, an error or
///    a skipped lookup means the claim stands. Claiming too much is the cheap
///    error.
struct Staleness<'a> {
    runs_root: &'a Path,
    lookup: &'a dyn Fn(&Path, u64) -> Option<PrLifecycle>,
    cache: HashMap<u64, bool>,
    asked: usize,
    failed: bool,
}

/// How many pull requests one invocation may put to the forge on behalf of
/// stale records.
const MAX_FORGE_LOOKUPS: usize = 5;

impl<'a> Staleness<'a> {
    fn new(runs_root: &'a Path, lookup: &'a dyn Fn(&Path, u64) -> Option<PrLifecycle>) -> Self {
        Self {
            runs_root,
            lookup,
            cache: HashMap::new(),
            asked: 0,
            failed: false,
        }
    }

    /// `true` when `view`'s recorded-open pull request is known not to be this
    /// run's to claim any more.
    fn pr_released(&mut self, repo: &Path, view: &RunView) -> bool {
        let Some(pr) = view.pr.as_ref().filter(|p| p.state == "open") else {
            return false;
        };
        if let Some(next) = &view.released_to
            && next != &view.id
            && RunView::read(self.runs_root, next).is_some()
        {
            return true;
        }
        if pr.number == 0 {
            return false;
        }
        if let Some(known) = self.cache.get(&pr.number) {
            return *known;
        }
        if self.failed || self.asked >= MAX_FORGE_LOOKUPS {
            return false;
        }
        self.asked += 1;
        let settled = match (self.lookup)(repo, pr.number) {
            Some(PrLifecycle::Merged | PrLifecycle::Closed) => true,
            Some(PrLifecycle::Open) => false,
            None => {
                self.failed = true;
                false
            }
        };
        self.cache.insert(pr.number, settled);
        settled
    }
}

/// Something an unfinished piece of work owns.
#[derive(Debug, Clone)]
struct Claim {
    owner: Owner,
    id: String,
    status: String,
    via: String,
    /// The owner's title and instruction, for the judge.
    about: String,
    branch: Option<String>,
    /// Commit the branch forked from; without it a SHA cannot be judged.
    base: Option<String>,
    /// `(number, url)` of an open pull request.
    pr: Option<(u64, String)>,
}

/// Look for work already in flight that `text` (and `review_branch`, for a
/// review-only run) points at. `repo` is the repository the new work is for;
/// `ignore_task` is a task being edited, whose own claims never count.
pub fn check(
    queue: &Queue,
    runs_root: &Path,
    repo: &Path,
    text: &str,
    review_branch: Option<&str>,
    ignore_task: Option<&str>,
) -> Vec<Hit> {
    check_with(
        queue,
        runs_root,
        repo,
        text,
        review_branch,
        ignore_task,
        &gh_open_pr,
        &gh_pr_state,
    )
}

/// [`check`] with the forge lookup supplied: `open_pr(repo, n)` says whether
/// pull request `n` of `repo` is open (`Some(url)`) or not / unknown (`None`).
/// It is asked only about numbers the text names that no local record already
/// explained, so a PR magi never produced (opened by hand) still collides.
/// `pr_state(repo, n)` is the forge's word on a pull request a run record calls
/// open (`None` = unreadable, which keeps the claim); see [`Staleness`].
#[allow(clippy::too_many_arguments)]
pub fn check_with(
    queue: &Queue,
    runs_root: &Path,
    repo: &Path,
    text: &str,
    review_branch: Option<&str>,
    ignore_task: Option<&str>,
    open_pr: &dyn Fn(&Path, u64) -> Option<String>,
    pr_state: &dyn Fn(&Path, u64) -> Option<PrLifecycle>,
) -> Vec<Hit> {
    let mut stale = Staleness::new(runs_root, pr_state);
    let mut idents = Idents::default();
    let here = idents.of(repo);
    let tasks = queue.list();
    let own_runs: BTreeSet<String> = tasks
        .iter()
        .filter(|t| Some(t.id.as_str()) == ignore_task)
        .flat_map(|t| t.runs.iter().cloned())
        .collect();
    // PRs the edited task's own runs opened: never a rival, forge or not.
    let own_prs: BTreeSet<u64> = own_runs
        .iter()
        .filter_map(|id| RunView::read(runs_root, id))
        .filter_map(|v| v.pr.map(|p| p.number))
        .collect();

    let mut claims: Vec<Claim> = Vec::new();
    let mut from_task: BTreeSet<String> = BTreeSet::new();
    for t in tasks
        .iter()
        .filter(|t| t.status != TaskStatus::Done && Some(t.id.as_str()) != ignore_task)
        .filter(|t| idents.of(&t.repo) == here)
    {
        claims.extend(task_claims(t, runs_root, &mut from_task, repo, &mut stale));
    }
    for id in crate::run::list_ids_in(runs_root) {
        if own_runs.contains(&id) {
            continue;
        }
        let Some(view) = RunView::read(runs_root, &id) else {
            continue;
        };
        if (view.terminal() && !view.pr_open()) || idents.of(&view.repo) != here {
            continue;
        }
        let released = stale.pr_released(repo, &view);
        // A terminal run held up only by a pull request that is not its own
        // (any more) claims nothing at all.
        if view.terminal() && released {
            continue;
        }
        claims.extend(run_claims(
            &view,
            Owner::Run,
            None,
            "its own run",
            !released,
            ("", &view.instruction),
        ));
    }

    let mut hits: Vec<Hit> = Vec::new();
    let mut push = |c: &Claim, signal: Signal, token: String| {
        let hit = Hit {
            owner: c.owner.clone(),
            id: c.id.clone(),
            status: c.status.clone(),
            signal,
            token,
            via: c.via.clone(),
            about: c.about.clone(),
        };
        if !hits.contains(&hit) {
            hits.push(hit);
        }
    };

    let prs = pr_numbers(text);
    let shas = sha_candidates(repo, text);
    for c in &claims {
        if let Some(b) = &c.branch {
            if names_branch(text, b) || review_branch == Some(b.as_str()) {
                push(c, Signal::Branch, b.clone());
            }
            if let Some(base) = &c.base {
                for sha in &shas {
                    if on_branch_only(repo, sha, b, base) {
                        push(c, Signal::Sha, short_sha(sha));
                    }
                }
            }
        }
        if let Some((n, url)) = &c.pr {
            if prs.contains(&Mention::Number(*n))
                || prs.iter().any(|p| matches!(p, Mention::Url(u) if u == url))
            {
                push(c, Signal::Pr, format!("#{n}"));
            }
        }
    }
    let mut asked = BTreeSet::new();
    for n in prs.iter().filter_map(|m| match m {
        Mention::Number(n) => Some(*n),
        Mention::Url(u) => u.rsplit('/').next().and_then(|d| d.parse().ok()),
    }) {
        let token = format!("#{n}");
        if hits
            .iter()
            .any(|h| h.signal == Signal::Pr && h.token == token)
            || own_prs.contains(&n)
            || !asked.insert(n)
        {
            continue;
        }
        if let Some(url) = open_pr(repo, n) {
            hits.push(Hit {
                owner: Owner::Pr,
                id: token.clone(),
                status: "open".into(),
                signal: Signal::Pr,
                token,
                via: format!("an open pull request with no run record here ({url})"),
                about: String::new(),
            });
        }
    }
    hits
}

/// Ask the forge, via `gh`, whether PR `n` is open. Best effort and bounded:
/// any failure (no `gh`, no remote, offline, a slow answer) is `None`, i.e.
/// today's behaviour. `GH_REPO` is dropped so the PR is looked up in `repo`'s
/// own remote, not whatever the environment points at.
fn gh_open_pr(repo: &Path, n: u64) -> Option<String> {
    let v = gh_pr_view(repo, n, "state,url")?;
    (v["state"] == "OPEN")
        .then(|| v["url"].as_str().map(str::to_owned))
        .flatten()
}

/// [`gh_open_pr`]'s question asked of a pull request a run record names: its
/// lifecycle, or `None` when the forge cannot say.
fn gh_pr_state(repo: &Path, n: u64) -> Option<PrLifecycle> {
    match gh_pr_view(repo, n, "state,url")?["state"].as_str()? {
        "OPEN" => Some(PrLifecycle::Open),
        "MERGED" => Some(PrLifecycle::Merged),
        "CLOSED" => Some(PrLifecycle::Closed),
        _ => None,
    }
}

fn gh_pr_view(repo: &Path, n: u64, fields: &str) -> Option<serde_json::Value> {
    let mut child = Command::new("gh")
        .quiet()
        .args(["pr", "view", &n.to_string(), "--json", fields])
        .current_dir(repo)
        .env_remove("GH_REPO")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut raw).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Fill in what a forge-only hit (an open pull request no record here owns)
/// is about, from its title and body. Only the judge reads it, so this runs
/// after the mechanical check, only on a hit, and best effort: an unreadable
/// forge leaves `about` empty, never drops the hit.
fn describe_forge_hits(repo: &Path, hits: &mut [Hit]) {
    for h in hits
        .iter_mut()
        .filter(|h| h.owner == Owner::Pr && h.about.is_empty())
    {
        let Some(n) = h.token.trim_start_matches('#').parse::<u64>().ok() else {
            continue;
        };
        if let Some(v) = gh_pr_view(repo, n, "title,body") {
            h.about = about_of(
                v["title"].as_str().unwrap_or(""),
                v["body"].as_str().unwrap_or(""),
            );
        }
    }
}

/// The owner's work for the judge: its title (when it has one) and its
/// instruction with whitespace collapsed, cut to 400 characters. The whole
/// instruction is used, not its first line: a heading such as `# Task` says
/// nothing about what the work is.
fn about_of(title: &str, instruction: &str) -> String {
    let body = instruction.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title.trim();
    let full = match (title.is_empty(), body.is_empty()) {
        (true, _) => body,
        (false, true) => title.to_owned(),
        (false, false) => format!("{title}: {body}"),
    };
    let mut out: String = full.chars().take(400).collect();
    if full.chars().count() > 400 {
        out.push('…');
    }
    out
}

fn task_claims(
    t: &Task,
    runs_root: &Path,
    seen: &mut BTreeSet<String>,
    repo: &Path,
    stale: &mut Staleness<'_>,
) -> Vec<Claim> {
    let status = t.status.as_str().to_owned();
    let mut out = Vec::new();
    if let Some(b) = &t.review_branch {
        out.push(Claim {
            owner: Owner::Task,
            id: t.id.clone(),
            status: status.clone(),
            via: "its review branch".into(),
            about: about_of(&t.title, &t.instruction),
            branch: Some(b.clone()),
            base: None,
            pr: None,
        });
    }
    for rid in &t.runs {
        if let Some(view) = RunView::read(runs_root, rid) {
            seen.insert(rid.clone());
            // A merged or closed pull request is not in flight, even when the
            // task that owns it is.
            let live_pr = !(view.terminal() && stale.pr_released(repo, &view));
            out.extend(run_claims(
                &view,
                Owner::Task,
                Some((&t.id, &status)),
                &format!("produced by its run {}", crate::queue::short(rid)),
                live_pr,
                (&t.title, &t.instruction),
            ));
        }
    }
    out
}

/// The claims of one run, attributed to `owner` (the task, when there is one).
///
/// `live_pr` is false when the record's open pull request is known to be
/// settled or somebody else's (see [`Staleness`]); it then names no PR.
fn run_claims(
    view: &RunView,
    owner: Owner,
    task: Option<(&str, &str)>,
    via: &str,
    live_pr: bool,
    (title, instruction): (&str, &str),
) -> Vec<Claim> {
    let about = about_of(title, instruction);
    let (id, status) = match task {
        Some((id, status)) => (id.to_owned(), status.to_owned()),
        None => (view.id.clone(), view.status.clone()),
    };
    let pr = view
        .pr
        .as_ref()
        .filter(|p| live_pr && p.state == "open" && p.number > 0)
        .map(|p| (p.number, p.url.clone()));
    let via_pr = |extra: &str| match &pr {
        Some((n, _)) => format!("{via} (PR #{n} open){extra}"),
        None => format!("{via}{extra}"),
    };
    let mut out: Vec<Claim> = view
        .candidates
        .iter()
        .filter(|c| !c.branch.is_empty())
        .map(|c| Claim {
            owner: owner.clone(),
            id: id.clone(),
            status: status.clone(),
            via: via_pr(""),
            about: about.clone(),
            branch: Some(c.branch.clone()),
            base: (!view.base_commit.is_empty()).then(|| view.base_commit.clone()),
            pr: None,
        })
        .collect();
    if pr.is_some() {
        out.push(Claim {
            owner,
            id,
            status,
            via: via_pr(""),
            about,
            branch: None,
            base: None,
            pr,
        });
    }
    out
}

/// Repository identity: the canonical git common dir, so two worktrees of one
/// repository are the same repository and a path spelled two ways is too.
#[derive(Default)]
struct Idents(HashMap<PathBuf, PathBuf>);

impl Idents {
    fn of(&mut self, path: &Path) -> PathBuf {
        self.0
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                git(
                    path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )
                .map(PathBuf::from)
                .and_then(|p| p.canonicalize().ok())
                .or_else(|| path.canonicalize().ok())
                .unwrap_or_else(|| path.to_path_buf())
            })
            .clone()
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .quiet()
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    git(cwd, args).is_some()
}

fn is_ref_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-')
}

/// Does `text` contain `branch` as a whole word? `.` and `/` before it are
/// fine (`origin/magi/x/A`), a name character on either side is not, so
/// `magi/27b2/A` does not match inside `magi/27b2/AB`.
fn names_branch(text: &str, branch: &str) -> bool {
    if branch.len() < 3 {
        return false;
    }
    text.match_indices(branch).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + branch.len()..].chars().next();
        let after_ok = match after {
            None => true,
            Some('/') => false,
            Some('.') => !text[i + branch.len() + 1..]
                .chars()
                .next()
                .is_some_and(is_ref_char),
            Some(c) => !is_ref_char(c),
        };
        before.is_none_or(|c| !is_ref_char(c) && c != '.') && after_ok
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Mention {
    Number(u64),
    Url(String),
}

/// `#48`, `PR 48`, `PR #48`, `pull request 48`, and `.../pull/48` URLs.
fn pr_numbers(text: &str) -> Vec<Mention> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let digits = |from: usize| -> Option<(u64, usize)> {
        let n = text[from..].bytes().take_while(u8::is_ascii_digit).count();
        (n > 0 && n < 10)
            .then(|| text[from..from + n].parse().ok().map(|v| (v, from + n)))
            .flatten()
    };
    for (i, _) in text.match_indices('#') {
        if let Some((n, _)) = digits(i + 1) {
            let word_before = i > 0 && is_ref_char(bytes[i - 1] as char);
            if !word_before {
                out.push(Mention::Number(n));
            }
        }
    }
    let lower = text.to_ascii_lowercase();
    for key in ["pull request ", "pr "] {
        for (i, _) in lower.match_indices(key) {
            if i > 0 && is_ref_char(bytes[i - 1] as char) {
                continue;
            }
            let from = i + key.len();
            let from = if text[from..].starts_with('#') {
                from + 1
            } else {
                from
            };
            if let Some((n, _)) = digits(from) {
                out.push(Mention::Number(n));
            }
        }
    }
    for (i, _) in text.match_indices("/pull/") {
        if let Some((_, end)) = digits(i + 6) {
            let start = text[..i]
                .rfind(|c: char| c.is_whitespace() || matches!(c, '(' | '<' | '"' | '\''))
                .map_or(0, |p| p + 1);
            out.push(Mention::Url(text[start..end].to_owned()));
        }
    }
    out
}

/// Hex words of 7..=40 chars in `text` that resolve to a commit in `repo`.
fn sha_candidates(repo: &Path, text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if !(7..=40).contains(&word.len()) || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if seen.len() >= 16 || !seen.insert(word.to_ascii_lowercase()) {
            continue;
        }
        if let Some(full) = git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{word}^{{commit}}"),
            ],
        ) {
            out.push(full);
        }
    }
    out
}

/// Is `sha` reachable from `branch` but not from `base`? A SHA already in the
/// base would match every branch there is.
fn on_branch_only(repo: &Path, sha: &str, branch: &str, base: &str) -> bool {
    let tip = format!("{branch}^{{commit}}");
    git_ok(repo, &["rev-parse", "--verify", "--quiet", &tip])
        && git_ok(repo, &["merge-base", "--is-ancestor", sha, branch])
        && !git_ok(repo, &["merge-base", "--is-ancestor", sha, base])
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Source, Task};

    struct Fx {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        runs: PathBuf,
        q: Queue,
    }

    fn sh(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .quiet()
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A repo with `main` and a branch `magi/aaaa/A` one commit ahead.
    /// Returns the fixture plus the (main, branch) tip SHAs.
    fn fx() -> (Fx, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "1").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "base"]);
        let base = sh(&repo, &["rev-parse", "HEAD"]);
        sh(&repo, &["checkout", "-q", "-b", "magi/aaaa/A"]);
        std::fs::write(repo.join("a"), "2").unwrap();
        sh(&repo, &["commit", "-q", "-am", "work"]);
        let tip = sh(&repo, &["rev-parse", "HEAD"]);
        sh(&repo, &["checkout", "-q", "main"]);
        let runs = tmp.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let q = Queue::at(tmp.path().join("queue"));
        (
            Fx {
                _tmp: tmp,
                repo,
                runs,
                q,
            },
            base,
            tip,
        )
    }

    fn write_run(f: &Fx, id: &str, status: &str, base: &str, pr: Option<(u64, &str)>) {
        let dir = f.runs.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let pr = pr.map(|(n, s)| {
            serde_json::json!({"url": format!("https://github.com/o/r/pull/{n}"), "number": n, "state": s})
        });
        let v = serde_json::json!({
            "schema": 999, "id": id, "repo": f.repo, "status": status,
            "base_commit": base, "candidates": [{"branch": "magi/aaaa/A"}], "pr": pr,
        });
        std::fs::write(dir.join("run.json"), v.to_string()).unwrap();
    }

    fn file_task(f: &Fx, status: TaskStatus, runs: &[&str]) -> Task {
        let mut t = Task::new("t".into(), "x".into(), f.repo.clone(), Source::Human);
        t.status = status;
        t.runs = runs.iter().map(|s| (*s).to_owned()).collect();
        f.q.put(&mut t).unwrap();
        t
    }

    const RID: &str = "20260901-100000-aaaa";

    fn run(f: &Fx, text: &str, review: Option<&str>) -> Vec<Hit> {
        check_with(
            &f.q,
            &f.runs,
            &f.repo,
            text,
            review,
            None,
            &|_, _| None,
            &|_, _| None,
        )
    }

    #[test]
    fn branch_matches_a_live_run_and_its_task() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let t = file_task(&f, TaskStatus::Running, &[RID]);
        let hits = run(&f, "land magi/aaaa/A onto a fresh branch.", None);
        assert!(hits.iter().any(|h| h.owner == Owner::Task
            && h.id == t.id
            && h.signal == Signal::Branch
            && h.token == "magi/aaaa/A"));
        assert!(hits.iter().any(|h| h.owner == Owner::Run && h.id == RID));
        let msg = Duplicate::new(hits).to_string();
        assert!(
            msg.contains("--force") && msg.contains("magi/aaaa/A"),
            "{msg}"
        );
    }

    #[test]
    fn branch_must_match_whole_word() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        assert!(run(&f, "see magi/aaaa/AB and magi/aaaa/A/x", None).is_empty());
    }

    #[test]
    fn sha_on_the_branch_matches_but_one_in_base_does_not() {
        let (f, base, tip) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let hits = run(&f, &format!("land commit {} please", &tip[..8]), None);
        assert!(
            hits.iter().any(|h| h.signal == Signal::Sha && h.id == RID),
            "{hits:?}"
        );
        assert!(run(&f, &format!("see {}", &base[..9]), None).is_empty());
        // Hex that resolves to nothing is ignored.
        assert!(run(&f, "deadbeef and 1234567", None).is_empty());
    }

    #[test]
    fn pr_number_matches_in_every_spelling() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        for text in [
            "finish #48",
            "PR 48 is stale",
            "pr #48",
            "pull request 48",
            "https://github.com/o/r/pull/48",
        ] {
            let hits = run(&f, text, None);
            assert!(
                hits.iter().any(|h| h.signal == Signal::Pr),
                "{text}: {hits:?}"
            );
        }
        assert!(run(&f, "see #480 and PR 4 and issue48", None).is_empty());
    }

    #[test]
    fn terminal_runs_and_done_tasks_do_not_match() {
        let (f, base, tip) = fx();
        write_run(&f, RID, "merged", &base, Some((48, "merged")));
        file_task(&f, TaskStatus::Done, &[RID]);
        let text = format!("magi/aaaa/A {} #48", &tip[..8]);
        assert!(run(&f, &text, None).is_empty());
    }

    #[test]
    fn terminal_run_with_open_pr_or_open_task_still_claims() {
        // Changed deliberately: a terminal run's recorded-open PR now claims
        // only while the forge says open or cannot be read (`run` passes an
        // unreadable forge). A merged / closed answer or a released run no
        // longer claims; see the `stale_open_*` tests below.
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        assert!(!run(&f, "magi/aaaa/A", None).is_empty());
        let (g, base, _) = fx();
        write_run(&g, RID, "ready", &base, None);
        assert!(run(&g, "magi/aaaa/A", None).is_empty());
        let t = file_task(&g, TaskStatus::Held, &[RID]);
        let hits = run(&g, "magi/aaaa/A", None);
        assert!(hits.iter().any(|h| h.id == t.id), "{hits:?}");
    }

    #[test]
    fn review_only_matches_a_branch_a_live_task_owns() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, None);
        assert!(run(&f, "", Some("magi/aaaa/A")).is_empty());
        let mut t = file_task(&f, TaskStatus::Queued, &[]);
        t.review_branch = Some("magi/aaaa/A".into());
        f.q.put(&mut t).unwrap();
        let hits = run(&f, "", Some("magi/aaaa/A"));
        assert!(
            hits.iter()
                .any(|h| h.id == t.id && h.signal == Signal::Branch)
        );
    }

    #[test]
    fn other_repository_and_edited_task_do_not_match() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let t = file_task(&f, TaskStatus::Running, &[RID]);
        let other = f._tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["init", "-q"]);
        assert!(
            check_with(
                &f.q,
                &f.runs,
                &other,
                "magi/aaaa/A",
                None,
                None,
                &|_, _| None,
                &|_, _| None,
            )
            .is_empty()
        );
        // Editing the task that owns the run never collides with itself.
        assert!(
            check_with(
                &f.q,
                &f.runs,
                &f.repo,
                "magi/aaaa/A",
                None,
                Some(&t.id),
                &|_, _| None,
                &|_, _| None,
            )
            .is_empty()
        );
    }

    #[test]
    fn a_worktree_is_the_same_repository() {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        let wt = f._tmp.path().join("wt");
        sh(
            &f.repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"],
        );
        assert!(
            !check_with(
                &f.q,
                &f.runs,
                &wt,
                "magi/aaaa/A",
                None,
                None,
                &|_, _| None,
                &|_, _| None,
            )
            .is_empty()
        );
    }

    #[test]
    fn an_open_pr_without_a_run_record_matches_through_the_forge() {
        let (f, _, _) = fx();
        let open = |_: &Path, n: u64| (n == 48).then(|| "https://example.test/pull/48".to_owned());
        let hit = |text: &str| {
            check_with(&f.q, &f.runs, &f.repo, text, None, None, &open, &|_, _| {
                None
            })
        };
        let hits = hit("finish PR #48");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].owner, Owner::Pr);
        assert!(hits[0].to_string().contains("#48"));
        // Closed / unknown / unnamed PRs never match.
        assert!(hit("finish PR #49").is_empty());
        assert!(hit("finish the work").is_empty());
    }

    #[test]
    fn a_forge_hit_does_not_repeat_a_pr_a_run_already_explains() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        let open = |_: &Path, _: u64| Some("u".to_owned());
        let hits = check_with(&f.q, &f.runs, &f.repo, "#48", None, None, &open, &|_, _| {
            None
        });
        assert!(hits.iter().all(|h| h.owner != Owner::Pr), "{hits:?}");
        assert!(!hits.is_empty());
    }

    #[test]
    fn an_edited_tasks_own_open_pr_is_not_a_forge_hit() {
        let (f, base, _) = fx();
        write_run(&f, RID, "ready", &base, Some((48, "open")));
        let t = file_task(&f, TaskStatus::Running, &[RID]);
        let open = |_: &Path, _: u64| Some("u".to_owned());
        let hits = check_with(
            &f.q,
            &f.runs,
            &f.repo,
            "#48",
            None,
            Some(&t.id),
            &open,
            &|_, _| None,
        );
        assert!(hits.is_empty(), "{hits:?}");
    }

    fn with_forge(f: &Fx, text: &str, state: Option<PrLifecycle>) -> Vec<Hit> {
        check_with(
            &f.q,
            &f.runs,
            &f.repo,
            text,
            None,
            None,
            &|_, _| None,
            &move |_, _| state,
        )
    }

    fn release(f: &Fx, id: &str, to: &str) {
        let path = f.runs.join(id).join("run.json");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["released_to"] = serde_json::json!(to);
        std::fs::write(path, v.to_string()).unwrap();
    }

    #[test]
    fn stale_open_pr_that_the_forge_says_is_merged_or_closed_does_not_claim() {
        for state in [PrLifecycle::Merged, PrLifecycle::Closed] {
            let (f, base, _) = fx();
            write_run(&f, RID, "superseded", &base, Some((48, "open")));
            assert!(with_forge(&f, "follow up on #48", Some(state)).is_empty());
            assert!(with_forge(&f, "magi/aaaa/A", Some(state)).is_empty());
            // Same through an owning, still-live task: the PR is not its claim.
            file_task(&f, TaskStatus::Held, &[RID]);
            let hits = with_forge(&f, "follow up on #48", Some(state));
            assert!(hits.iter().all(|h| h.signal != Signal::Pr), "{hits:?}");
        }
    }

    #[test]
    fn stale_open_pr_with_an_unreadable_forge_still_claims() {
        let (f, base, _) = fx();
        write_run(&f, RID, "superseded", &base, Some((48, "open")));
        let hits = with_forge(&f, "follow up on #48", None);
        assert!(hits.iter().any(|h| h.signal == Signal::Pr), "{hits:?}");
    }

    #[test]
    fn a_genuinely_open_pr_still_claims() {
        let (f, base, _) = fx();
        write_run(&f, RID, "blocked", &base, Some((48, "open")));
        let hits = with_forge(&f, "follow up on #48", Some(PrLifecycle::Open));
        assert!(hits.iter().any(|h| h.signal == Signal::Pr), "{hits:?}");
    }

    #[test]
    fn a_released_run_defers_to_its_successor_without_asking_the_forge() {
        let (f, base, _) = fx();
        let next = "20260901-110000-bbbb";
        write_run(&f, RID, "superseded", &base, Some((48, "open")));
        write_run(&f, next, "merged", &base, Some((48, "merged")));
        release(&f, RID, next);
        let asked = std::cell::Cell::new(0);
        let hits = check_with(
            &f.q,
            &f.runs,
            &f.repo,
            "follow up on #48",
            None,
            None,
            &|_, _| None,
            &|_, _| {
                asked.set(asked.get() + 1);
                None
            },
        );
        assert!(hits.is_empty(), "{hits:?}");
        assert_eq!(asked.get(), 0);
        // A successor that cannot be read decides nothing: claim stands.
        std::fs::remove_dir_all(f.runs.join(next)).unwrap();
        let hits = with_forge(&f, "follow up on #48", None);
        assert!(hits.iter().any(|h| h.signal == Signal::Pr), "{hits:?}");
    }

    #[test]
    fn forge_lookups_are_cached_and_stop_after_a_failure() {
        let (f, base, _) = fx();
        for (i, id) in ["20260901-100000-aaa1", "20260901-100000-aaa2"]
            .iter()
            .enumerate()
        {
            write_run(&f, id, "blocked", &base, Some((48 + i as u64, "open")));
        }
        let asked = std::cell::Cell::new(0);
        check_with(
            &f.q,
            &f.runs,
            &f.repo,
            "x",
            None,
            None,
            &|_, _| None,
            &|_, _| {
                asked.set(asked.get() + 1);
                None
            },
        );
        assert_eq!(
            asked.get(),
            1,
            "an unreadable forge is asked once, not per PR"
        );
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn verdict(duplicate: bool) -> Result<Judgement> {
        Ok(Judgement {
            duplicate,
            reason: "because".into(),
            agent: "j".into(),
        })
    }

    /// Run `screen` over `text` with a judge answering `answer`; returns the
    /// outcome and how many times the judge was asked.
    fn judged(
        f: &Fx,
        text: &str,
        answer: impl Fn() -> Result<Judgement> + Send + Sync + 'static,
    ) -> (Result<Screened, Duplicate>, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let judge = move |_: String, _: Vec<Hit>| -> JudgeFuture {
            seen.fetch_add(1, Ordering::SeqCst);
            let r = answer();
            Box::pin(async move { r })
        };
        let hits = run(f, text, None);
        let out = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(screen(hits, text, None, &judge));
        (out, calls.load(Ordering::SeqCst))
    }

    const NAMES: &str = "seen on magi/aaaa/A, which does not touch this file";

    fn fx_with_run() -> Fx {
        let (f, base, _) = fx();
        write_run(&f, RID, "reviewing", &base, None);
        f
    }

    #[test]
    fn a_no_ruling_lets_the_work_through() {
        let f = fx_with_run();
        let (out, calls) = judged(&f, NAMES, || verdict(false));
        assert!(matches!(out, Ok(Screened::Cleared(_))));
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_yes_ruling_refuses_and_says_why() {
        let f = fx_with_run();
        let (out, _) = judged(&f, NAMES, || verdict(true));
        let msg = out.unwrap_err().to_string();
        assert!(
            msg.contains("duplicate - because") && msg.contains("--force"),
            "{msg}"
        );
        assert!(msg.contains("magi/aaaa/A"), "{msg}");
    }

    #[test]
    fn a_failing_judge_lets_it_through_unjudged() {
        let f = fx_with_run();
        let (out, calls) = judged(&f, NAMES, || Err(anyhow::anyhow!("quota")));
        match out {
            Ok(Screened::Unjudged(why)) => assert!(why.contains("quota"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(calls, 1);
    }

    #[test]
    fn garbage_replies_do_not_parse() {
        assert!(parse_judgement("sure, go ahead").is_err());
        assert!(parse_judgement(r#"{"duplicate":"maybe","reason":"x"}"#).is_err());
        assert!(parse_judgement(r#"{"duplicate":false}"#).is_err());
        assert!(parse_judgement(r#"{"duplicate":false,"reason":"  "}"#).is_err());
        let two = "{\"duplicate\":false,\"reason\":\"c\"}\n{\"duplicate\":true}";
        assert!(parse_judgement(two).is_err());
        assert!(parse_judgement("ok {\"duplicate\":false,\"reason\":\"c\"}").is_err());
        let (d, why) = parse_judgement("{\"duplicate\":true,\"reason\":\"same\\nPR\"}").unwrap();
        assert_eq!((d, why.as_str()), (true, "same PR"));
    }

    #[test]
    fn a_text_too_long_to_judge_in_full_passes_without_asking() {
        let f = fx_with_run();
        let long = format!("{NAMES} {}", "x".repeat(prompt::DUPES_JUDGE_MAX_CHARS));
        let (out, calls) = judged(&f, &long, || verdict(false));
        assert!(matches!(out, Ok(Screened::Unjudged(w)) if w.contains("too long to judge")));
        assert_eq!(calls, 0);
    }

    #[test]
    fn no_hit_never_asks_the_judge() {
        let (f, _, _) = fx();
        let (out, calls) = judged(&f, "nothing named here", || verdict(true));
        assert_eq!(out.unwrap(), Screened::Clean);
        assert_eq!(calls, 0);
    }

    #[test]
    fn without_a_config_a_hit_passes_unjudged() {
        let f = fx_with_run();
        let hits = run(&f, NAMES, None);
        let out = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(screen_with_config(hits, NAMES, None, &f.repo, None));
        assert!(matches!(out, Ok(Screened::Unjudged(_))));
    }

    #[test]
    fn about_carries_title_and_whole_instruction() {
        assert_eq!(
            about_of("Retries", "# Task\nFix  auth\nretries"),
            "Retries: # Task Fix auth retries"
        );
        assert_eq!(about_of("", "x"), "x");
        assert_eq!(about_of("", &"y".repeat(500)).chars().count(), 401);
    }
}
