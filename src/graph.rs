//! The competition graph.
//!
//! ```text
//! prep ──► implement ×N ──► judge ×M (blind) ──► split? ──► deliberate ──► vote (private)
//!                                                   │                          │
//!                                                   └──── unanimous ───────────┤
//!                                                                              ▼
//!   merge ◄── gate ◄── review ×R + E2E, fix, repeat ◄── fold losers ◄──────── tally
//! ```
//!
//! Every node persists before the next one starts, so a run can be resumed
//! after a crash, a rate limit, or a reboot without re-spending the work that
//! already landed.
//!
//! The design decision that matters most is *where the facilitator lives*.
//! There is no moderator agent: magi assigns the labels, decides the
//! presentation order, relays the transcript, and collects the final votes
//! one-to-one. A moderator that never learns an author cannot leak one.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use jiff::Timestamp;
use tokio::sync::Semaphore;

use crate::advise;
use crate::agent::{self, AgentOutput, Invocation, SeatState};
use crate::ask;
use crate::blind;
use crate::bump;
use crate::config::{
    AgentSpec, Config, IncompleteReviewPolicy, LeakPolicy, MergeMode, MergeStyle, Prompts,
    ResolvedRoles,
};
use crate::git;
use crate::land;
use crate::proc::Quiet as _;
use crate::prompt::{
    self, CandidateView, Lens, ReviewPatch, ReviewReconsiderCtx, ReviewSeatReport, Turn,
};
use crate::queue;
use crate::run::{
    BaseSync, Candidate, CommandOutcome, ContinuationOutcome, ContinuationRecord,
    DeliberationRound, DeliberationTurn, E2eStatus, FixRecord, JobRecord, JobStatus, Judgement,
    MergeOutcome, OperatorFixFinding, OperatorFixOutcome, OperatorFixRequest, QuotaLoss,
    ReviewRecord, ReviewRevoteRecord, ReviewRound, RunState, RunStatus, Tally, VoteRecord, tail,
    write_artifact,
};
use crate::verdict::{
    self, FinalVote, Finding, FixReport, Position, Proposal, Ranking, Review, ReviewRevote,
    ReviewVote, Severity,
};

/// How much verification output is kept and fed back to the fixer.
const OUTPUT_TAIL: usize = 8_000;

/// Bytes of a failing command's output kept in an event, so the reason a run
/// stopped is readable from the report without opening `run.json`.
const EVENT_OUTPUT_TAIL: usize = 2_000;

/// How often [`wait_for_timed_out_children_to_die`] re-checks a timed-out
/// command's pid before releasing the build cache's lease.
const LEASE_RELEASE_POLL: Duration = Duration::from_secs(1);

/// The most [`wait_for_timed_out_children_to_die`] will wait for a timed-out
/// command's pid to actually exit before giving up and releasing anyway.
///
/// A timeout means the process was asked to die (`kill_on_drop`,
/// `start_kill`), not that it already has — on Windows in particular that can
/// take a moment, the same reason `agent`'s own `PIPE_GRACE` exists. Releasing
/// the instant the command returns would let the very next acquirer (this
/// run's own next round, another run's verification, the janitor's prune)
/// start touching the same directory while it might still be writing to it,
/// so this polls the actual pid — real confirmation, not a fixed guess —
/// until it is gone or this ceiling is reached. It is still not full
/// process-tree reaping: a grandchild the timed-out process spawned and that
/// outlives it independently is invisible to a pid check, and continuing to
/// observe and collect *that* stays a different piece of work with its own
/// owner. Set generously because the common case returns early the moment
/// the pid is confirmed gone, not because every timeout pays this in full.
const LEASE_RELEASE_MAX_WAIT: Duration = Duration::from_secs(30);

/// Consecutive review rounds with no tree progress (see
/// [`crate::run::ReviewRound::progressed`]) before `review_loop` hands off
/// instead of spending the rest of the round budget.
///
/// Not 1: a single non-progressing round is not yet a pattern — a fixer that
/// legitimately finds nothing left to change (its previous round's fix already
/// covered it, and this round's reviewers re-raised only nits) looks the same
/// as one that is spinning, for exactly one round. Two in a row is where the
/// two stop being distinguishable, and a review round on this workload has
/// been measured at 30-45 minutes of reviewer-plus-fixer agent time, so a
/// third attempt at a tree that has not moved twice running is pure cost.
/// This does not touch `review_rounds` itself, which stays the operator's
/// call.
pub(crate) const STAGNANT_LIMIT: usize = 2;

/// How many times [`Runner::sync_to_base`] will re-land the winner's tree on
/// a base that moved before giving up and leaving the run `Blocked` for a
/// person.
///
/// Mirrors `land::Step::Rebase`'s budget and the reasoning behind it: a base
/// that keeps moving faster than a run can catch it is not something more
/// rebasing fixes, it is a person's call. Not the same *number as*
/// `land_rounds` - this budget is spent before a pull request exists, land's
/// after - but bounded for the identical reason, so it uses the same
/// default. Counted across both call sites in [`Runner::finish_after_tally`]
/// (once before review, once before the gate), because either one finding
/// the base still moving is the same signal.
const BASE_SYNC_ROUNDS: usize = 4;

/// How many times [`Runner::continue_fix_report`] will resume the fixer's own
/// seat when its CLI turn ended cleanly — usable, non-empty, exit 0 — but the
/// reply held no [`FixReport`].
///
/// The shape this recovers: run 20260912-114326-d3b8's fix-2 came back
/// `subtype=success`/`is_error=false`/`stop_reason=end_turn` with the reply
/// "I'll pause here until the `cargo make check` background run reports
/// back." — a CLI turn that ended cleanly while the fixer's own job had not.
/// No `FixReport` was ever collected from that seat, and the run moved on to
/// the next review round regardless.
///
/// Bounded independently of `review_rounds` and `graph.retries`: this
/// recovers one seat's missing report mid-round, not a new round of review or
/// an ordinary parse retry, and must not itself become the unbounded wait the
/// rest of this module exists to avoid.
const MAX_FIX_CONTINUATIONS: usize = 2;

/// One queued agent invocation.
///
/// `Clone` so a node can keep the jobs it sent and re-send one: a seat whose
/// CLI hung up on its own stream is asked again from the same job rather than
/// rebuilt from scratch. See [`Runner::resume_undelivered`].
#[derive(Clone)]
struct SeatJob {
    spec: AgentSpec,
    seat: SeatState,
    cwd: PathBuf,
    prompt: String,
    timeout: Duration,
    allow_write: bool,
    sessions: bool,
    artifacts: PathBuf,
    stem: String,
}

/// How the graph reads one agent invocation.
///
/// Quota is split out from an ordinary failure on purpose: a rate-limited call
/// is known to fail again if retried now, so the retry loop must not spend an
/// attempt on it. `Dropped` is split out for the opposite reason: unlike
/// `Failed`, it is worth re-asking, and unlike `Ok`, its text is the CLI's raw
/// error JSON, never the agent's answer — a caller that matched only
/// `Ok`/`Quota`/`Failed` before `Dropped` existed must be updated rather than
/// left to read that JSON as if it were usable output. `resume_undelivered`
/// is the only caller that acts on it; everywhere else it is reported like an
/// ordinary failure.
enum AgentOutcome {
    /// A usable output.
    Ok(AgentOutput),
    /// The CLI ran out of quota / rate limit. Retrying now is pointless.
    Quota(AgentOutput),
    /// The CLI hung up on its own stream after billed work. See
    /// [`agent::AgentOutput::work_undelivered`].
    Dropped(AgentOutput),
    /// Any other failure: a timeout, a bad exit code, an empty reply.
    Failed(String),
}

/// A request to park the run at its next node boundary.
///
/// Cloning is how the request travels: the loop keeps one handle and hands a
/// clone to each [`Runner`], and every clone points at the same flag. There
/// is no channel because there is nothing to send - the only message is
/// "park", it is idempotent, and a flag cannot be missed by a receiver that
/// was not listening yet.
///
/// The boundary is what makes this cheap. Every node writes the run's state
/// before the next one starts, and every node skips what is already recorded:
/// `prep` returns early once candidates exist, `implement` asks only the seats
/// with nothing on disk, `judge` returns early once judgements exist. So a
/// parked run resumes into exactly the node it stopped before, and no agent
/// work is thrown away. Killing the process mid-node, by contrast, loses
/// whatever the seats in flight had not yet written - which for an implement
/// wave is an hour of paid work.
///
/// A [`Runner`] watches two independent handles of this type - see
/// [`Runner::on_pause`] and [`Runner::watch_interrupt`] - never one shared
/// between them. `magi serve`'s own shutdown (`Stop::park`) hands out one
/// clone covering the whole daemon's lifetime and is never asked to un-park,
/// which is correct exactly because nothing is dispatched after it fires.
/// `magi serve`'s interrupt scheduler needs the opposite lifetime - a run
/// that parks for an interrupted task must go on to run other tasks
/// afterward - so it mints a fresh, unshared [`Pause`] per run instead of
/// reusing the daemon-wide one.
#[derive(Debug, Clone, Default)]
pub struct Pause(Arc<AtomicBool>, Arc<Mutex<Option<String>>>);

impl Pause {
    /// A pause nobody has asked for yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the run to park at its next node boundary. Idempotent.
    pub fn park(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Same as [`Pause::park`], but records why, for [`Runner::park_here`] to
    /// fold into the run's own `park` event - so an operator reading the run
    /// later knows this was a deliberate interrupt rather than a shutdown or
    /// a binary swap. The first reason recorded wins; a park already in
    /// flight is not relabelled by a second, unrelated request.
    pub fn park_because(&self, reason: impl Into<String>) {
        let mut reason_guard = self
            .1
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reason_guard.is_none() {
            *reason_guard = Some(reason.into());
        }
        drop(reason_guard);
        self.park();
    }

    /// Has a park been asked for?
    #[must_use]
    pub fn parked(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Why the park was asked for, when the caller used [`Pause::park_because`].
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        self.1
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Drives one run.
pub struct Runner {
    /// Run state; public so the CLI can report on it.
    pub state: RunState,
    roles: ResolvedRoles,
    sem: Arc<Semaphore>,
    /// Set when the daemon's own shutdown (Ctrl-C, a binary swap) wants the
    /// run parked at its next node boundary. See [`Pause`]'s own doc for why
    /// this is never the same handle as `interrupt`.
    pause: Pause,
    /// Set when `magi serve`'s interrupt scheduler wants this specific run
    /// parked at its next node boundary, to let a task marked
    /// [`crate::queue::Task::interrupt`] run alone before this one carries
    /// on. Unlike `pause`, a fresh, unshared handle per run - see
    /// [`Runner::watch_interrupt`].
    interrupt: Pause,
}

/// The commit a run branches from: the base branch as the remote has it.
///
/// Two failures this replaces. A run used to branch off `HEAD` and so refused
/// to start on a dirty tree, which made `magi serve` decline every task for as
/// long as the operator had work in progress - most of the time. Branching off
/// the *local* base branch fixed that and introduced a worse one: `land` merges
/// the winner on GitHub, nothing updates the local ref, and the next run
/// branches off a base missing everything the previous runs landed. Two tasks
/// in a row from a phone would have had the second silently re-implementing
/// against stale code and opening a pull request that reverted the first.
///
/// Only refs move here - no checkout, no local branch, no merge - so it is safe
/// with uncommitted work in the tree. A machine with no network still starts:
/// the fetch may fail and the local tip is used with a warning, because
/// refusing to run offline is a worse failure than running against a base the
/// operator can see for themselves.
///
/// One function, called by both entry points. Two answers to "where does a run
/// branch from" is the kind of drift nobody notices until a diff is wrong.
async fn resolve_base(repo: &Path, base_branch: &str, remote: &str) -> Result<String> {
    let tracking = format!("{remote}/{base_branch}");
    let fetched = git::fetch(repo, remote, base_branch).await;
    if let Ok(out) = &fetched
        && out.ok()
        && git::rev_exists(repo, &tracking).await
    {
        return git::rev_parse(repo, &tracking).await;
    }
    let why = match &fetched {
        Ok(out) if !out.ok() => out.stderr.lines().next().unwrap_or("").to_owned(),
        Ok(_) => format!("{remote} has no {base_branch}"),
        Err(e) => e.to_string(),
    };
    tracing::warn!(
        "could not read {tracking} ({why}); branching off the local \
         {base_branch} instead, which may be behind"
    );
    git::rev_parse(repo, base_branch).await.with_context(|| {
        format!(
            "cannot resolve `{base_branch}`; set [merge] base in magi.toml to a \
             branch that exists"
        )
    })
}

/// Exclusive claim on one run's `magi fix` step, released on drop — including
/// on an early return or a panic.
///
/// `daemon::is_working_on` only sees a heartbeat-publishing daemon; two
/// manual `magi fix` invocations against the same run are otherwise
/// invisible to each other and would race to remove and recreate the same
/// worktree (see [`Runner::fix_selected`]). The lock file itself is the same
/// `create_new` shape as `queue::Claim`, but unlike a queued task's lock —
/// which is only ever reclaimed later, out of band, by
/// `daemon::sweep_stale_claims` running inside `magi serve`/`magi web` — a
/// `magi fix` invocation is not necessarily running under either of those, so
/// nothing would ever sweep a lock a killed or crashed process left behind.
/// [`Self::acquire`] therefore reclaims a stale lock itself, on the same
/// conservative PID-liveness policy `sweep_stale_claims` and `cache`'s own
/// lease use: an unreadable or unparsable pid, or a liveness query the
/// platform cannot answer, reads as alive and the lock is left in place.
struct FixClaim {
    path: PathBuf,
}

impl FixClaim {
    fn acquire(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join("fix.lock");
        match Self::create(&path) {
            Ok(claim) => Ok(claim),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if Self::reclaim_if_dead(&path) {
                    Self::create(&path).with_context(|| format!("lock {}", path.display()))
                } else {
                    bail!(
                        "another `magi fix` is already running for this run ({} exists)",
                        path.display()
                    )
                }
            }
            Err(e) => Err(e).with_context(|| format!("lock {}", path.display())),
        }
    }

    fn create(path: &Path) -> std::io::Result<Self> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        use std::io::Write as _;
        // Read back by `reclaim_if_dead` on a later, stuck invocation.
        writeln!(f, "{}", std::process::id())?;
        Ok(Self {
            path: path.to_owned(),
        })
    }

    /// True if the lock named a process confirmed dead, in which case it was
    /// also removed. Never true on an unreadable file, an unparsable pid, or
    /// a liveness query the platform cannot answer — see this type's own doc.
    fn reclaim_if_dead(path: &Path) -> bool {
        let dead = std::fs::read_to_string(path)
            .ok()
            .and_then(|body| body.trim().parse::<u32>().ok())
            .is_some_and(|pid| !crate::proc::pid_alive(pid));
        dead && std::fs::remove_file(path).is_ok()
    }
}

impl Drop for FixClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Runner {
    /// Start a fresh run against `repo`.
    pub async fn start(repo: &Path, instruction: String, config: Config) -> Result<Self> {
        let repo = git::toplevel(repo).await?;
        let missing = agent::missing_programs(&config.agents);
        if !missing.is_empty() {
            bail!(
                "these agent programs are not on PATH: {}. Fix the roster in \
                 magi.toml or install them.",
                missing.join(", ")
            );
        }
        let base_branch = match config.merge.base.clone() {
            Some(b) => b,
            None => git::current_branch(&repo)
                .await?
                .context("HEAD is detached; set [merge] base in magi.toml")?,
        };
        let base_commit = resolve_base(&repo, &base_branch, &config.merge.remote).await?;
        // Still worth saying out loud. The operator's uncommitted work is not
        // part of this run, and someone watching a candidate fail to use a
        // change they just made deserves to know why.
        if !git::is_clean(&repo).await? {
            tracing::warn!(
                "{} has uncommitted changes; they are not part of this run, \
                 which branches off {base_branch} ({})",
                repo.display(),
                &base_commit[..base_commit.len().min(8)]
            );
        }
        let roles = config.resolve_roles()?;
        let max_parallel = config.graph.max_parallel.max(1);
        let mut state = RunState::new(repo, base_branch, base_commit, instruction, config);
        state.event("start", format!("run {} created", state.id));
        state.save()?;
        Ok(Self {
            state,
            roles,
            sem: Arc::new(Semaphore::new(max_parallel)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        })
    }

    /// Open a review-only run against work that already exists on `branch`.
    ///
    /// The expensive half of the graph is the implement wave — measured at
    /// 111 and 134 internal tool-loop turns on this repository, against a
    /// handful for a judge or a reviewer. The cheap half is worth running on
    /// hand-written work too, and there was no way to reach it.
    ///
    /// No new state and no schema change are needed: a run with **one** viable
    /// candidate and a tally already decided degrades `execute` to exactly
    /// review → gate → merge, because `judge` skips a single-candidate field,
    /// `deliberate` has fewer than two first choices to reconcile, `vote`
    /// returns early, `tally` is already present and `fold_losers` has no
    /// losers. Resuming such a run therefore does the right thing as well.
    pub async fn review(repo: &Path, branch: &str, config: Config) -> Result<Self> {
        let repo = git::toplevel(repo).await?;
        let missing = agent::missing_programs(&config.agents);
        if !missing.is_empty() {
            bail!(
                "these agent programs are not on PATH: {}. Fix the roster in \
                 magi.toml or install them.",
                missing.join(", ")
            );
        }
        if !git::branch_exists(&repo, branch).await? {
            bail!("no branch `{branch}` in {}", repo.display());
        }
        let base_branch = match config.merge.base.clone() {
            Some(b) => b,
            None => git::current_branch(&repo)
                .await?
                .context("HEAD is detached; set [merge] base in magi.toml")?,
        };
        if base_branch == branch {
            bail!("`{branch}` is the base branch; there is nothing to review against");
        }
        let base_commit = resolve_base(&repo, &base_branch, &config.merge.remote).await?;

        let roles = config.resolve_roles()?;
        let max_parallel = config.graph.max_parallel.max(1);
        // The commit subjects are the closest thing to a task statement that
        // existing work carries, and the reviewers are told as much.
        let log = git::log_oneline(&repo, &base_commit, branch)
            .await
            .unwrap_or_default();
        let instruction = format!(
            "Review the work already on branch `{branch}`. There is no task \
             statement: what the change claims to do is whatever its commits \
             say.\n\n{}",
            if log.trim().is_empty() {
                "(no commit messages)"
            } else {
                log.trim()
            }
        );
        let mut state = RunState::new(
            repo.clone(),
            base_branch,
            base_commit.clone(),
            instruction,
            config,
        );

        // An attached worktree, so the fixer's commits land on the branch under
        // review rather than on a detached head nobody will look at again.
        let worktree = state.worktree_root().join("under-review");
        if let Some(parent) = worktree.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let path = worktree.to_string_lossy().to_string();
        git::git(&repo, &["worktree", "add", &path, branch])
            .await
            .with_context(|| {
                format!("checking out `{branch}` at {path} (is it checked out elsewhere?)")
            })?;

        let commits = git::commits_ahead(&worktree, &base_commit, "HEAD")
            .await
            .unwrap_or(0);
        if commits == 0 {
            bail!("`{branch}` has no commits beyond {}", short(&base_commit));
        }
        let files = git::changed_files(&worktree, &base_commit, "HEAD")
            .await
            .map(|f| f.len())
            .unwrap_or(0);
        let stat = git::diff_stat(&worktree, &base_commit, "HEAD")
            .await
            .unwrap_or_default();

        state.candidates.push(Candidate {
            index: 0,
            label: 'A',
            // Not an agent id on purpose: nothing in the roster wrote this, and
            // the stats tables must not credit anyone with a win for it.
            agent: "(existing branch)".to_owned(),
            branch: branch.to_owned(),
            worktree,
            summary: String::new(),
            stat,
            files,
            commits,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        });
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 0)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 0,
            unanimous_initial: false,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: false,
            tie_break: None,
            // No panel sat, so no quorum applies. Zero judges is the correct
            // number for work that never competed, and must not be reported as
            // a collapsed panel.
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("review-only run: nothing competed".to_owned()),
        });
        state.status = RunStatus::Reviewing;
        state.event(
            "start",
            format!(
                "review-only run {} on `{branch}` ({files} files, {commits} commits)",
                state.id
            ),
        );
        state.save()?;
        Ok(Self {
            state,
            roles,
            sem: Arc::new(Semaphore::new(max_parallel)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        })
    }

    /// Reopen an existing run.
    pub fn resume(id: &str) -> Result<Self> {
        let state = RunState::load(id)?;
        let roles = state.config.resolve_roles()?;
        let max_parallel = state.config.graph.max_parallel.max(1);
        Ok(Self {
            state,
            roles,
            sem: Arc::new(Semaphore::new(max_parallel)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        })
    }

    /// Walk the graph to a terminal state, skipping nodes already recorded.
    pub async fn execute(&mut self) -> Result<()> {
        // Moving again, so it is no longer parked. Set before the walk rather
        // than in `resume`, so every way of re-entering the graph clears it
        // and a card cannot claim a run is waiting to be resumed while the
        // agents are already working.
        self.state.parked = false;
        // Any seat this state still lists as answering belongs to whatever
        // process last drove this run — this one included, if it crashed
        // mid-wave. Cleared and flushed immediately, before anything else
        // runs, so a resume can never show a seat as live when nothing is
        // asking it anything yet; the node that actually dispatches the next
        // wave repopulates it.
        if self.state.clear_active() {
            self.state.save()?;
        }
        // A run that already lost its quorum never resumes into the verdict
        // machinery: `deliberate` and `vote` would otherwise clobber the
        // stalled marker back to Voting and the run would keep going past a
        // verdict that is no longer trustworthy. Everything already recorded is
        // kept, so the run stays resumable (or foldable) for a human to pick up.
        //
        // On --resume the run gets one chance to repair itself: the seats a
        // rate limit took out are re-asked. If their quota has since reset and
        // the quorum is restored, the run picks up and finishes; otherwise it
        // stays stale and still-resumable for a later retry. If it does not
        // recover, the returned status stays `Stalled` and nothing was
        // clobbered (the recovery only mutates entries for the lost seats).
        if self.state.status == RunStatus::Stalled {
            if self.recover_stall().await? {
                self.finish_after_tally().await?;
            } else {
                // Still below quorum: persist the marker and stay resumable.
                self.state.save()?;
            }
            return Ok(());
        }
        // A run parked inside `land` - watching CI, mid fix-round, or
        // waiting on the owner's merge approval - resumes directly into it,
        // never back through `prep`. Everything before `merge` already
        // concluded; that is the only way `status` reaches `Landing` in the
        // first place. Re-walking `review_loop` first would also be actively
        // wrong: its own status recomputation (see its doc) treats any
        // clean round as reason to set `status` to `Gating`, which would
        // clobber this marker before `merge` ever ran, and this run would
        // never find its way back into `land` at all.
        if self.state.status == RunStatus::Landing {
            self.run_land().await?;
            // `run_land` may have settled the run right here - CI came back
            // green and the PR merged, say - without ever passing back
            // through `merge`'s own trailing call. Whatever it left `status`
            // as is what this has to read.
            self.settle_questions();
            return Ok(());
        }
        self.prep().await?;
        if self.park_here()? {
            return Ok(());
        }
        self.advise().await?;
        if self.park_here()? {
            return Ok(());
        }
        self.implement().await?;
        if self.park_here()? {
            return Ok(());
        }
        // `after_implement` already saved the state and settled any open
        // questions when it set this; nothing later in the graph has
        // anything to judge.
        if self.state.status == RunStatus::VerifiedNoop {
            return Ok(());
        }
        self.judge().await?;
        if self.park_here()? {
            return Ok(());
        }
        self.deliberate().await?;
        if self.park_here()? {
            return Ok(());
        }
        self.vote().await?;
        if self.park_here()? {
            return Ok(());
        }
        self.tally()?;
        // A verdict that lost its quorum is not trustworthy: do not review,
        // gate, or merge on it. Everything already done is kept, so the run
        // stays resumable (or foldable); the human can replace the agent that
        // ran out of quota and pick it up.
        if self.state.status == RunStatus::Stalled {
            // Persist the stalled marker now — the normal end-of-execute save
            // below is below this early return, and without it a resumed run
            // would reload a pre-tally status and keep going.
            self.state.save()?;
            return Ok(());
        }
        self.finish_after_tally().await?;
        Ok(())
    }

    /// Park here if asked to, recording it in the run's own timeline.
    ///
    /// Returns whether the caller should stop walking the graph. The state is
    /// saved either way by the node that just finished; this adds the event so
    /// the operator's card says why a run that is neither finished nor moving
    /// is sitting where it is.
    fn park_here(&mut self) -> Result<bool> {
        // Either handle asking is enough - see `Pause`'s own doc for why
        // they are never the same one. `interrupt` is checked second so a
        // reason it carries is preferred in the message below over a plain
        // shutdown park racing it at the same boundary.
        if !self.pause.parked() && !self.interrupt.parked() {
            return Ok(false);
        }
        let why = match self.interrupt.reason().or_else(|| self.pause.reason()) {
            Some(reason) => format!(
                "parked after `{}` ({reason}) — resume to carry on from here",
                self.state.status.as_str()
            ),
            None => format!(
                "parked after `{}` — resume to carry on from here",
                self.state.status.as_str()
            ),
        };
        self.state.event("park", why);
        self.state.parked = true;
        self.state.save()?;
        Ok(true)
    }

    /// Hand the runner the pause `magi serve`'s own shutdown watches.
    pub fn on_pause(&mut self, pause: Pause) {
        self.pause = pause;
    }

    /// Hand the runner a second, independent pause: `magi serve`'s interrupt
    /// scheduler asking this one run - and no other - to park so a task
    /// marked [`crate::queue::Task::interrupt`] can run alone. See
    /// [`Pause`]'s own doc for why this is never [`Runner::on_pause`]'s
    /// handle.
    pub fn watch_interrupt(&mut self, pause: Pause) {
        self.interrupt = pause;
    }

    /// Abandon this run's own open questions, once `status` has actually
    /// settled rather than merely paused.
    ///
    /// `Blocked` and `Stalled` are `RunStatus::resumable` — a human can pick
    /// either back up with the candidates, the review round and the seat
    /// sessions already on disk, so a question an implementer asked mid-round
    /// may still get a real answer read by a real resume. Only the statuses
    /// `resumable` excludes are actually final: the run merged, it reached
    /// `Ready` with nothing left to do, it failed outright with no
    /// established point to continue from, or every candidate agreed, with
    /// evidence, that nothing belonged in the worktree (`VerifiedNoop`). In
    /// every one of those the seat that asked is gone for good, exactly like
    /// the run being deleted under `magi run rm` - so the same cleanup
    /// applies, worded for what actually happened instead of "the run was
    /// deleted".
    ///
    /// Best-effort and silent on success: called from every place `status`
    /// can land on one of those three, including ones a resumed run revisits,
    /// so it must cost nothing when there was nothing open to begin with.
    fn settle_questions(&mut self) {
        if let Err(e) = ask::Questions::open().settle_run(&self.state.id, self.state.status) {
            tracing::warn!("abandon questions for {}: {e:#}", self.state.id);
        }
    }

    /// The tail of the graph after a trustworthy tally: fold losers, review,
    /// gate, merge, and persist.
    async fn finish_after_tally(&mut self) -> Result<()> {
        self.fold_losers().await?;
        // Before review starts, and again right before the gate: a run's
        // review rounds can themselves take long enough for the base to move
        // a second time, and the gate is the one node whose "green" gets
        // acted on.
        self.sync_to_base().await?;
        self.review_loop().await?;
        self.sync_to_base().await?;
        self.gate().await?;
        self.merge().await?;
        self.state.save()?;
        Ok(())
    }

    // ---------------------------------------------------------------- prep

    async fn prep(&mut self) -> Result<()> {
        if !self.state.candidates.is_empty() {
            return Ok(());
        }
        self.state.status = RunStatus::Prep;
        let repo = self.state.repo.clone();
        let base = self.state.base_commit.clone();
        let root = self.state.worktree_root();
        let labels = blind::assign_labels(self.roles.implementers.len(), self.state.seed);

        // The hook is the write-time half of the blindness contract; the
        // presentation filter in `blind` is the half that cannot be bypassed.
        let hooks_dir = self.state.dir().join("hooks");
        if self.state.config.blind.commit_msg_hook {
            std::fs::create_dir_all(&hooks_dir)
                .with_context(|| format!("create {}", hooks_dir.display()))?;
            let script = blind::commit_msg_hook(&self.state.config.blind.strip_lines);
            let path = hooks_dir.join("commit-msg");
            std::fs::write(&path, script).with_context(|| format!("write {}", path.display()))?;
            make_executable(&path)?;
            // Ref-counted rather than a plain idempotent set: with more than
            // one run able to be in flight in the same repository at once
            // (see `Config::daemon.max_concurrent_runs`), a bare "already
            // true?" check cannot tell "another run of mine still needs
            // this" from "nobody does", and the run that happens to finish
            // first would disable the hook out from under a sibling still
            // relying on it.
            git::acquire_worktree_config(&repo).await?;
            self.state.enabled_worktree_config = true;
        }

        for (index, (spec, label)) in self
            .roles
            .implementers
            .clone()
            .into_iter()
            .zip(labels)
            .enumerate()
        {
            let branch = self.state.branch_for(label);
            let worktree = root.join(format!("cand-{label}"));
            git::worktree_add_branch(&repo, &worktree, &branch, &base).await?;
            if self.state.config.blind.commit_msg_hook {
                git::set_worktree_hooks_path(&worktree, &hooks_dir).await?;
            }
            git::local_exclude(&worktree, "/.magi/").await?;
            self.state.candidates.push(Candidate {
                index,
                label,
                agent: spec.id.clone(),
                branch,
                worktree,
                summary: String::new(),
                stat: String::new(),
                files: 0,
                commits: 0,
                empty: false,
                failed: None,
                verified_noop: None,
                duration_ms: 0,
                folded: false,
            });
        }

        for j in 1..=self.roles.judges.len() {
            let wt = root.join(format!("judge-{j}"));
            if !wt.exists() {
                git::worktree_add_detached(&repo, &wt, &base).await?;
            }
        }

        // Disposable, detached checkouts for the design-deliberation stage's
        // advisor seats — the same shape as the judges' above, at the same
        // base commit, since advisors also only ever read. Sized off the
        // configured count directly rather than a resolved roster: unlike
        // `implementers`/`judges`/`reviewers`, advisor seats are resolved
        // lazily inside `advise` itself (see `Config::advisors`'s doc), so
        // `prep` has no `ResolvedRoles` field to read a count from here.
        if self.state.config.graph.advise {
            for k in 1..=self.state.config.graph.advisors {
                let wt = root.join(format!("advisor-{k}"));
                if !wt.exists() {
                    git::worktree_add_detached(&repo, &wt, &base).await?;
                }
            }
        }

        // A judge cannot tell it is looking at its own patch — the seats keep
        // separate conversations — but a panel that shares agents with the
        // field is less independent than it looks, and that is worth saying out
        // loud once per run rather than leaving it in the config.
        let authors: Vec<&str> = self
            .roles
            .implementers
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        let overlap: Vec<String> = self
            .roles
            .judges
            .iter()
            .enumerate()
            .filter(|(_, j)| authors.contains(&j.id.as_str()))
            .map(|(i, j)| format!("judge {} = {}", i + 1, j.id))
            .collect();
        if !overlap.is_empty() {
            let note = format!(
                "{} also authored a candidate; blind, but the panel is less \
                 independent than {} distinct agents would be",
                overlap.join(", "),
                self.roles.judges.len()
            );
            self.state.event("prep", note);
        }

        self.state.event(
            "prep",
            format!(
                "{} candidates, {} judges, base {} ({})",
                self.state.candidates.len(),
                self.roles.judges.len(),
                &self.state.base_commit[..7.min(self.state.base_commit.len())],
                self.state.base_branch
            ),
        );
        self.state.status = RunStatus::Implementing;
        self.state.save()?;
        Ok(())
    }

    // -------------------------------------------------------------- advise

    /// The design-deliberation stage: independent, read-only advisor seats
    /// each sketch a design before any implementer touches the repository,
    /// and (when at least one produced a usable proposal) a synthesis seat
    /// blends them into a brief `implement` carries in every candidate's
    /// prompt.
    ///
    /// `[graph] advise` is the on/off switch, on by default; `[graph]
    /// advisors` is the proposal count. Everything here is best-effort and
    /// non-fatal to the run: a misconfigured `[roles] advisors`, a roster
    /// that cannot reach quota, or a synthesis seat that produced nothing
    /// usable all leave `implement` exactly as it was before this stage
    /// existed — the task instruction alone — rather than failing the whole
    /// competition over an enrichment stage. Every outcome is still recorded
    /// as an event, so a run that got nothing from this stage says why.
    ///
    /// [`RunState::advise_attempted`] is this node's idempotency marker, the
    /// same role [`RunState::judge_skipped`] plays for `judge`: without it a
    /// resumed run whose stage failed would re-run it, and re-spend the
    /// agent calls, on every reentry before `implement`.
    ///
    /// Also skipped once any candidate shows implementation progress — the
    /// exact predicate `implement` itself uses to decide a candidate is no
    /// longer "todo" (see its own `todo` filter). `advise_attempted` alone
    /// is not enough: a run created by an older binary that predates this
    /// field deserializes it as `false` (`#[serde(default)]`), so resuming
    /// an already-`Implementing`-or-later run under this build would
    /// otherwise walk straight back through `prep` (a no-op once candidates
    /// exist) into this node and spawn every advisor seat against worktrees
    /// `prep` never recreated — after implementation has already started,
    /// which is exactly the invariant this stage exists to guarantee.
    async fn advise(&mut self) -> Result<()> {
        let implement_untouched = self
            .state
            .candidates
            .iter()
            .all(|c| c.commits == 0 && c.failed.is_none() && !c.empty);
        if !self.state.config.graph.advise || self.state.advise_attempted {
            return Ok(());
        }
        if !implement_untouched {
            self.state.event(
                "advise",
                "skipping the design-deliberation stage: at least one \
                 candidate already shows implementation progress, so this \
                 run is past the point the stage exists to run before"
                    .to_owned(),
            );
            self.state.advise_attempted = true;
            self.state.save()?;
            return Ok(());
        }
        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        let instruction = self.state.instruction.clone();
        let language = self.state.config.graph.language.clone();
        let root = self.state.worktree_root();
        let n = self.state.config.graph.advisors;
        let where_recorded = self.state.dir().join("run.json");

        let seats = match self.state.config.advisors() {
            Ok(seats) if !seats.is_empty() => seats,
            Ok(_) => {
                self.state.event(
                    "advise",
                    format!(
                        "[graph] advisors is 0; skipping the design-deliberation \
                         stage and continuing without a synthesis brief (see {})",
                        where_recorded.display()
                    ),
                );
                self.state.advise_attempted = true;
                self.state.save()?;
                return Ok(());
            }
            Err(e) => {
                self.state.event(
                    "advise",
                    format!(
                        "could not resolve advisor seats ({e:#}); continuing \
                         without a design-deliberation brief (see {})",
                        where_recorded.display()
                    ),
                );
                self.state.advise_attempted = true;
                self.state.save()?;
                return Ok(());
            }
        };

        let timeout = Duration::from_secs(self.state.config.graph.timeout_judge.max(1));
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let worktrees: Vec<PathBuf> = (1..=n).map(|k| root.join(format!("advisor-{k}"))).collect();

        let mut jobs = Vec::new();
        for (i, spec) in seats.iter().cloned().enumerate() {
            let seat_key = format!("advisor-{}", i + 1);
            let seat = self.seat(&seat_key, &spec.id);
            jobs.push(SeatJob {
                prompt: prompt::advisor(&instruction, i + 1, seats.len(), &language),
                spec,
                seat,
                cwd: worktrees[i % worktrees.len()].clone(),
                timeout,
                allow_write: false,
                sessions: false,
                artifacts: artifacts.clone(),
                stem: seat_key,
            });
        }

        self.state.event(
            "advise",
            format!(
                "{} advisor seat(s) sketching a design in parallel",
                jobs.len()
            ),
        );
        let mut quota_losses = Vec::new();
        let cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "advise",
            prompts: &prompts,
            cache: cache.as_deref(),
            round: None,
        };
        let results = ask_json_wave::<Proposal>(
            jobs,
            Arc::clone(&self.sem),
            self.state.config.graph.retries,
            &ctx,
            &mut quota_losses,
            &mut self.state,
            &|p: &Proposal| p.validate(),
        )
        .await;
        self.state.quota.extend(quota_losses);

        let mut records = Vec::with_capacity(results.len());
        for (i, (seat, res)) in results.into_iter().enumerate() {
            let agent_id = seat.agent.clone();
            self.state.seats.insert(seat.key.clone(), seat);
            match res {
                Ok((proposal, out)) => {
                    self.state
                        .event("advise", format!("advisor-{} proposed a design", i + 1));
                    records.push(advise::AdvisorRecord::proposed(
                        i + 1,
                        agent_id,
                        proposal,
                        out.duration_ms,
                    ));
                }
                Err(e) => {
                    self.state.event(
                        "advise",
                        format!("advisor-{} produced no usable proposal: {e:#}", i + 1),
                    );
                    records.push(advise::AdvisorRecord::failed(
                        i + 1,
                        agent_id,
                        e.to_string(),
                    ));
                }
            }
        }

        let mut advice = advise::Advice {
            records,
            synthesis: None,
        };
        if advice.proposals().is_empty() {
            self.state.event(
                "advise",
                "no advisor produced a usable proposal; continuing without a \
                 synthesis brief"
                    .to_owned(),
            );
        } else {
            match self
                .synthesize_brief(
                    &advice,
                    &instruction,
                    &language,
                    &worktrees[0],
                    &artifacts,
                    &run_id,
                    &prompts,
                    cache.as_deref(),
                )
                .await
            {
                Ok(Some(text)) => {
                    self.state.event(
                        "advise",
                        "synthesized a design brief for the implementer".to_owned(),
                    );
                    advice.synthesis = Some(text);
                }
                Ok(None) => {
                    self.state.event(
                        "advise",
                        "the synthesis seat produced nothing usable; continuing \
                         without a design brief"
                            .to_owned(),
                    );
                }
                Err(e) => {
                    self.state.event(
                        "advise",
                        format!("could not synthesize a design brief: {e:#}"),
                    );
                }
            }
        }
        advise::apply_reflection(&mut advice);

        self.state.advice = Some(advice);
        self.state.advise_attempted = true;
        self.state.save()?;
        Ok(())
    }

    /// The synthesis seat: reads every advisor's proposal and blends them
    /// into the design brief `advise` stores on [`RunState::advice`]. Split
    /// out of [`Runner::advise`] only for readability — it is not called
    /// anywhere else.
    ///
    /// Picked the same way [`crate::talk`]'s standing conversation and
    /// [`crate::bump`]'s release-bump decision are: [`agent::pick`], with
    /// `[roles] synthesizer` checked first and [`agent::pick`]'s own default
    /// order (a claude seat, else the first runnable agent in roster order)
    /// used when that field is unset — see `[roles] synthesizer`'s own doc
    /// in [`crate::config`] for why a dedicated field exists here at all.
    #[allow(clippy::too_many_arguments)]
    async fn synthesize_brief(
        &mut self,
        advice: &advise::Advice,
        instruction: &str,
        language: &str,
        cwd: &Path,
        artifacts: &Path,
        run_id: &str,
        prompts: &Prompts,
        cache: Option<&Path>,
    ) -> Result<Option<String>> {
        let want = self.state.config.roles.synthesizer.as_deref();
        let spec = agent::pick(&self.state.config.agents, want, &agent::installed)?;
        let mut seat = self.seat("advise-synthesis", &spec.id);
        let proposals = advice.proposals();
        let mut prompt = prompt::with_overlay(
            prompt::synthesize_brief(instruction, &proposals, language),
            prompts.overlay("advise"),
        );
        if cache.is_some() {
            // This seat never writes, so it is never handed `CARGO_TARGET_DIR`
            // below — see `prompt::build_cache_note`'s doc for why telling a
            // read-only seat to build through the shared cache is exactly how
            // a sandbox's write refusal gets misread as a defect.
            prompt.push('\n');
            prompt.push_str(&prompt::build_cache_note("advise", false));
        }
        let timeout = Duration::from_secs(self.state.config.graph.timeout_judge.max(1));
        let out = agent::invoke(
            &spec,
            &mut seat,
            &Invocation {
                cwd,
                prompt: &prompt,
                timeout,
                allow_write: false,
                sessions: false,
                artifacts,
                stem: "advise-synthesis",
                run: run_id,
                node: "advise",
                cache_dir: None,
                attachments: &[],
            },
        )
        .await?;
        self.state.seats.insert(seat.key.clone(), seat);
        if !out.usable() {
            return Ok(None);
        }
        let text =
            verdict::section(&out.text, "synthesis").unwrap_or_else(|| out.text.trim().to_owned());
        Ok((!text.trim().is_empty()).then_some(text))
    }

    // ----------------------------------------------------------- implement

    async fn implement(&mut self) -> Result<()> {
        // Attribution for every agent this node spawns: `MAGI_RUN` lets a task the
        // agent files with `magi task add` name the run that paid for it. The
        // prompt overlay is cloned alongside it because the waves borrow it
        // while `self` is mutably borrowed by the node's own bookkeeping.
        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        let todo: Vec<usize> = self
            .state
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.commits == 0 && c.failed.is_none() && !c.empty)
            .map(|(i, _)| i)
            .collect();
        if todo.is_empty() {
            return self.after_implement();
        }
        self.state.status = RunStatus::Implementing;

        let language = self.state.config.graph.language.clone();
        let timeout = Duration::from_secs(self.state.config.graph.timeout_implement);
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        // The design-deliberation stage's blended brief, when `advise` found
        // one — carried into every implementer's prompt the same way
        // regardless of which candidate it is.
        let brief = self
            .state
            .advice
            .as_ref()
            .and_then(|a| a.synthesis.as_deref())
            .map(str::to_owned);

        let mut jobs = Vec::new();
        for &i in &todo {
            let (index, label, worktree) = {
                let c = &self.state.candidates[i];
                (c.index, c.label, c.worktree.clone())
            };
            let spec = self.roles.implementers[index].clone();
            let seat_key = format!("impl-{label}");
            let seat = self.seat(&seat_key, &spec.id);
            let instruction = self.state.instruction.clone();
            jobs.push(SeatJob {
                spec,
                seat,
                prompt: prompt::implement(
                    &instruction,
                    &worktree.to_string_lossy(),
                    &language,
                    brief.as_deref(),
                ),
                cwd: worktree,
                timeout,
                allow_write: true,
                sessions,
                artifacts: artifacts.clone(),
                stem: format!("impl-{label}"),
            });
        }

        self.state.event(
            "implement",
            format!("{} candidates in parallel", jobs.len()),
        );
        // Kept so a seat whose CLI hung up can be asked again from the same
        // job: `wave` consumes what it is given. Mutable so `resume_quota_losses`
        // can update a seat's own entry once a fallback agent takes it over —
        // `resume_unconfirmed_commands`, which reads `sent` afterward, must see
        // whichever agent actually answered, not the one that quota'd out.
        let mut sent = jobs.clone();
        let cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "implement",
            prompts: &prompts,
            cache: cache.as_deref(),
            round: None,
        };
        let mut results = wave(jobs, Arc::clone(&self.sem), &ctx, &mut self.state, 0).await;
        self.resume_undelivered(&mut results, &sent, &prompts, &run_id)
            .await;
        self.resume_quota_losses(&mut results, &mut sent, &prompts, &run_id)
            .await;
        self.resume_unconfirmed_commands(&mut results, &sent, &prompts, &run_id)
            .await;

        for (&i, (_wi, seat, out)) in todo.iter().zip(results) {
            let seat_key = seat.key.clone();
            // A quota fallback (`resume_quota_losses`) may have handed this
            // seat to a different agent than the one `prep` recorded on the
            // candidate; the stats tables and any later fixer-defaults-to-
            // winner's-author lookup must credit whoever actually answered —
            // unless every fallback also quota'd out, in which case nobody
            // actually answered and crediting the last agent tried would
            // erase every earlier agent's own quota loss from the stats
            // tables instead of just this one seat's.
            let agent = seat.agent.clone();
            let exhausted_the_fallback_chain = matches!(&out, AgentOutcome::Quota(_));
            self.state.seats.insert(seat.key.clone(), seat);
            let label = self.state.candidates[i].label;
            let worktree = self.state.candidates[i].worktree.clone();
            let base = self.state.base_commit.clone();

            let (summary, duration, failed, verified_claim) = match out {
                AgentOutcome::Ok(o) => {
                    let text = verdict::section(&o.text, "summary").unwrap_or(o.text.clone());
                    let failed = (!o.usable()).then(|| {
                        if o.timed_out {
                            "agent timed out".to_owned()
                        } else {
                            format!("agent exited with {:?}", o.exit_code)
                        }
                    });
                    let verified_claim = verified_noop_claim(failed.is_none(), &o.commands, &text);
                    (text, o.duration_ms, failed, verified_claim)
                }
                // Left un-resumed by `resume_undelivered` (a dirty tree
                // already rescues the work, or there was no session left to
                // resume into) — reported like the ordinary failure it is,
                // never as if `o.text` (the CLI's raw error JSON) were an
                // answer.
                AgentOutcome::Dropped(o) => {
                    let why = o
                        .dropped
                        .as_ref()
                        .map(|d| d.why.as_str())
                        .unwrap_or("the CLI ended the stream without delivering its answer");
                    (
                        String::new(),
                        o.duration_ms,
                        Some(format!("the CLI dropped the stream ({why})")),
                        None,
                    )
                }
                AgentOutcome::Quota(o) => {
                    self.state.quota.push(QuotaLoss {
                        seat: seat_key,
                        node: "implement".to_owned(),
                        at: Timestamp::now(),
                        reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                    });
                    (
                        String::new(),
                        o.duration_ms,
                        Some("rate limited (quota); produced no change".to_owned()),
                        None,
                    )
                }
                AgentOutcome::Failed(e) => (String::new(), 0, Some(e), None),
            };

            // Rescue anything the agent edited but never committed: an
            // uncommitted candidate would silently be an empty one.
            let rescued = git::commit_all(
                &worktree,
                &format!("magi: candidate {label} (uncommitted work)"),
            )
            .await
            .unwrap_or(false);
            let commits = git::commits_ahead(&worktree, &base, "HEAD")
                .await
                .unwrap_or(0);
            let patch = git::diff(&worktree, &base, "HEAD")
                .await
                .unwrap_or_default();
            let stat = git::diff_stat(&worktree, &base, "HEAD")
                .await
                .unwrap_or_default();
            let files = git::changed_files(&worktree, &base, "HEAD")
                .await
                .map(|f| f.len())
                .unwrap_or(0);
            write_artifact(&self.state, &format!("cand-{label}.patch"), &patch)?;

            let c = &mut self.state.candidates[i];
            if !exhausted_the_fallback_chain {
                c.agent = agent;
            }
            c.summary = blind::sanitize_prose(&summary, &self.state.config.blind);
            c.stat = stat;
            c.files = files;
            c.commits = commits;
            c.duration_ms = duration;
            c.empty = commits == 0 || patch.trim().is_empty();
            // An agent that failed but still produced a committed change stays
            // in the running: the patch is what gets judged, not the exit code.
            c.failed = match failed {
                Some(_) if c.empty => failed,
                _ => None,
            };
            // Only an empty candidate can be a verified no-op: a claim next
            // to a real patch is not what the marker is for, and `c.failed`
            // being `Some` here already implies `verified_claim` was never
            // set (see the guard above the match that produced it).
            c.verified_noop = if c.empty { verified_claim } else { None };
            let note = match (&c.failed, c.empty, &c.verified_noop, rescued) {
                (Some(e), _, _, _) => format!("candidate {label}: {e}"),
                (None, true, Some(_), _) => {
                    format!("candidate {label}: no change produced (agent-verified no-op)")
                }
                (None, true, None, _) => format!("candidate {label}: no change produced"),
                (None, false, _, true) => {
                    format!(
                        "candidate {label}: {files} files, {commits} commits (rescued an uncommitted tree)"
                    )
                }
                (None, false, _, false) => {
                    format!("candidate {label}: {files} files, {commits} commits")
                }
            };
            self.state.event("implement", note);
            self.state.save()?;
        }

        self.after_implement()
    }

    /// Ask again, once, for work a CLI did and then failed to hand over.
    ///
    /// [`agent::dropped_stream`] recognises the one shape observed: an error
    /// status with an empty response and a usage report showing output tokens,
    /// i.e. **billed work with nothing delivered**. Run 26c7's candidate B was
    /// seven minutes and 14,267 output tokens that arrived as an empty
    /// candidate, because `agy`'s own subscriber fell behind and hung up.
    ///
    /// Two conditions, and both matter:
    ///
    /// - **Only when the tree is untouched.** Often the agent has already
    ///   written its files and only the closing message was lost; the rescue
    ///   commit below picks that up and there is nothing to ask for. Re-asking
    ///   then would pay for a second implementation of work already on disk.
    /// - **Once.** A CLI that drops one stream can drop the next, and this
    ///   node is the most expensive in the graph.
    ///
    /// The re-ask is a resume, not a re-run: `has_context` is true because the
    /// dropped reply still carried its `conversation_id`, so the seat is asked
    /// to finish what it was doing rather than sent the whole task again. It
    /// therefore gets a nudge's budget ([`retry_budget`]) - a quarter of the
    /// node's - for the same reason a re-ranked judge does: restating finished
    /// work is not the work.
    ///
    /// Unlike a quota this is worth retrying at all: a rate limit fails the
    /// same way until it resets, while an abandoned conversation is still
    /// there to be picked up.
    async fn resume_undelivered(
        &mut self,
        results: &mut [(usize, SeatState, AgentOutcome)],
        sent: &[SeatJob],
        prompts: &Prompts,
        run_id: &str,
    ) {
        for (wi, seat, out) in results.iter_mut() {
            let Some(dropped) = (match &*out {
                AgentOutcome::Dropped(o) => o.dropped.clone(),
                _ => None,
            }) else {
                continue;
            };
            let Some(job) = sent.get(*wi) else { continue };
            // Already on disk? Then only the closing message was lost.
            if !git::is_clean(&job.cwd).await.unwrap_or(true) {
                self.state.event(
                    "implement",
                    format!(
                        "{}: the CLI dropped the stream after {} output tokens ({}), but the \
                         work is in the tree",
                        seat.key, dropped.output_tokens, dropped.why
                    ),
                );
                continue;
            }
            // The re-ask only makes sense as a resume: `resume_after_drop`
            // says nothing about the task, trusting the seat to still hold it.
            // Without a session to resume — sessions disabled, or this CLI's
            // drop shape happened not to carry a session id — that prompt
            // would open a brand-new conversation with no context at all,
            // which is worse than leaving this as the ordinary failure it
            // already is.
            if !has_context(&job.spec, seat, job.sessions) {
                self.state.event(
                    "implement",
                    format!(
                        "{}: the CLI dropped the stream after {} output tokens ({}), but there \
                         is no session left to resume",
                        seat.key, dropped.output_tokens, dropped.why
                    ),
                );
                continue;
            }
            self.state.event(
                "implement",
                format!(
                    "{}: the CLI dropped the stream after {} output tokens ({}); resuming the \
                     conversation",
                    seat.key, dropped.output_tokens, dropped.why
                ),
            );
            let mut retry = job.clone();
            retry.seat = seat.clone();
            retry.prompt = prompt::resume_after_drop(&dropped.why);
            retry.timeout = retry_budget(job.timeout, true);
            retry.stem = format!("{}-resume", job.stem);
            let cache = self.state.config.cache_dir();
            let ctx = WaveCtx {
                run: run_id,
                node: "implement",
                prompts,
                cache: cache.as_deref(),
                round: None,
            };
            let (resumed_seat, resumed) =
                run_one(retry, Arc::clone(&self.sem), &ctx, &mut self.state, 1).await;
            *seat = resumed_seat;
            *out = resumed;
        }
    }

    /// Fall an implement seat through to the next untried agent in the
    /// implementer roster when it lost to quota, instead of leaving the
    /// seat's loss final the moment one agent's account runs dry.
    ///
    /// Solo runs (`graph.candidates = 1`, `daemon::apply_solo`'s forced shape)
    /// are the motivating case: `Config::resolve_roles`'s `implementers`
    /// truncates to the single slot rotation picked, so a solo task whose one
    /// implementer hits quota mid-run used to have nothing else to try. This
    /// walks [`ResolvedRoles::implementer_roster`] instead — the untruncated,
    /// unrotated roster — which is the only place the *other* candidates in
    /// the machine's roster still exist once `implementers` has been cut down
    /// to size.
    ///
    /// Walks forward from just past the seat's own original position in the
    /// roster, never wrapping back to the front: a later candidate slot (say
    /// `beta`, the roster's second entry) must fall through to the *next*
    /// entry (`gamma`) on its own quota loss, not back to `alpha`, which is
    /// almost certainly a different candidate's own agent already — and once
    /// the roster's tail is exhausted there is nothing left to fall through
    /// to for *this* seat, wrapping or not. Tried by `spec.id`, never the
    /// whole [`AgentSpec`]: a roster with the same id named twice must not
    /// let this retry that id forever. The loop keeps falling through until
    /// an attempt lands something other than `Quota` or the roster's tail
    /// runs out of untried ids, at which point the seat is left exactly as
    /// `implement`'s own `AgentOutcome::Quota` arm already handles it: one
    /// `QuotaLoss` recorded, the candidate failed/empty.
    ///
    /// `sent` is taken mutably and updated with the fallback agent's spec:
    /// `resume_unconfirmed_commands`, which runs after this and also reads
    /// `sent`, must see whichever agent actually ended up answering the seat
    /// — reading the stale, original spec there would check session
    /// eligibility against the wrong CLI and could hand a fallback agent's
    /// session id to the agent that just lost the seat to quota.
    ///
    /// Every fallback gets a fresh [`SeatState`], never the quota'd seat's own
    /// — `self.seat` only reuses state when the agent id is unchanged, so
    /// handing it a different id already gets this for free. Reusing the old
    /// seat would resume a different CLI's session as if it were a
    /// continuation of this one.
    ///
    /// Unlike [`Runner::resume_undelivered`], not gated on a clean worktree:
    /// a quota loss cuts an agent off mid-turn, so anything already in the
    /// tree is unfinished work, not a completed candidate a re-ask would pay
    /// for twice. A dirty tree is rescued into a commit first (the same
    /// neutral-identity rescue `implement`'s own outcome loop gives every
    /// candidate) so the next agent starts clean.
    ///
    /// The new agent gets the implementer's full prompt and full
    /// `timeout_implement` budget, not `resume_after_drop`'s nudge-sized one:
    /// it has no session and no context, and is implementing the task from
    /// nothing, unlike a resumed drop which is only restating work already
    /// done.
    ///
    /// Every intermediate `Quota` this loop absorbs is folded into a plain
    /// `implement` event, never into `self.state.quota` — that is what
    /// `daemon.rs`'s own backoff reads to decide a run's task attempt should
    /// go unspent, and a seat that ultimately recovered on its second or
    /// third agent is not the stalled panel that check exists to catch. Only
    /// the final, unrecovered `Quota` (once the roster runs out) ever reaches
    /// `self.state.quota`, via the ordinary `AgentOutcome::Quota` arm the
    /// outcome loop already has — this helper never pushes to it itself.
    async fn resume_quota_losses(
        &mut self,
        results: &mut [(usize, SeatState, AgentOutcome)],
        sent: &mut [SeatJob],
        prompts: &Prompts,
        run_id: &str,
    ) {
        let instruction = self.state.instruction.clone();
        let language = self.state.config.graph.language.clone();
        let brief = self
            .state
            .advice
            .as_ref()
            .and_then(|a| a.synthesis.as_deref())
            .map(str::to_owned);
        for (wi, seat, out) in results.iter_mut() {
            let Some(job) = sent.get_mut(*wi) else {
                continue;
            };
            // Where the seat's own original agent sits in the roster — the
            // fallback walk starts just past here, never at the front, so a
            // later candidate slot's quota loss does not fall back onto an
            // earlier slot's own agent.
            let start = self
                .roles
                .implementer_roster
                .iter()
                .position(|s| s.id == job.spec.id)
                .unwrap_or(0);
            let mut tried: BTreeSet<String> = BTreeSet::from([job.spec.id.clone()]);
            let mut fallback_attempt = 0usize;
            while matches!(&*out, AgentOutcome::Quota(_)) {
                let Some(next) =
                    next_untried_implementer(&self.roles.implementer_roster, start, &tried)
                        .cloned()
                else {
                    break;
                };
                tried.insert(next.id.clone());
                fallback_attempt += 1;

                git::commit_all(
                    &job.cwd,
                    &format!(
                        "magi: candidate {} (uncommitted work before quota fallback)",
                        seat.key
                    ),
                )
                .await
                .ok();

                self.state.event(
                    "implement",
                    format!(
                        "{}: rate limited (quota) on {}; retrying with {}",
                        seat.key, seat.agent, next.id
                    ),
                );

                let new_seat = self.seat(&seat.key, &next.id);
                // Kept in sync on `sent` itself, not just the local retry: a
                // later helper (`resume_unconfirmed_commands`) reads `sent`
                // after this one returns and must see whichever agent is now
                // occupying the seat, not the one that just quota'd out —
                // otherwise it would judge session/continuation eligibility
                // by the wrong CLI and could resend a fallback's session id
                // to the agent that lost it the seat in the first place.
                job.spec = next.clone();
                let mut retry = job.clone();
                retry.seat = new_seat;
                retry.prompt = prompt::implement(
                    &instruction,
                    &job.cwd.to_string_lossy(),
                    &language,
                    brief.as_deref(),
                );
                retry.stem = format!("{}-quota-{}", job.stem, next.id);
                let cache = self.state.config.cache_dir();
                let ctx = WaveCtx {
                    run: run_id,
                    node: "implement",
                    prompts,
                    cache: cache.as_deref(),
                    round: None,
                };
                let (fallback_seat, fallback_out) = run_one(
                    retry,
                    Arc::clone(&self.sem),
                    &ctx,
                    &mut self.state,
                    fallback_attempt,
                )
                .await;
                *seat = fallback_seat;
                *out = fallback_out;
            }
        }
    }

    /// Ask an implement seat's own CLI to confirm what it started, once, when
    /// its reply reported a command whose completion status it never
    /// confirmed — see [`has_unconfirmed_command`]'s own doc for exactly what
    /// that does and does not mean.
    ///
    /// The completion contract this task asks for, extended to `implement`
    /// with the same signal `continue_fix_report` reads for the fixer,
    /// rather than a keyword search over the reply or a hard requirement on
    /// `## SUMMARY`'s presence — the shape behind fb35, 9566 and e185, where
    /// a candidate's CLI turn ended cleanly while a test run it had started
    /// had not. A short, ordinary reply with no `## SUMMARY` and no commands
    /// named in it at all is untouched by this: `commands` is empty, so
    /// there is nothing to be unconfirmed.
    ///
    /// Unlike `resume_undelivered`, not gated on the tree being untouched:
    /// this is not about recovering edits that might already be on disk, it
    /// is about a result the seat itself never vouched for, which resuming
    /// asks for regardless of what the tree already holds. Bounded to one
    /// attempt for the same reason `resume_undelivered` is — this is the
    /// most expensive node in the graph — and a seat that still cannot
    /// confirm on that attempt is left as whatever its (possibly still
    /// unconfirmed) reply says; this does not invent a new "failed" reason
    /// for a candidate that otherwise produced a real, committed change.
    async fn resume_unconfirmed_commands(
        &mut self,
        results: &mut [(usize, SeatState, AgentOutcome)],
        sent: &[SeatJob],
        prompts: &Prompts,
        run_id: &str,
    ) {
        for (wi, seat, out) in results.iter_mut() {
            let AgentOutcome::Ok(o) = &*out else {
                continue;
            };
            if !has_unconfirmed_command(&o.commands) {
                continue;
            }
            let Some(job) = sent.get(*wi) else { continue };
            if !has_context(&job.spec, seat, job.sessions) {
                self.state.event(
                    "implement",
                    format!(
                        "{}: the reply named a command whose own CLI never confirmed the exit \
                         status of, but there is no session left to resume",
                        seat.key
                    ),
                );
                continue;
            }
            self.state.event(
                "implement",
                format!(
                    "{}: the reply named a command whose own CLI never confirmed the exit \
                     status of; resuming the conversation",
                    seat.key
                ),
            );
            let mut retry = job.clone();
            retry.seat = seat.clone();
            retry.prompt = prompt::resume_incomplete(
                "a command in your last reply had no confirmed exit status",
            );
            retry.timeout = retry_budget(job.timeout, true);
            retry.stem = format!("{}-confirm", job.stem);
            let cache = self.state.config.cache_dir();
            let ctx = WaveCtx {
                run: run_id,
                node: "implement",
                prompts,
                cache: cache.as_deref(),
                round: None,
            };
            let (resumed_seat, resumed) =
                run_one(retry, Arc::clone(&self.sem), &ctx, &mut self.state, 1).await;
            *seat = resumed_seat;
            *out = resumed;
        }
    }

    /// Ask the fixer's own seat again, up to [`MAX_FIX_CONTINUATIONS`] times,
    /// when its CLI turn ended cleanly (`AgentOutcome::Ok`) but the reply held
    /// no [`FixReport`] — see [`MAX_FIX_CONTINUATIONS`]'s own doc for the run
    /// that motivated this.
    ///
    /// Not the same gap as an unparsable *shape*, which [`ask_json_wave`]'s
    /// own nudge loop already covers for judge/review/vote seats, and not a
    /// dropped stream, which [`Runner::resume_undelivered`] covers for
    /// implement seats: here the CLI turn genuinely finished while the node's
    /// own work — the fixer's account of what it did — had not. Gated purely
    /// on `extract_json::<FixReport>` having failed on an otherwise-usable
    /// reply, never on any wording in it, so a fixer whose valid, first-try
    /// `FixReport` happens to mention having waited on a background test is
    /// never resumed — the `Ok(report)` branch at the call site returns
    /// before this is ever invoked.
    ///
    /// Same discipline as `resume_undelivered`: a nudge-sized timeout per
    /// attempt ([`retry_budget`]), nothing attempted once the session is
    /// gone, and a quota hit ends the loop immediately rather than retrying a
    /// rate limit that fails the same way again.
    async fn continue_fix_report(
        &mut self,
        mut seat: SeatState,
        parse_err: String,
        job: &SeatJob,
        prompts: &Prompts,
        run_id: &str,
        round: usize,
    ) -> (
        SeatState,
        Option<FixReport>,
        Option<String>,
        ContinuationRecord,
    ) {
        let mut last_err = parse_err;
        let mut cumulative_wait_ms = 0u64;
        let mut attempts = 0usize;
        loop {
            if !has_context(&job.spec, &seat, job.sessions) {
                self.state.event(
                    "fix",
                    format!(
                        "round {round}: fixer's reply had no adoption report ({last_err}); no \
                         session left to resume into"
                    ),
                );
                let outcome = if attempts == 0 {
                    ContinuationOutcome::NoSession
                } else {
                    ContinuationOutcome::Exhausted
                };
                return (
                    seat,
                    None,
                    Some(format!("unparsable fix report: {last_err}")),
                    ContinuationRecord {
                        attempts,
                        cumulative_wait_ms,
                        outcome,
                    },
                );
            }
            if attempts >= MAX_FIX_CONTINUATIONS {
                self.state.event(
                    "fix",
                    format!(
                        "round {round}: fixer's reply still had no adoption report after \
                         {attempts} continuation(s) ({last_err}); giving up"
                    ),
                );
                return (
                    seat,
                    None,
                    Some(format!(
                        "unparsable fix report after {attempts} continuation(s): {last_err}"
                    )),
                    ContinuationRecord {
                        attempts,
                        cumulative_wait_ms,
                        outcome: ContinuationOutcome::Exhausted,
                    },
                );
            }
            attempts += 1;
            self.state.event(
                "fix",
                format!(
                    "round {round}: fixer's reply had no adoption report ({last_err}); resuming \
                     the conversation (attempt {attempts}/{MAX_FIX_CONTINUATIONS})"
                ),
            );
            let mut retry = job.clone();
            retry.seat = seat.clone();
            retry.prompt = prompt::resume_incomplete(&last_err);
            retry.timeout = retry_budget(job.timeout, true);
            retry.stem = format!("{}-continue{attempts}", job.stem);
            let cache = self.state.config.cache_dir();
            let ctx = WaveCtx {
                run: run_id,
                node: "fix",
                prompts,
                cache: cache.as_deref(),
                round: Some(round),
            };
            let (resumed_seat, resumed_out) = run_one(
                retry,
                Arc::clone(&self.sem),
                &ctx,
                &mut self.state,
                attempts,
            )
            .await;
            seat = resumed_seat;
            match resumed_out {
                AgentOutcome::Ok(o) => {
                    cumulative_wait_ms += o.duration_ms;
                    match verdict::extract_json::<FixReport>(&o.text) {
                        Ok(report) if !has_unconfirmed_command(&o.commands) => {
                            self.state.event(
                                "fix",
                                format!(
                                    "round {round}: fixer's adoption report recovered after \
                                     {attempts} continuation(s)"
                                ),
                            );
                            return (
                                seat,
                                Some(report),
                                None,
                                ContinuationRecord {
                                    attempts,
                                    cumulative_wait_ms,
                                    outcome: ContinuationOutcome::Resumed,
                                },
                            );
                        }
                        // The report parsed, but this same reply's own
                        // CommandEvidence — the identical record `state.jobs`
                        // renders — names a command whose CLI never
                        // confirmed an exit status. Read together, that is
                        // not a resolved answer: keep nudging rather than
                        // accept a report standing next to a command the
                        // seat's own CLI cannot vouch for.
                        Ok(_) => {
                            last_err = "the reply parsed, but it reported a command whose own CLI \
                                 never confirmed an exit status"
                                .to_owned();
                        }
                        Err(e) => last_err = e.to_string(),
                    }
                }
                AgentOutcome::Quota(o) => {
                    cumulative_wait_ms += o.duration_ms;
                    self.state.quota.push(QuotaLoss {
                        seat: seat.key.clone(),
                        node: "fix".to_owned(),
                        at: Timestamp::now(),
                        reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                    });
                    self.state.event(
                        "fix",
                        format!(
                            "round {round}: continuation rate limited (quota); not retrying now"
                        ),
                    );
                    return (
                        seat,
                        None,
                        Some("rate limited (quota) while recovering the fix report".to_owned()),
                        ContinuationRecord {
                            attempts,
                            cumulative_wait_ms,
                            outcome: ContinuationOutcome::QuotaLost,
                        },
                    );
                }
                AgentOutcome::Dropped(o) => {
                    cumulative_wait_ms += o.duration_ms;
                    let why = o
                        .dropped
                        .as_ref()
                        .map(|d| d.why.as_str())
                        .unwrap_or("the CLI ended the stream without delivering its answer");
                    last_err = format!("the CLI dropped the stream ({why})");
                }
                AgentOutcome::Failed(e) => last_err = e,
            }
        }
    }

    fn after_implement(&mut self) -> Result<()> {
        // Scan every candidate patch once the set is complete.
        if self.state.leaks.is_empty() {
            let cfg = self.state.config.blind.clone();
            let mut leaks = Vec::new();
            for c in &self.state.candidates {
                let Some(patch) =
                    crate::run::read_artifact(&self.state, &format!("cand-{}.patch", c.label))
                else {
                    continue;
                };
                leaks.extend(blind::scan(
                    &format!("candidate {} patch", c.label),
                    &patch,
                    &cfg.vendor_tokens,
                ));
            }
            if !leaks.is_empty() {
                let summary = leaks
                    .iter()
                    .map(|l| format!("{}×{} in {}", l.token, l.count, l.site))
                    .collect::<Vec<_>>()
                    .join(", ");
                match cfg.on_leak {
                    LeakPolicy::Fail => {
                        self.state.status = RunStatus::Failed;
                        self.state
                            .event("blind", format!("vendor text in a patch: {summary}"));
                        self.state.leaks = leaks;
                        self.state.save()?;
                        self.settle_questions();
                        bail!(
                            "blind.on_leak = \"fail\" and vendor text reached a \
                             judged patch: {summary}"
                        );
                    }
                    LeakPolicy::Redact => self.state.event(
                        "blind",
                        format!("redacting vendor text for judging: {summary}"),
                    ),
                    LeakPolicy::Warn => self.state.event(
                        "blind",
                        format!("vendor text present in a judged patch (shown as-is): {summary}"),
                    ),
                }
                self.state.leaks = leaks;
            }
        }

        if self.state.viable().is_empty() {
            if self.state.all_candidates_verified_noop() {
                // Every candidate agreed, with evidence the adoption guard
                // accepted, that nothing belongs in this worktree. That is
                // not the same fact as a candidate that simply failed to
                // write anything, and settling it as an ordinary `Failed`
                // (see `SCHEMA`'s doc for schema 10) is what let two of
                // task 391f's attempts burn a retry each re-discovering the
                // same already-landed fix. Terminal either way, so `judge`
                // must never run over an empty candidate set — unlike the
                // `Failed` branch below this returns `Ok`, not an error:
                // nothing here failed.
                self.state.status = RunStatus::VerifiedNoop;
                self.state.save()?;
                self.settle_questions();
                return Ok(());
            }
            self.state.status = RunStatus::Failed;
            self.state.save()?;
            self.settle_questions();
            bail!("no candidate produced a change; nothing to judge");
        }
        self.state.status = RunStatus::Judging;
        self.state.save()?;
        Ok(())
    }

    // --------------------------------------------------------------- judge

    async fn judge(&mut self) -> Result<()> {
        // Attribution for every agent this node spawns: `MAGI_RUN` lets a task the
        // agent files with `magi task add` name the run that paid for it. The
        // prompt overlay is cloned alongside it because the waves borrow it
        // while `self` is mutably borrowed by the node's own bookkeeping.
        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        if !self.state.judgements.is_empty() || self.state.judge_skipped {
            return Ok(());
        }
        let viable: Vec<Candidate> = self.state.viable().into_iter().cloned().collect();
        if viable.len() == 1 {
            // Recorded so this is a one-time event: `judgements` stays empty
            // either way, which without this flag is indistinguishable from
            // "not yet judged" on the next reentry — and status is left
            // untouched, so a later node's conclusion (e.g. `Blocked` after
            // the review budget ran out) survives a resume instead of being
            // clobbered back to `Judging` by this node running again.
            self.state.judge_skipped = true;
            self.state.event(
                "judge",
                format!(
                    "only candidate {} produced a change; judging skipped",
                    viable[0].label
                ),
            );
            self.state.save()?;
            return Ok(());
        }
        self.state.status = RunStatus::Judging;

        let labels: Vec<char> = viable.iter().map(|c| c.label).collect();
        let language = self.state.config.graph.language.clone();
        let timeout = Duration::from_secs(self.state.config.graph.timeout_judge);
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let root = self.state.worktree_root();
        let base_short = short(&self.state.base_commit);

        let mut jobs = Vec::new();
        let mut orders = Vec::new();
        for (j, spec) in self.roles.judges.clone().into_iter().enumerate() {
            let order = blind::presentation_order(viable.len(), j, self.state.seed);
            let views: Vec<CandidateView> = order.iter().map(|&k| self.view(&viable[k])).collect();
            orders.push(order.iter().map(|&k| viable[k].index).collect::<Vec<_>>());
            let seat_key = format!("judge-{}", j + 1);
            let seat = self.seat(&seat_key, &spec.id);
            jobs.push(SeatJob {
                prompt: prompt::judge(
                    &self.state.instruction,
                    &views,
                    self.roles.judges.len(),
                    &base_short,
                    &language,
                ),
                spec,
                seat,
                cwd: root.join(format!("judge-{}", j + 1)),
                timeout,
                allow_write: false,
                sessions,
                artifacts: artifacts.clone(),
                stem: format!("judge-{}", j + 1),
            });
        }

        self.state.event(
            "judge",
            format!(
                "{} judges ranking {} candidates blind",
                jobs.len(),
                viable.len()
            ),
        );
        let labels_for_check = labels.clone();
        let mut quota_losses = Vec::new();
        let cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "judge",
            prompts: &prompts,
            cache: cache.as_deref(),
            round: None,
        };
        let results = ask_json_wave::<Ranking>(
            jobs,
            Arc::clone(&self.sem),
            self.state.config.graph.retries,
            &ctx,
            &mut quota_losses,
            &mut self.state,
            &move |r: &Ranking| r.validate(&labels_for_check),
        )
        .await;
        self.state.quota.extend(quota_losses);

        for (j, (seat, res)) in results.into_iter().enumerate() {
            let agent_id = seat.agent.clone();
            self.state.seats.insert(seat.key.clone(), seat);
            let mut record = Judgement {
                judge: j + 1,
                seat: format!("judge-{}", j + 1),
                agent: agent_id,
                ranking: Vec::new(),
                reasons: BTreeMap::new(),
                confidence: None,
                order: orders[j].clone(),
                failed: None,
                duration_ms: 0,
            };
            match res {
                Ok((ranking, out)) => {
                    record.ranking = ranking.normalized();
                    record.reasons = ranking.reasons;
                    record.confidence = ranking.confidence;
                    record.duration_ms = out.duration_ms;
                    self.state.event(
                        "judge",
                        format!(
                            "judge {} ranked {}",
                            j + 1,
                            record.ranking.iter().collect::<String>()
                        ),
                    );
                }
                Err(e) => {
                    record.failed = Some(e.to_string());
                    self.state
                        .event("judge", format!("judge {} produced no ranking: {e}", j + 1));
                }
            }
            self.state.judgements.push(record);
            self.state.save()?;
        }
        Ok(())
    }

    // ---------------------------------------------------------- deliberate

    async fn deliberate(&mut self) -> Result<()> {
        // Attribution for every agent this node spawns: `MAGI_RUN` lets a task the
        // agent files with `magi task add` name the run that paid for it. The
        // prompt overlay is cloned alongside it because the waves borrow it
        // while `self` is mutably borrowed by the node's own bookkeeping.
        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        if !self.state.deliberation.is_empty() {
            return Ok(());
        }
        let tops: Vec<char> = self
            .state
            .judgements
            .iter()
            .filter_map(|j| j.ranking.first().copied())
            .collect();
        let rounds = self.state.config.graph.deliberate_rounds;
        if tops.len() < 2 || tops.iter().all(|t| *t == tops[0]) || rounds == 0 {
            if tops.len() >= 2 && tops.iter().all(|t| *t == tops[0]) {
                self.state.event(
                    "deliberate",
                    format!("judges agreed on {} outright; no deliberation", tops[0]),
                );
            }
            self.state.status = RunStatus::Voting;
            self.state.save()?;
            return Ok(());
        }

        self.state.status = RunStatus::Deliberating;
        self.state.event(
            "deliberate",
            format!(
                "split: first choices were {} — opening {rounds} round(s)",
                tops.iter().collect::<String>()
            ),
        );

        let viable: Vec<Candidate> = self.state.viable().into_iter().cloned().collect();
        let language = self.state.config.graph.language.clone();
        let timeout = Duration::from_secs(self.state.config.graph.timeout_judge);
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let root = self.state.worktree_root();
        let base_short = short(&self.state.base_commit);

        // Judges argue in sequence so that a turn can answer the one before it;
        // that is the difference between deliberation and three parallel
        // monologues.
        for round in 1..=rounds {
            let mut turns: Vec<DeliberationTurn> = Vec::new();
            for (j, spec) in self.roles.judges.clone().into_iter().enumerate() {
                if self.state.judgements[j].failed.is_some() {
                    continue;
                }
                let seat_key = format!("judge-{}", j + 1);
                let mut seat = self.seat(&seat_key, &spec.id);
                let transcript = self.transcript(&turns, j);
                let context = if has_context(&spec, &seat, sessions) {
                    None
                } else {
                    Some(self.candidate_block(&viable, &base_short))
                };
                let text = prompt::deliberate(
                    &self.state.instruction,
                    context.as_deref(),
                    &transcript,
                    round,
                    rounds,
                    &language,
                );
                let job = SeatJob {
                    spec,
                    seat: seat.clone(),
                    prompt: text,
                    cwd: root.join(format!("judge-{}", j + 1)),
                    timeout,
                    allow_write: false,
                    sessions,
                    artifacts: artifacts.clone(),
                    stem: format!("delib-{round}-judge-{}", j + 1),
                };
                let cache = self.state.config.cache_dir();
                let ctx = WaveCtx {
                    run: &run_id,
                    node: "deliberate",
                    prompts: &prompts,
                    cache: cache.as_deref(),
                    round: None,
                };
                let (updated, out) =
                    run_one(job, Arc::clone(&self.sem), &ctx, &mut self.state, 0).await;
                seat = updated;
                let agent_id = seat.agent.clone();
                let seat_key = seat.key.clone();
                self.state.seats.insert(seat.key.clone(), seat);
                let body = match out {
                    AgentOutcome::Ok(o) => verdict::section(&o.text, "position").unwrap_or(o.text),
                    // Never read the CLI's raw error JSON as this judge's
                    // position — skip the seat instead, the same as any other
                    // failed turn.
                    AgentOutcome::Dropped(o) => {
                        let why =
                            o.dropped.as_ref().map(|d| d.why.as_str()).unwrap_or(
                                "the CLI ended the stream without delivering its answer",
                            );
                        self.state.event(
                            "deliberate",
                            format!(
                                "judge {} skipped: the CLI dropped the stream ({why})",
                                j + 1
                            ),
                        );
                        continue;
                    }
                    AgentOutcome::Quota(o) => {
                        self.state.quota.push(QuotaLoss {
                            seat: seat_key,
                            node: "deliberate".to_owned(),
                            at: Timestamp::now(),
                            reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                        });
                        self.state.event(
                            "deliberate",
                            format!("judge {} skipped: rate limited (quota)", j + 1),
                        );
                        continue;
                    }
                    AgentOutcome::Failed(e) => {
                        self.state
                            .event("deliberate", format!("judge {} skipped: {e}", j + 1));
                        continue;
                    }
                };
                let tentative = verdict::extract_json::<Position>(&body)
                    .ok()
                    .and_then(|p| p.tentative)
                    .and_then(|s| s.trim().chars().next())
                    .map(|c| c.to_ascii_uppercase());
                self.state.event(
                    "deliberate",
                    format!(
                        "round {round}: judge {} now favours {}",
                        j + 1,
                        tentative.map_or("—".to_owned(), |c| c.to_string())
                    ),
                );
                turns.push(DeliberationTurn {
                    judge: j + 1,
                    agent: agent_id,
                    body: blind::sanitize_prose(&body, &self.state.config.blind),
                    tentative,
                });
            }
            self.state
                .deliberation
                .push(DeliberationRound { round, turns });
            self.state.save()?;
        }

        self.state.status = RunStatus::Voting;
        self.state.save()?;
        Ok(())
    }

    // ---------------------------------------------------------------- vote

    async fn vote(&mut self) -> Result<()> {
        // Attribution for every agent this node spawns: `MAGI_RUN` lets a task the
        // agent files with `magi task add` name the run that paid for it. The
        // prompt overlay is cloned alongside it because the waves borrow it
        // while `self` is mutably borrowed by the node's own bookkeeping.
        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        if !self.state.votes.is_empty() {
            return Ok(());
        }
        let viable: Vec<char> = self.state.viable().into_iter().map(|c| c.label).collect();
        if viable.len() == 1 {
            return Ok(());
        }
        self.state.status = RunStatus::Voting;

        let language = self.state.config.graph.language.clone();
        let timeout = Duration::from_secs(self.state.config.graph.timeout_judge);
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let root = self.state.worktree_root();
        let base_short = short(&self.state.base_commit);
        let candidates: Vec<Candidate> = self.state.viable().into_iter().cloned().collect();

        let mut jobs = Vec::new();
        let mut seats_at = Vec::new();
        for (j, spec) in self.roles.judges.clone().into_iter().enumerate() {
            if self
                .state
                .judgements
                .get(j)
                .is_some_and(|r| r.failed.is_some())
            {
                continue;
            }
            let seat_key = format!("judge-{}", j + 1);
            let seat = self.seat(&seat_key, &spec.id);
            let mut text = prompt::final_vote(&viable, &language);
            if !has_context(&spec, &seat, sessions) {
                text = format!(
                    "{}\n\n# Candidates\n\n{}",
                    text,
                    self.candidate_block(&candidates, &base_short)
                );
            }
            jobs.push(SeatJob {
                spec,
                seat,
                prompt: text,
                cwd: root.join(format!("judge-{}", j + 1)),
                timeout,
                allow_write: false,
                sessions,
                artifacts: artifacts.clone(),
                stem: format!("vote-judge-{}", j + 1),
            });
            seats_at.push(j);
        }

        self.state.event(
            "vote",
            format!(
                "collecting {} final votes one by one, privately",
                jobs.len()
            ),
        );
        let allowed = viable.clone();
        let mut quota_losses = Vec::new();
        let cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "vote",
            prompts: &prompts,
            cache: cache.as_deref(),
            round: None,
        };
        let results = ask_json_wave::<FinalVote>(
            jobs,
            Arc::clone(&self.sem),
            self.state.config.graph.retries,
            &ctx,
            &mut quota_losses,
            &mut self.state,
            &move |v: &FinalVote| match v.label() {
                Some(c) if allowed.contains(&c) => Ok(()),
                other => bail!("vote {other:?} is not one of {allowed:?}"),
            },
        )
        .await;
        self.state.quota.extend(quota_losses);

        for (&j, (seat, res)) in seats_at.iter().zip(results) {
            let agent_id = seat.agent.clone();
            self.state.seats.insert(seat.key.clone(), seat);
            let initial = self
                .state
                .judgements
                .get(j)
                .and_then(|r| r.ranking.first().copied());
            let mut record = VoteRecord {
                judge: j + 1,
                agent: agent_id,
                vote: None,
                reason: String::new(),
                changed: false,
            };
            match res {
                Ok((v, _)) => {
                    record.vote = v.label();
                    record.reason = blind::sanitize_prose(&v.reason, &self.state.config.blind);
                    record.changed = matches!((record.vote, initial), (Some(a), Some(b)) if a != b);
                    self.state.event(
                        "vote",
                        format!(
                            "judge {} voted {}{}",
                            j + 1,
                            record.vote.unwrap_or('?'),
                            if record.changed { " (changed)" } else { "" }
                        ),
                    );
                }
                Err(e) => {
                    self.state
                        .event("vote", format!("judge {} cast no vote: {e}", j + 1));
                }
            }
            self.state.votes.push(record);
            self.state.save()?;
        }
        Ok(())
    }

    // --------------------------------------------------------------- tally

    fn tally(&mut self) -> Result<()> {
        if self.state.tally.is_some() {
            return Ok(());
        }
        let viable: Vec<char> = self.state.viable().into_iter().map(|c| c.label).collect();
        let tops: Vec<char> = self
            .state
            .judgements
            .iter()
            .filter_map(|j| j.ranking.first().copied())
            .collect();
        let unanimous_initial = tops.len() > 1 && tops.iter().all(|t| *t == tops[0]);

        // A judge whose private vote failed still counted once, in the initial
        // ranking; using it beats discarding a whole seat.
        let mut first_choice: BTreeMap<char, usize> = viable.iter().map(|l| (*l, 0)).collect();
        let mut cast: Vec<char> = Vec::new();
        for (i, j) in self.state.judgements.iter().enumerate() {
            let vote = self
                .state
                .votes
                .iter()
                .find(|v| v.judge == i + 1)
                .and_then(|v| v.vote)
                .or_else(|| j.ranking.first().copied());
            if let Some(v) = vote {
                *first_choice.entry(v).or_insert(0) += 1;
                cast.push(v);
            }
        }

        let mut borda: BTreeMap<char, usize> = viable.iter().map(|l| (*l, 0)).collect();
        for j in &self.state.judgements {
            let n = j.ranking.len();
            for (pos, label) in j.ranking.iter().enumerate() {
                *borda.entry(*label).or_insert(0) += n.saturating_sub(pos + 1);
            }
        }

        let best = first_choice.values().copied().max().unwrap_or(0);
        let mut leaders: Vec<char> = first_choice
            .iter()
            .filter(|(_, v)| **v == best)
            .map(|(k, _)| *k)
            .collect();
        let mut tie_break = None;
        if leaders.len() > 1 {
            let top_borda = leaders.iter().map(|l| borda[l]).max().unwrap_or(0);
            let borda_leaders: Vec<char> = leaders
                .iter()
                .copied()
                .filter(|l| borda[l] == top_borda)
                .collect();
            tie_break = Some(if borda_leaders.len() == 1 {
                format!(
                    "{} way tie on first-choice votes, broken by Borda points from the initial rankings",
                    leaders.len()
                )
            } else {
                format!(
                    "{} way tie on both first-choice votes and Borda points, broken by label order",
                    leaders.len()
                )
            });
            leaders = borda_leaders;
            leaders.sort_unstable();
        }
        let winner = *leaders
            .first()
            .or(viable.first())
            .context("no candidate to declare a winner from")?;

        let changed_votes = self.state.votes.iter().filter(|v| v.changed).count();
        let unanimous_final = !cast.is_empty() && cast.iter().all(|c| *c == cast[0]);
        let deliberated = !self.state.deliberation.is_empty();

        // Whose verdict is this? A rate-limited seat is absent even if it
        // ranked before the limit hit, so presence is measured against the
        // recorded losses, not just "did a ranking ever appear".
        let quota_seats: std::collections::BTreeSet<&str> =
            self.state.quota.iter().map(|q| q.seat.as_str()).collect();
        let mut present = 0usize;
        for (i, j) in self.state.judgements.iter().enumerate() {
            if quota_seats.contains(j.seat.as_str()) {
                continue;
            }
            let ranked = !j.ranking.is_empty() && j.failed.is_none();
            let voted = self
                .state
                .votes
                .iter()
                .any(|v| v.judge == i + 1 && v.vote.is_some());
            if ranked || voted {
                present += 1;
            }
        }
        // Strict majority of the configured panel. A bare majority is real
        // signal we can act on, while a minority verdict must never stand in
        // for a healthy one. A one-candidate run needs no panel at all, and
        // `judges` stays `0` rather than the roster size a panel that never
        // sat would otherwise be credited with.
        let needs_quorum = viable.len() > 1;
        let judges_total = if needs_quorum {
            self.roles.judges.len()
        } else {
            0
        };
        let quorum = if needs_quorum {
            judges_total / 2 + 1
        } else {
            0
        };
        let met_quorum = !needs_quorum || present >= quorum;
        let uncontested = (!needs_quorum).then(|| {
            format!("only one candidate ({winner}) produced a usable change; no panel was asked")
        });

        self.state.event(
            "tally",
            match &uncontested {
                Some(reason) => format!("winner {winner} — {reason}"),
                None => format!(
                    "winner {winner} — votes {} | initial {} | {} changed | \
                     {present}/{judges_total} judges{}",
                    first_choice
                        .iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    if unanimous_initial {
                        "unanimous"
                    } else {
                        "split"
                    },
                    changed_votes,
                    if met_quorum {
                        String::new()
                    } else {
                        format!(" — below quorum ({quorum} required)")
                    },
                ),
            },
        );
        if !met_quorum {
            self.state.event(
                "stall",
                format!(
                    "verdict rests on {present} of {judges_total} judges (quorum {quorum}); \
                     the run stops here, resumable"
                ),
            );
        }
        self.state.tally = Some(Tally {
            first_choice,
            borda,
            winner,
            rankings: tops.len(),
            unanimous_initial,
            deliberated,
            changed_votes,
            unanimous_final,
            tie_break,
            judges: judges_total,
            present,
            quorum,
            met_quorum,
            uncontested,
        });
        self.state.status = if met_quorum {
            RunStatus::Reviewing
        } else {
            RunStatus::Stalled
        };
        self.state.save()?;
        Ok(())
    }

    // ------------------------------------------------------------- recover

    /// Re-ask the judge seats `tally` counts as absent, so a `Stalled` run can be
    /// resumed toward completion once the transient cause clears.
    ///
    /// A seat is absent — and therefore re-asked — when `tally` refuses to count
    /// it toward the quorum, which is exactly the set of seats whose absence
    /// collapsed the panel: struck by a rate limit at *any* node (the quorum must
    /// not depend on which node happened to hit the limit), or an ordinary
    /// failure (`failed = Some`) that never produced a usable ranking. A healthy
    /// seat is never disturbed.
    ///
    /// A seat that now answers with a usable ranking is "recovered": its
    /// `Judgement` is refreshed, its `QuotaLoss`/`failed` state cleared (so
    /// `tally` counts it present again), and its vote re-collected. A seat that
    /// still fails keeps its loss and stays absent.
    ///
    /// Returns `true` when the re-tally restores the quorum (the run may proceed
    /// to review/gate/merge), `false` when it is still below quorum (the run
    /// stays `Stalled`, still resumable for a later retry).
    #[allow(clippy::too_many_lines)]
    async fn recover_stall(&mut self) -> Result<bool> {
        // Attribution for every agent this node spawns: `MAGI_RUN` lets a task the
        // agent files with `magi task add` name the run that paid for it. The
        // prompt overlay is cloned alongside it because the waves borrow it
        // while `self` is mutably borrowed by the node's own bookkeeping.
        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        // Absent seats = quota-lost at any node, or failed outright. Mirroring
        // `tally`'s presence test (rather than the old quota-judge/vote filter)
        // is what keeps a non-quota collapse — or a quota loss recorded at the
        // deliberate node — from being a permanent dead-end on `--resume`.
        let quota_seats: BTreeSet<&str> =
            self.state.quota.iter().map(|q| q.seat.as_str()).collect();
        let absent: Vec<String> = self
            .state
            .judgements
            .iter()
            .filter(|j| quota_seats.contains(j.seat.as_str()) || j.failed.is_some())
            .map(|j| j.seat.clone())
            .collect();
        if absent.is_empty() {
            return Ok(false);
        }
        let viable: Vec<Candidate> = self.state.viable().into_iter().cloned().collect();
        if viable.len() <= 1 {
            return Ok(false);
        }
        let labels: Vec<char> = viable.iter().map(|c| c.label).collect();
        let language = self.state.config.graph.language.clone();
        let timeout = Duration::from_secs(self.state.config.graph.timeout_judge);
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let root = self.state.worktree_root();
        let base_short = short(&self.state.base_commit);
        let candidates: Vec<Candidate> = viable.clone();

        // Map each absent seat key to its 0-based position in `roles.judges`.
        let mut positions: Vec<usize> = absent
            .iter()
            .filter_map(|k| self.state.judgements.iter().position(|r| &r.seat == k))
            .collect();
        if positions.is_empty() {
            return Ok(false);
        }
        positions.sort_unstable();
        positions.dedup();

        // Re-rank the lost seats, one blind prompt each.
        let mut judge_jobs = Vec::new();
        for &j in &positions {
            let order = blind::presentation_order(viable.len(), j, self.state.seed);
            let views: Vec<CandidateView> = order.iter().map(|&k| self.view(&viable[k])).collect();
            let seat_key = format!("judge-{}", j + 1);
            let spec = self.roles.judges[j].clone();
            let seat = self.seat(&seat_key, &spec.id);
            judge_jobs.push(SeatJob {
                spec,
                seat,
                prompt: prompt::judge(
                    &self.state.instruction,
                    &views,
                    self.roles.judges.len(),
                    &base_short,
                    &language,
                ),
                cwd: root.join(seat_key),
                timeout,
                allow_write: false,
                sessions,
                artifacts: artifacts.clone(),
                stem: format!("judge-{}-recover", j + 1),
            });
        }

        let labels_for_check = labels.clone();
        let mut judge_losses = Vec::new();
        let retries = self.state.config.graph.retries;
        let cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "judge",
            prompts: &prompts,
            cache: cache.as_deref(),
            round: None,
        };
        let results = ask_json_wave::<Ranking>(
            judge_jobs,
            Arc::clone(&self.sem),
            retries,
            &ctx,
            &mut judge_losses,
            &mut self.state,
            &move |r: &Ranking| r.validate(&labels_for_check),
        )
        .await;

        // Refresh the judgement of every seat that ranked again.
        let mut recovered: BTreeSet<usize> = BTreeSet::new();
        for (&j, (seat, res)) in positions.iter().zip(results) {
            self.state.seats.insert(seat.key.clone(), seat);
            let record = &mut self.state.judgements[j];
            match res {
                Ok((ranking, out)) => {
                    record.ranking = ranking.normalized();
                    record.reasons = ranking.reasons;
                    record.confidence = ranking.confidence;
                    record.failed = None;
                    record.duration_ms = out.duration_ms;
                    recovered.insert(j);
                    self.state.event(
                        "recover",
                        format!("judge {} ranked again after the limit", j + 1),
                    );
                }
                Err(e) => {
                    self.state
                        .event("recover", format!("judge {} still cannot rank: {e}", j + 1));
                }
            }
        }

        // Re-ask the votes of the seats that recovered a ranking.
        let mut vote_jobs = Vec::new();
        let mut vote_pos: Vec<usize> = Vec::new();
        for &j in &recovered {
            let seat_key = format!("judge-{}", j + 1);
            let spec = self.roles.judges[j].clone();
            let seat = self.seat(&seat_key, &spec.id);
            let mut text = prompt::final_vote(&labels, &language);
            if !has_context(&spec, &seat, sessions) {
                text = format!(
                    "{}\n\n# Candidates\n\n{}",
                    text,
                    self.candidate_block(&candidates, &base_short)
                );
            }
            vote_jobs.push(SeatJob {
                spec,
                seat,
                prompt: text,
                cwd: root.join(seat_key),
                timeout,
                allow_write: false,
                sessions,
                artifacts: artifacts.clone(),
                stem: format!("vote-judge-{}-recover", j + 1),
            });
            vote_pos.push(j);
        }
        let allowed = labels.clone();
        let mut vote_losses = Vec::new();
        let vote_retries = self.state.config.graph.retries;
        let vote_cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "vote",
            prompts: &prompts,
            cache: vote_cache.as_deref(),
            round: None,
        };
        let votes = ask_json_wave::<FinalVote>(
            vote_jobs,
            Arc::clone(&self.sem),
            vote_retries,
            &ctx,
            &mut vote_losses,
            &mut self.state,
            &move |v: &FinalVote| match v.label() {
                Some(c) if allowed.contains(&c) => Ok(()),
                other => bail!("vote {other:?} is not one of {allowed:?}"),
            },
        )
        .await;
        for (&j, (seat, res)) in vote_pos.iter().zip(votes) {
            let agent_id = seat.agent.clone();
            self.state.seats.insert(seat.key.clone(), seat);
            match res {
                Ok((v, _)) => {
                    if let Some(rec) = self.state.votes.iter_mut().find(|r| r.judge == j + 1) {
                        rec.vote = v.label();
                        rec.reason = blind::sanitize_prose(&v.reason, &self.state.config.blind);
                    } else {
                        self.state.votes.push(VoteRecord {
                            judge: j + 1,
                            agent: agent_id,
                            vote: v.label(),
                            reason: blind::sanitize_prose(&v.reason, &self.state.config.blind),
                            changed: false,
                        });
                    }
                    self.state.event(
                        "recover",
                        format!("judge {} voted again after the limit", j + 1),
                    );
                }
                Err(e) => {
                    self.state
                        .event("recover", format!("judge {} still cannot vote: {e}", j + 1));
                }
            }
        }

        // A seat that ranked again is present even if its re-vote failed —
        // `tally` falls back to the initial ranking's first choice — so clear
        // its quota loss. Seats that still fail keep theirs and stay absent.
        if !recovered.is_empty() {
            let recovered_keys: BTreeSet<String> = recovered
                .iter()
                .map(|&j| format!("judge-{}", j + 1))
                .collect();
            self.state
                .quota
                .retain(|q| !recovered_keys.contains(&q.seat));
        }

        // Recompute the verdict from the refreshed panel.
        self.state.tally = None;
        self.tally()?;
        Ok(self
            .state
            .tally
            .as_ref()
            .map(|t| t.met_quorum)
            .unwrap_or(false))
    }

    // ----------------------------------------------------------------- fold

    async fn fold_losers(&mut self) -> Result<()> {
        let Some(winner) = self.state.tally.as_ref().map(|t| t.winner) else {
            return Ok(());
        };
        let repo = self.state.repo.clone();
        let mut folded = Vec::new();
        for i in 0..self.state.candidates.len() {
            let c = &self.state.candidates[i];
            if c.label == winner || c.folded {
                continue;
            }
            let (wt, branch, label) = (c.worktree.clone(), c.branch.clone(), c.label);
            git::worktree_remove(&repo, &wt).await.ok();
            git::branch_delete(&repo, &branch).await.ok();
            self.state.candidates[i].folded = true;
            folded.push(label.to_string());
        }
        // The judges are finished; their checkouts are pure cost from here.
        let root = self.state.worktree_root();
        for j in 1..=self.roles.judges.len() {
            let wt = root.join(format!("judge-{j}"));
            if wt.exists() {
                git::worktree_remove(&repo, &wt).await.ok();
            }
        }
        // The design-deliberation stage is finished by the time a tally
        // exists — same reasoning as the judges above.
        if self.state.config.graph.advise {
            for k in 1..=self.state.config.graph.advisors {
                let wt = root.join(format!("advisor-{k}"));
                if wt.exists() {
                    git::worktree_remove(&repo, &wt).await.ok();
                }
            }
        }
        if !folded.is_empty() {
            self.state
                .event("fold", format!("folded candidates {}", folded.join(", ")));
            self.state.save()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ base sync

    /// Land the winner's tree on the current tip of `<remote>/<base>` before
    /// anything verifies it.
    ///
    /// `verify.e2e`, `verify.gate` and every reviewer in [`Self::review_loop`]
    /// read whatever is checked out in the winner's worktree. Left alone that
    /// tree stays rooted at `base_commit` - the base as [`resolve_base`] saw
    /// it when the run *branched* - and a run takes long enough that the base
    /// has usually moved by the time it gets here. A gate that ran there
    /// answers "green on the commit this run started from", not "green on
    /// what is about to land", and the difference showed up three times in
    /// one day as a green run whose merge would have reverted a file another
    /// pull request had already landed.
    ///
    /// Reuses [`git::rebase_branch_in_temp`] rather than a second
    /// implementation of the same idea: `land::Step::Rebase` already worked
    /// out the rules - throwaway worktree, conflict stops and reports rather
    /// than feeding a fixer, nothing runs in the primary tree - and a second
    /// rebase path is exactly the kind of drift `resolve_base`'s own doc
    /// warns about ("two answers to a question nobody notices until a diff is
    /// wrong").
    ///
    /// Bounded by [`BASE_SYNC_ROUNDS`], counted in `state.base_sync.attempts`
    /// so it survives a park/resume. A conflict or a push failure sets
    /// `state.base_sync.conflict` and leaves the branch and worktree exactly
    /// as they were - untouched, for a person to look at - which is also what
    /// makes re-entering this function afterwards a no-op instead of a second
    /// attempt at the same wall.
    async fn sync_to_base(&mut self) -> Result<()> {
        if self
            .state
            .base_sync
            .as_ref()
            .is_some_and(|s| s.conflict.is_some())
        {
            return Ok(());
        }
        let Some(winner) = self.state.winner().cloned() else {
            return Ok(());
        };

        let repo = self.state.repo.clone();
        let remote = self.state.config.merge.remote.clone();
        let base_branch = self.state.base_branch.clone();
        let tracking = format!("{remote}/{base_branch}");

        git::fetch(&repo, &remote, &base_branch).await.ok();
        // No network, or the remote never had this branch: `resolve_base`
        // already treats that as non-fatal at branch time, and a run that got
        // this far must not be blocked by it here either.
        let Ok(tip) = git::rev_parse(&repo, &tracking).await else {
            return Ok(());
        };

        let head = git::rev_parse(&winner.worktree, "HEAD").await?;
        let behind = git::commits_ahead(&repo, &head, &tip).await.unwrap_or(0);
        let attempts = self.state.base_sync.as_ref().map_or(0, |s| s.attempts);

        if behind == 0 {
            self.state.base_sync = Some(BaseSync {
                tip,
                behind: 0,
                attempts,
                conflict: None,
            });
            self.state.save()?;
            return Ok(());
        }

        if attempts >= BASE_SYNC_ROUNDS {
            let why = format!(
                "{base_branch} moved {behind} commit(s) ahead of {} after {BASE_SYNC_ROUNDS} \
                 rebase(s); rebasing again would only race it",
                winner.branch
            );
            self.state.status = RunStatus::Blocked;
            self.state.base_sync = Some(BaseSync {
                tip,
                behind,
                attempts,
                conflict: Some(why.clone()),
            });
            self.state.event("land", why);
            self.state.save()?;
            return Ok(());
        }

        self.state.event(
            "land",
            format!(
                "{base_branch} moved {behind} commit(s) ahead of {}; rebasing before verifying",
                winner.branch
            ),
        );
        self.state.save()?;

        let scratch = self.state.dir().join("base-sync");
        let rebased = git::rebase_branch_in_temp(&repo, &scratch, &winner.branch, &tracking).await;
        let attempts = attempts + 1;
        match rebased {
            Ok(None) => {
                // The branch ref moved, but a worktree that already had it
                // checked out (the winner's) was not told; sync its index and
                // files before anything reads them.
                git::sync_to_head(&winner.worktree).await?;
                self.state.base_sync = Some(BaseSync {
                    tip: tip.clone(),
                    behind: 0,
                    attempts,
                    conflict: None,
                });
                self.state
                    .event("land", format!("rebased {} onto {tracking}", winner.branch));
            }
            Ok(Some(conflict)) => {
                let why = format!(
                    "{} conflicts with {tracking} and did not rebase: {}",
                    winner.branch,
                    conflict.chars().take(600).collect::<String>()
                );
                self.state.status = RunStatus::Blocked;
                self.state.base_sync = Some(BaseSync {
                    tip,
                    behind,
                    attempts,
                    conflict: Some(why.clone()),
                });
                self.state.event("land", why);
            }
            Err(e) => {
                let why = format!("could not rebase {} onto {tracking}: {e:#}", winner.branch);
                self.state.status = RunStatus::Blocked;
                self.state.base_sync = Some(BaseSync {
                    tip,
                    behind,
                    attempts,
                    conflict: Some(why.clone()),
                });
                self.state.event("land", why);
            }
        }
        self.state.save()?;
        Ok(())
    }

    /// The commit review and gate diff against: the tip [`Self::sync_to_base`]
    /// last landed the winner on, once it has run, else the commit the run
    /// branched from.
    ///
    /// Only [`Self::review_loop`] reads this. `prep`, `judge`, `deliberate`
    /// and `vote` all happen before there is a winner to rebase, so they
    /// compare every candidate against the branch point on purpose, and a
    /// base that moves after they are already done cannot change an answer
    /// they already gave.
    fn landing_base(&self) -> String {
        self.state
            .base_sync
            .as_ref()
            .map_or_else(|| self.state.base_commit.clone(), |s| s.tip.clone())
    }

    // ------------------------------------------------------- operator fix

    /// Route specific, already-recorded review findings to a fixer for a
    /// targeted, out-of-band fix on the winning branch — `magi fix`'s own
    /// entry point.
    ///
    /// Distinct from `review_loop`'s own fix step in three ways: it never
    /// runs a reviewer wave, it never spends review-round budget, and what
    /// happened is recorded as an [`OperatorFixRequest`] appended to
    /// [`RunState::operator_fixes`], never folded into a [`ReviewRound`] —
    /// see `run::SCHEMA`'s doc for schema 9 on why a reviewer's own severity
    /// and vote must never be rewritten to look like a manufactured blocking
    /// verdict.
    ///
    /// Only meaningful once review has actually concluded: `Ready` (handed
    /// off with findings still open, or simply concluded clean while minor
    /// findings sat unaddressed) or `Blocked` (round budget spent, or the
    /// gate failed). Everything else is refused: a run still in progress
    /// should simply be resumed, and a `Merged` run's branch has already
    /// landed — reopening *this* run's own record cannot change that, so the
    /// answer there is a fresh `magi review <branch>`.
    ///
    /// A real commit here re-verifies through a fresh, ordinary review-only
    /// run on the same branch ([`Self::review`]) rather than reopening this
    /// run's own `review_loop`: once any round in this run's history went
    /// clean, `review_conclusion` treats that as permanent by design (the
    /// same purity `gate`/`merge` rely on for safe reentry), so there is no
    /// way to force one more genuine reviewer wave out of *this* run without
    /// either rewriting history or weakening that guarantee for every other
    /// caller. A review-only run costs nothing extra — no implementation, no
    /// judging, no vote — and exercises the exact same review → verify →
    /// gate → (human) merge path, unmodified.
    pub async fn fix_selected(
        &mut self,
        ids: &[String],
        reason: &str,
        allow_stale: bool,
    ) -> Result<()> {
        let reason = reason.trim();
        if reason.is_empty() {
            bail!("a fix request needs a reason — that is the operator's own record of why");
        }
        if ids.is_empty() {
            bail!("no finding id given");
        }
        if !matches!(self.state.status, RunStatus::Ready | RunStatus::Blocked) {
            bail!(
                "run {} is `{}`; only a `ready` or `blocked` run — one whose review \
                 has already concluded — can be given a targeted fix. A run still \
                 in progress should simply be resumed; a `merged` run's branch has \
                 already landed, so its answer is a fresh `magi review <branch>`, \
                 not reopening this run's own record",
                self.state.id,
                self.state.status.as_str()
            );
        }
        let Some(winner) = self.state.winner().cloned() else {
            bail!("run {} has no winning candidate to fix", self.state.id);
        };
        if !git::branch_exists(&self.state.repo, &winner.branch).await? {
            bail!(
                "branch `{}` no longer exists; this run cannot be extended",
                winner.branch
            );
        }
        let home = crate::run::home();
        if crate::daemon::is_working_on(&home, &self.state.id, Timestamp::now()) {
            bail!(
                "run {} is currently being worked on by another magi process",
                self.state.id
            );
        }
        // Held for the rest of this call, including the follow-up review
        // below: two `magi fix` invocations against the same run must not
        // both reach the worktree manipulation further down, which would
        // otherwise race to remove and recreate the same directory — see
        // [`FixClaim`]'s own doc.
        let _claim = FixClaim::acquire(&self.state.dir())?;

        // Resolve every id before spending anything — an unknown id refuses
        // the whole request rather than silently dropping it — and dedup
        // while keeping the operator's own order.
        let mut seen = BTreeSet::new();
        let mut findings = Vec::new();
        let mut missing = Vec::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            match self.state.finding(id) {
                Some((round, rec, f)) => findings.push(OperatorFixFinding {
                    id: f.id.clone(),
                    severity: f.severity,
                    reviewer_vote: rec.vote,
                    round: round.round,
                    round_head: round.head.clone(),
                    reviewer: rec.reviewer,
                    agent: rec.agent.clone(),
                    file: f.file.clone(),
                    line: f.line,
                    title: f.title.clone(),
                    detail: f.detail.clone(),
                    outcome: OperatorFixOutcome::Pending,
                }),
                None => missing.push(id.clone()),
            }
        }
        if !missing.is_empty() {
            bail!(
                "unknown finding id(s): {}; nothing was changed",
                missing.join(", ")
            );
        }

        let head_at_request = git::rev_parse(&self.state.repo, &winner.branch).await?;
        let stale_details: Vec<(String, String)> = findings
            .iter()
            .filter(|f| f.round_head != head_at_request)
            .map(|f| (f.id.clone(), f.round_head.clone()))
            .collect();
        let stale = !stale_details.is_empty();
        if stale && !allow_stale {
            bail!(
                "the branch has moved since some finding(s) were raised — {} — now \
                 at {}; pass --allow-stale to fix anyway, or re-run review first",
                stale_details
                    .iter()
                    .map(|(id, head)| format!("{id} (raised against {})", short(head)))
                    .collect::<Vec<_>>()
                    .join(", "),
                short(&head_at_request)
            );
        }

        let request = OperatorFixRequest {
            requested_at: Timestamp::now(),
            reason: reason.to_owned(),
            findings,
            head_at_request: head_at_request.clone(),
            allow_stale,
            stale,
            fix: None,
            result_head: None,
            follow_up_review_run: None,
        };
        self.state.event(
            "fix",
            format!(
                "operator requested a targeted fix on {} finding(s) ({}): {reason}",
                request.findings.len(),
                request
                    .findings
                    .iter()
                    .map(|f| f.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        );
        // Recorded now, before any worktree work or the fixer call itself —
        // and re-saved at each checkpoint below: a crash at any point after
        // this (mid fixer call, mid follow-up review) must not lose the fact
        // that this was requested, for which findings, and why. Everything
        // past this point reads and writes through `request_index` rather
        // than a local variable, since `request` itself is moved here.
        self.state.operator_fixes.push(request);
        self.state.save()?;
        let request_index = self.state.operator_fixes.len() - 1;

        // A fresh, dedicated worktree for this one call, never the winner's
        // own worktree in place: that one may already be gone (folded away),
        // and reusing it in place would leave the branch checked out there
        // when the follow-up review below tries to check it out again. Freed
        // immediately after, either way — but only once confirmed clean:
        // `worktree_remove` is a `git worktree remove --force`, which would
        // otherwise discard uncommitted work left there by the operator or
        // another process before this had a chance to even look at it.
        if winner.worktree.exists() {
            if !git::is_clean(&winner.worktree).await? {
                bail!(
                    "`{}` has uncommitted changes; refusing to touch it — commit or \
                     discard them first",
                    winner.worktree.display()
                );
            }
            git::worktree_remove(&self.state.repo, &winner.worktree)
                .await
                .ok();
        }
        let fix_worktree = self.state.worktree_root().join("operator-fix");
        let fix_worktree_s = fix_worktree.to_string_lossy().to_string();
        git::git(
            &self.state.repo,
            &["worktree", "add", &fix_worktree_s, winner.branch.as_str()],
        )
        .await
        .with_context(|| format!("checking out `{}` for the fix", winner.branch))?;
        if !git::is_clean(&fix_worktree).await? {
            git::worktree_remove(&self.state.repo, &fix_worktree)
                .await
                .ok();
            bail!(
                "`{}` has uncommitted changes; refusing to start a fix on a dirty tree",
                winner.branch
            );
        }

        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        let language = self.state.config.graph.language.clone();
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let (fix_spec, fix_seat_key) = match &self.roles.fixer {
            Some(f) if f.id != winner.agent => (f.clone(), "fix".to_owned()),
            _ => (
                self.state
                    .config
                    .agent(&winner.agent)
                    .cloned()
                    .unwrap_or_else(|_| self.roles.implementers[winner.index].clone()),
                format!("impl-{}", winner.label),
            ),
        };
        let seat = self.seat(&fix_seat_key, &fix_spec.id);
        let finding_list: Vec<Finding> = self.state.operator_fixes[request_index]
            .findings
            .iter()
            .map(|f| Finding {
                id: f.id.clone(),
                severity: f.severity,
                file: f.file.clone(),
                line: f.line,
                title: f.title.clone(),
                detail: f.detail.clone(),
            })
            .collect();
        let job = SeatJob {
            prompt: prompt::operator_fix(
                &self.state.instruction,
                &finding_list,
                reason,
                &stale_details,
                &head_at_request,
                &language,
            ),
            spec: fix_spec.clone(),
            seat,
            cwd: fix_worktree.clone(),
            timeout: Duration::from_secs(self.state.config.graph.timeout_fix),
            allow_write: true,
            sessions,
            artifacts: artifacts.clone(),
            stem: "operator-fix".to_owned(),
        };
        let cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "fix",
            prompts: &prompts,
            cache: cache.as_deref(),
            round: None,
        };
        let (seat, out) =
            run_one(job.clone(), Arc::clone(&self.sem), &ctx, &mut self.state, 0).await;
        let agent_id = seat.agent.clone();

        let mut fix = FixRecord {
            agent: agent_id,
            addressed: Vec::new(),
            rejected: Vec::new(),
            notes: String::new(),
            committed: false,
            failed: None,
            duration_ms: 0,
            continuation: None,
        };
        let mut final_seat = seat.clone();
        match out {
            AgentOutcome::Ok(o) => {
                fix.duration_ms = o.duration_ms;
                let parsed = verdict::extract_json::<FixReport>(&o.text);
                let incomplete_reason = match &parsed {
                    Ok(_) if has_unconfirmed_command(&o.commands) => Some(
                        "the reply parsed, but it reported a command whose own CLI \
                         never confirmed an exit status"
                            .to_owned(),
                    ),
                    Ok(_) => None,
                    Err(e) => Some(e.to_string()),
                };
                match incomplete_reason {
                    None => {
                        let report = parsed.expect("checked Ok above");
                        fix.addressed = report.addressed;
                        fix.rejected = report.rejected;
                        fix.notes = blind::sanitize_prose(&report.notes, &self.state.config.blind);
                    }
                    Some(reason) => {
                        let (resumed_seat, resolved, failure, cont) = self
                            .continue_fix_report(seat, reason, &job, &prompts, &run_id, 0)
                            .await;
                        fix.duration_ms += cont.cumulative_wait_ms;
                        fix.continuation = Some(cont);
                        final_seat = resumed_seat;
                        match resolved {
                            Some(report) => {
                                fix.addressed = report.addressed;
                                fix.rejected = report.rejected;
                                fix.notes =
                                    blind::sanitize_prose(&report.notes, &self.state.config.blind);
                            }
                            None => fix.failed = failure,
                        }
                    }
                }
            }
            AgentOutcome::Dropped(o) => {
                fix.duration_ms = o.duration_ms;
                let why = o
                    .dropped
                    .as_ref()
                    .map(|d| d.why.as_str())
                    .unwrap_or("the CLI ended the stream without delivering its answer");
                fix.failed = Some(format!("the CLI dropped the stream ({why})"));
            }
            AgentOutcome::Quota(o) => {
                self.state.quota.push(QuotaLoss {
                    seat: final_seat.key.clone(),
                    node: "fix".to_owned(),
                    at: Timestamp::now(),
                    reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                });
                fix.failed = Some("rate limited (quota); fixer could not run".to_owned());
            }
            AgentOutcome::Failed(e) => fix.failed = Some(e),
        }
        if fix.continuation.is_none() {
            fix.continuation = Some(ContinuationRecord::not_needed());
        }
        self.state.seats.insert(final_seat.key.clone(), final_seat);

        git::commit_all(
            &fix_worktree,
            &format!(
                "magi: operator-selected fix ({}) (uncommitted work)",
                self.state.operator_fixes[request_index]
                    .findings
                    .iter()
                    .map(|f| f.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .await
        .ok();
        let after = git::rev_parse(&fix_worktree, "HEAD").await?;
        fix.committed = after != head_at_request;
        git::worktree_remove(&self.state.repo, &fix_worktree)
            .await
            .ok();

        self.state.event(
            "fix",
            match &fix.failed {
                Some(reason) => format!(
                    "operator fix: adoption report was lost ({reason}); {}",
                    if fix.committed {
                        "committed"
                    } else {
                        "NO new commit"
                    }
                ),
                None => format!(
                    "operator fix: {} addressed, {} rejected, {}",
                    fix.addressed.len(),
                    fix.rejected.len(),
                    if fix.committed {
                        "committed"
                    } else {
                        "NO new commit"
                    }
                ),
            },
        );

        // Every selected finding gets an outcome — never left `Pending` once
        // the fixer's own turn is over. A report that never came back at all
        // marks every one of them `Unreported`, not silently "not addressed":
        // quota, a dropped stream, or an exhausted continuation are gaps in
        // the report, not evidence about the finding itself (see [`SCHEMA`]'s
        // doc for schema 9 and [`OperatorFixOutcome::Unreported`]).
        for f in &mut self.state.operator_fixes[request_index].findings {
            f.outcome = if fix.failed.is_some() {
                OperatorFixOutcome::Unreported
            } else if fix.addressed.contains(&f.id) {
                OperatorFixOutcome::Addressed
            } else if let Some(r) = fix.rejected.iter().find(|r| r.id == f.id) {
                OperatorFixOutcome::Rejected { why: r.why.clone() }
            } else {
                OperatorFixOutcome::Unreported
            };
        }

        let committed = fix.committed;
        if committed {
            self.state.operator_fixes[request_index].result_head = Some(after.clone());
        }
        self.state.operator_fixes[request_index].fix = Some(fix);
        // Saved again now that the fixer's own outcome is final, on top of
        // the save right after the request was first pushed above.
        self.state.save()?;

        if committed {
            self.state.event(
                "fix",
                format!(
                    "operator fix committed {}; opening a follow-up review-only run",
                    short(&after)
                ),
            );
            match Self::review(&self.state.repo, &winner.branch, self.state.config.clone()).await {
                Ok(mut follow_up) => {
                    follow_up.state.event(
                        "start",
                        format!(
                            "requested by an operator fix on run {} for finding(s) {}",
                            self.state.id,
                            self.state.operator_fixes[request_index]
                                .findings
                                .iter()
                                .map(|f| f.id.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                    );
                    follow_up.state.save()?;
                    let follow_up_id = follow_up.state.id.clone();
                    if let Err(e) = follow_up.execute().await {
                        self.state.event(
                            "fix",
                            format!(
                                "follow-up review {follow_up_id} did not complete cleanly: {e:#}"
                            ),
                        );
                    }
                    self.state.operator_fixes[request_index].follow_up_review_run =
                        Some(follow_up_id);
                }
                Err(e) => {
                    self.state.event(
                        "fix",
                        format!("committed the fix but could not open a follow-up review: {e:#}"),
                    );
                }
            }
            self.state.save()?;
        }

        Ok(())
    }

    // --------------------------------------------------------------- review

    async fn review_loop(&mut self) -> Result<()> {
        // A base that would not rebase is a person's decision, not a review
        // round: nothing here would change the answer, and reviewers and a
        // fixer would be spending real budget on a tree that cannot land
        // regardless of what they find.
        if self
            .state
            .base_sync
            .as_ref()
            .is_some_and(|s| s.conflict.is_some())
        {
            return Ok(());
        }
        // Attribution for every agent this node spawns: `MAGI_RUN` lets a task the
        // agent files with `magi task add` name the run that paid for it. The
        // prompt overlay is cloned alongside it because the waves borrow it
        // while `self` is mutably borrowed by the node's own bookkeeping.
        let run_id = self.state.id.clone();
        let prompts = self.state.config.prompts.clone();
        let Some(winner) = self.state.winner().cloned() else {
            return Ok(());
        };
        let max_rounds = self.state.config.graph.review_rounds;
        // A clean round, an exhausted round budget, or a stalled tree (see
        // `STAGNANT_LIMIT`) are all already-decided conclusions the moment
        // they are recorded — recomputed here, not read off `status`, so a
        // reentry into a run that already stopped restates the identical
        // verdict instead of silently handing back whatever an earlier node
        // in this same walk clobbered `status` to (a solo-candidate
        // `judge`/`deliberate` skip rewrites it on every reentry). The loop
        // below runs an empty range once the budget is spent, and would
        // otherwise fall through without touching `status` at all.
        if let Some(status) = review_conclusion(&self.state.reviews, max_rounds) {
            self.state.status = status;
            self.state.save()?;
            return Ok(());
        }
        self.state.status = RunStatus::Reviewing;
        // A last recorded round whose own verification never resolved
        // (`ResourceBlocked` — the shared build cache, not the patch) is
        // never a concluded round, whatever the round budget says: starting
        // a fresh round on top of it would spend a whole new reviewer wave
        // re-reading an unchanged patch instead of just retrying the one
        // check that actually needs it, and once the budget is spent the
        // loop below has nothing left to do at all (its range is empty).
        // Retry that check directly instead, exactly the same retry
        // `stop_reviewing` already does for its own catch-up case.
        if self
            .state
            .reviews
            .last()
            .is_some_and(|r| r.e2e_status() == E2eStatus::ResourceBlocked)
        {
            let shell = self.state.config.shell();
            return self
                .stop_reviewing(
                    "the last round's own verification never resolved",
                    &shell,
                    &winner.worktree,
                )
                .await;
        }

        let repo = self.state.repo.clone();
        let root = self.state.worktree_root();
        let language = self.state.config.graph.language.clone();
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let base = self.landing_base();
        let base_short = short(&base);
        let reviewers = self.roles.reviewers.clone();
        let shell = self.state.config.shell();

        for round in (self.state.reviews.len() + 1)..=max_rounds {
            let head = git::rev_parse(&winner.worktree, "HEAD").await?;
            let patch = git::diff(&winner.worktree, &base, "HEAD").await?;
            let stat = git::diff_stat(&winner.worktree, &base, "HEAD").await?;
            // The prior round's own record, already persisted — never a
            // hand-carried variable of just its failing output: that is
            // exactly what let a round's e2e result drift out of sync with
            // which commit it was actually about (see `SCHEMA`'s doc for
            // schema 8). Judged against `head`, the commit reviewers are
            // about to look at now, so the summary always reads as "an
            // earlier head" here — this round's own patch has not been
            // checked yet.
            let prev_verification = self
                .state
                .reviews
                .last()
                .and_then(|r| r.verification_summary(&head));

            // Each reviewer gets its own detached checkout of exactly this
            // commit: nobody can perturb the winner's tree, and the fixer can
            // keep working without racing a reviewer.
            let mut jobs = Vec::new();
            for (r, spec) in reviewers.iter().cloned().enumerate() {
                let wt = root.join(format!("review-{}", r + 1));
                if wt.exists() {
                    git::reset_detached(&wt, &head).await?;
                } else {
                    git::worktree_add_detached(&repo, &wt, &head).await?;
                }
                let seat_key = format!("review-{}", r + 1);
                let seat = self.seat(&seat_key, &spec.id);
                jobs.push(SeatJob {
                    prompt: prompt::review(&prompt::ReviewCtx {
                        instruction: &self.state.instruction,
                        branch: &winner.branch,
                        base_short: &base_short,
                        stat: &stat,
                        patch: &patch,
                        verification: prev_verification.as_ref(),
                        reviewers: reviewers.len(),
                        round,
                        rounds: max_rounds,
                        // A review-only run has no rankings, so nothing
                        // competed for this patch and the reviewer is told so.
                        competed: self.state.tally.as_ref().is_some_and(|t| t.rankings > 0),
                        lens: Lens::for_seat(r),
                        language: &language,
                    }),
                    spec,
                    seat,
                    cwd: wt,
                    timeout: Duration::from_secs(self.state.config.graph.timeout_review),
                    allow_write: false,
                    sessions,
                    artifacts: artifacts.clone(),
                    stem: format!("review-{round}-{}", r + 1),
                });
            }

            self.state.event(
                "review",
                format!(
                    "round {round}: {} reviewers on {}",
                    jobs.len(),
                    short(&head)
                ),
            );
            let mut quota_losses = Vec::new();
            let review_retries = self.state.config.graph.retries;
            let review_cache = self.state.config.cache_dir();
            let ctx = WaveCtx {
                run: &run_id,
                node: "review",
                prompts: &prompts,
                cache: review_cache.as_deref(),
                round: Some(round),
            };
            let results = ask_json_wave::<Review>(
                jobs,
                Arc::clone(&self.sem),
                review_retries,
                &ctx,
                &mut quota_losses,
                &mut self.state,
                &|_: &Review| Ok(()),
            )
            .await;
            // Counted before the move below: how many of *this* round's
            // reviewer seats were lost to their own rate limit, as opposed to
            // a crash, a timeout, or unparsable output — see `round_is_clean`.
            let round_quota_missing = quota_losses.len();
            self.state.quota.extend(quota_losses);

            let mut records = Vec::new();
            let mut all_findings = Vec::new();
            for (r, (seat, res)) in results.into_iter().enumerate() {
                let agent_id = seat.agent.clone();
                self.state.seats.insert(seat.key.clone(), seat);
                let mut record = ReviewRecord {
                    reviewer: r + 1,
                    agent: agent_id,
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: None,
                    failed: None,
                    duration_ms: 0,
                };
                match res {
                    Ok((review, out)) => {
                        // Sanitized here, at the point every other piece of
                        // agent prose in this file is (candidate summaries,
                        // deliberation turns, vote reasons): a reviewer's own
                        // words are the one thing about it that could name
                        // it, and reconsideration below broadcasts this same
                        // summary and these same findings to every other
                        // seat on the panel.
                        record.summary =
                            blind::sanitize_prose(&review.summary, &self.state.config.blind);
                        record.vote = Some(review.vote);
                        record.duration_ms = out.duration_ms;
                        for (n, mut f) in review.findings.into_iter().enumerate() {
                            // ids are magi's, never the agent's: the fixer's
                            // adoption report is keyed by them.
                            f.id = format!("R{round}-{}-{}", r + 1, n + 1);
                            f.title = blind::sanitize_prose(&f.title, &self.state.config.blind);
                            f.detail = blind::sanitize_prose(&f.detail, &self.state.config.blind);
                            // `file` is agent-supplied prose too, never
                            // checked against the real tree — the same
                            // exposure `title`/`detail` above have, just in
                            // a field easy to forget because it looks like a
                            // path rather than free text.
                            f.file = f
                                .file
                                .map(|file| blind::sanitize_prose(&file, &self.state.config.blind));
                            all_findings.push(f.clone());
                            record.findings.push(f);
                        }
                        self.state.event(
                            "review",
                            format!(
                                "round {round}: reviewer {} voted {} with {} finding(s)",
                                r + 1,
                                review.vote.label(),
                                record.findings.len()
                            ),
                        );
                    }
                    Err(e) => {
                        record.failed = Some(e.to_string());
                        self.state.event(
                            "review",
                            format!("round {round}: reviewer {} produced nothing: {e}", r + 1),
                        );
                    }
                }
                records.push(record);
            }

            // Tally the round's votes and, if they split, spend the one
            // round of reconsideration the split -> deliberate -> revote
            // shape `judge`/`vote` use for the panel, sized down to what a
            // read-only review round can afford: one round, and a revote
            // rather than an argument, because the panel already wrote its
            // reasoning down as findings the first time around.
            let initial_votes: Vec<ReviewVote> = records.iter().filter_map(|r| r.vote).collect();
            let vote_split =
                initial_votes.len() > 1 && !initial_votes.iter().all(|v| *v == initial_votes[0]);
            let mut reconsideration: Vec<ReviewRevoteRecord> = Vec::new();
            if vote_split {
                self.state.event(
                    "review",
                    format!(
                        "round {round}: votes split ({}) — one round of reconsideration",
                        initial_votes
                            .iter()
                            .map(|v| v.label())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                // Seats read every seat's findings and votes, still numbered
                // and never named — the same anonymity `review` itself keeps.
                let panel: Vec<ReviewSeatReport<'_>> = records
                    .iter()
                    .filter_map(|r| {
                        r.vote.map(|vote| ReviewSeatReport {
                            reviewer: r.reviewer,
                            vote,
                            summary: &r.summary,
                            findings: &r.findings,
                        })
                    })
                    .collect();

                let mut jobs = Vec::new();
                let mut seats_at = Vec::new();
                for (r, spec) in reviewers.iter().cloned().enumerate() {
                    // A seat with no initial vote has nothing to reconsider
                    // from and stays absent, the same as it stayed absent
                    // from `panel` above.
                    if records[r].vote.is_none() {
                        continue;
                    }
                    let wt = root.join(format!("review-{}", r + 1));
                    let seat_key = format!("review-{}", r + 1);
                    let seat = self.seat(&seat_key, &spec.id);
                    // A seat with no live session has already forgotten the
                    // initial review's prompt — restate the patch it is
                    // voting on, the same as `deliberate`/`vote` do for a
                    // judge in the same position.
                    let patch_ctx = if has_context(&spec, &seat, sessions) {
                        None
                    } else {
                        Some(ReviewPatch {
                            branch: &winner.branch,
                            base_short: &base_short,
                            stat: &stat,
                            patch: &patch,
                        })
                    };
                    let prompt = prompt::review_reconsider(&ReviewReconsiderCtx {
                        instruction: &self.state.instruction,
                        reviewer: r + 1,
                        lens: Lens::for_seat(r),
                        panel: &panel,
                        patch: patch_ctx,
                        round,
                        rounds: max_rounds,
                        language: &language,
                    });
                    jobs.push(SeatJob {
                        prompt,
                        spec,
                        seat,
                        cwd: wt,
                        timeout: Duration::from_secs(self.state.config.graph.timeout_review),
                        allow_write: false,
                        sessions,
                        artifacts: artifacts.clone(),
                        stem: format!("review-{round}-reconsider-{}", r + 1),
                    });
                    seats_at.push(r);
                }

                let mut recon_quota_losses = Vec::new();
                let recon_cache = self.state.config.cache_dir();
                let recon_ctx = WaveCtx {
                    run: &run_id,
                    node: "review",
                    prompts: &prompts,
                    cache: recon_cache.as_deref(),
                    round: Some(round),
                };
                let recon_results = ask_json_wave::<ReviewRevote>(
                    jobs,
                    Arc::clone(&self.sem),
                    review_retries,
                    &recon_ctx,
                    &mut recon_quota_losses,
                    &mut self.state,
                    &|_: &ReviewRevote| Ok(()),
                )
                .await;
                self.state.quota.extend(recon_quota_losses);

                for (&r, (seat, res)) in seats_at.iter().zip(recon_results) {
                    let agent_id = seat.agent.clone();
                    self.state.seats.insert(seat.key.clone(), seat);
                    let mut rec = ReviewRevoteRecord {
                        reviewer: r + 1,
                        agent: agent_id,
                        vote: None,
                        reason: String::new(),
                        failed: None,
                    };
                    match res {
                        Ok((rv, _)) => {
                            rec.vote = Some(rv.vote);
                            rec.reason =
                                blind::sanitize_prose(&rv.reason, &self.state.config.blind);
                            self.state.event(
                                "review",
                                format!(
                                    "round {round}: reviewer {} revoted {}",
                                    r + 1,
                                    rv.vote.label()
                                ),
                            );
                        }
                        Err(e) => {
                            rec.failed = Some(e.to_string());
                            self.state.event(
                                "review",
                                format!("round {round}: reviewer {} did not revote: {e}", r + 1),
                            );
                        }
                    }
                    reconsideration.push(rec);
                }
            } else if initial_votes.len() > 1 {
                self.state.event(
                    "review",
                    format!(
                        "round {round}: votes agreed ({}) — no reconsideration",
                        initial_votes[0].label()
                    ),
                );
            }

            // The final vote per seat is its revote where reconsideration
            // ran and answered, its initial vote otherwise — the same
            // fallback `tally` uses for a judge whose private vote failed.
            let final_votes: Vec<ReviewVote> = records
                .iter()
                .filter_map(|r| {
                    reconsideration
                        .iter()
                        .find(|rv| rv.reviewer == r.reviewer)
                        .and_then(|rv| rv.vote)
                        .or(r.vote)
                })
                .collect();
            let round_verdict = ReviewVote::worst(final_votes);

            let blocking = all_findings.iter().filter(|f| f.severity.blocks()).count();
            let verify_timeout = Duration::from_secs(self.state.config.graph.verify_timeout());
            // A round that already has a blocking finding and a round left to
            // try is going back to the fixer no matter what `verify.e2e`
            // says, so running it first only spends the loop's slowest step
            // (minutes, for a Rust repo's full test suite) on a head about
            // to be rewritten. Deferred, never skipped: `verify.e2e` still
            // runs once a round has no blocking findings left (see
            // `round_is_clean`, which a deferred — empty — `e2e` can never
            // satisfy since `blocking` is nonzero whenever this branch is
            // taken), and `stop_reviewing` forces a real run before it will
            // ever read a deferred round as green.
            let defer_e2e =
                blocking > 0 && round < max_rounds && !self.state.config.graph.e2e_every_round;
            let (e2e, verify_retried, e2e_deferred, e2e_defer_reason) = if defer_e2e {
                let reason =
                    format!("{blocking} blocking finding(s) already required a fix this round");
                self.state.event(
                    "verify",
                    format!(
                        "round {round}: {reason} — e2e deferred to the fixer (reviewed head \
                         {}); it will run once a round has none left",
                        short(&head)
                    ),
                );
                (Vec::new(), false, true, Some(reason))
            } else {
                let e2e_commands = self.state.config.verify.e2e.clone();
                let cache_dir = self.state.config.cache_dir();
                let context = format!("round {round}");
                let (e2e, verify_retried) = with_cache_lease(
                    &mut self.state,
                    cache_dir.as_deref(),
                    "e2e",
                    "e2e",
                    &winner.worktree,
                    &head,
                    verify_timeout,
                    &context,
                    |state, budget| {
                        let shell = shell.clone();
                        let e2e_commands = e2e_commands.clone();
                        let worktree = winner.worktree.clone();
                        let context = context.clone();
                        async move {
                            run_e2e_with_retry(
                                state,
                                &shell,
                                &e2e_commands,
                                &worktree,
                                budget,
                                &context,
                            )
                            .await
                        }
                    },
                )
                .await;
                (e2e, verify_retried, false, None)
            };

            let expected = records.len();
            let answered = records.iter().filter(|r| r.failed.is_none()).count();
            let incomplete = answered < expected;
            let e2e_ok = e2e.iter().all(CommandOutcome::ok);
            let policy = self.state.config.graph.incomplete_review;
            let clean = round_is_clean(
                blocking,
                e2e_ok,
                answered,
                expected,
                round_quota_missing,
                policy,
            );

            let mut round_record = ReviewRound {
                round,
                head: head.clone(),
                verified_head: None,
                verified_at: None,
                reviews: records,
                e2e,
                verify_retried,
                e2e_deferred,
                e2e_defer_reason,
                fix: None,
                blocking,
                answered,
                expected,
                clean,
                progressed: false,
                vote_split,
                reconsideration,
                verdict: round_verdict,
            };
            // Which commit and when magi actually attempted to check —
            // known the moment a command was dispatched against `head`,
            // whether or not it finished: a resource-blocked attempt still
            // targeted a specific commit at a specific time, and leaving
            // that unrecorded is exactly what made `verification_summary`
            // report a fresh attempt as "commit unknown ... recorded before
            // this was tracked", indistinguishable from a genuinely old,
            // untracked record. Only a deferred or unconfigured round never
            // ran at all and has nothing to record — see
            // `ReviewRound::verified_head`'s own doc.
            if !matches!(
                round_record.e2e_status(),
                E2eStatus::Deferred | E2eStatus::NotConfigured
            ) {
                round_record.verified_head = Some(head.clone());
                round_record.verified_at = Some(Timestamp::now());
            }
            let this_round_verification = round_record.verification_summary(&head);

            if incomplete {
                let missing: Vec<String> = round_record
                    .reviews
                    .iter()
                    .filter(|r| r.failed.is_some())
                    .map(|r| format!("review-{}", r.reviewer))
                    .collect();
                self.state.event(
                    "review",
                    format!(
                        "round {round}: {answered}/{expected} reviewer(s) answered ({} never answered)",
                        missing.join(", ")
                    ),
                );
            }

            if clean {
                self.state.event(
                    "review",
                    if incomplete && policy == IncompleteReviewPolicy::Warn {
                        format!(
                            "round {round}: clean (warn policy, incomplete panel) — no \
                             blocking findings from the seats that answered, verification green"
                        )
                    } else if incomplete {
                        format!(
                            "round {round}: clean ({} rate-limited reviewer(s) excluded from \
                             quorum) — no blocking findings from the seats that answered, \
                             verification green",
                            expected - answered
                        )
                    } else {
                        format!("round {round}: clean — no blocking findings, verification green")
                    },
                );
                self.state.reviews.push(round_record);
                self.state.status = RunStatus::Gating;
                self.state.save()?;
                return Ok(());
            }

            // Nothing was raised and verification passed, but not every seat
            // answered and `round_is_clean` still refused to call it clean —
            // either a seat is missing for a reason other than its own quota
            // (a crash, a timeout, unparsable output — worth another try), or
            // every seat that could have answered lost its quota and nobody
            // is left to decide on: re-review rather than send the fixer
            // after a round with nothing to fix.
            if incomplete && blocking == 0 && e2e_ok {
                self.state.reviews.push(round_record);
                self.state.save()?;
                if round == max_rounds {
                    self.state.status = RunStatus::Blocked;
                    self.state.event(
                        "review",
                        format!(
                            "{} reviewer seat(s) never answered after {max_rounds} rounds; \
                             refusing to call it clean",
                            expected - answered
                        ),
                    );
                    return Ok(());
                }
                continue;
            }

            // Nothing for the fixer to act on (`blocking == 0`) and the only
            // reason this round is not clean is that magi itself never got
            // a command to run — the shared build cache, not the patch (see
            // `CommandOutcome::resource_blocked`'s own doc). Sending that to
            // the fixer would invite a change to appease contention that has
            // nothing to do with the diff, and would leave this attempt
            // sitting in the next round's prompt as if it were about an
            // earlier, superseded commit rather than what it actually is:
            // the same head, still waiting to be checked. Wait for it the
            // same way the final round's own contention is already handled,
            // whatever round this happens to be.
            if blocking == 0 && round_record.e2e_status() == E2eStatus::ResourceBlocked {
                self.state.reviews.push(round_record);
                return self
                    .stop_reviewing(
                        "the round's own verification could not run",
                        &shell,
                        &winner.worktree,
                    )
                    .await;
            }

            if round == max_rounds {
                self.state.reviews.push(round_record);
                return self
                    .stop_reviewing(
                        &format!(
                            "{blocking} blocking finding(s) still open after {max_rounds} round(s)"
                        ),
                        &shell,
                        &winner.worktree,
                    )
                    .await;
            }

            // Fix. The winner's own implementer seat continues its conversation:
            // the competition is over, so context is pure benefit now.
            let (fix_spec, fix_seat_key) = match &self.roles.fixer {
                Some(f) if f.id != winner.agent => (f.clone(), "fix".to_owned()),
                _ => (
                    self.state
                        .config
                        .agent(&winner.agent)
                        .cloned()
                        .unwrap_or_else(|_| self.roles.implementers[winner.index].clone()),
                    format!("impl-{}", winner.label),
                ),
            };
            let seat = self.seat(&fix_seat_key, &fix_spec.id);
            let blocking_findings: Vec<_> = all_findings
                .iter()
                .filter(|f| f.severity.blocks())
                .cloned()
                .collect();
            let job = SeatJob {
                prompt: prompt::fix(
                    &self.state.instruction,
                    &blocking_findings,
                    this_round_verification.as_ref(),
                    round,
                    max_rounds,
                    &language,
                ),
                spec: fix_spec.clone(),
                seat,
                cwd: winner.worktree.clone(),
                timeout: Duration::from_secs(self.state.config.graph.timeout_fix),
                allow_write: true,
                sessions,
                artifacts: artifacts.clone(),
                stem: format!("fix-{round}"),
            };
            let before = git::rev_parse(&winner.worktree, "HEAD").await?;
            let cache = self.state.config.cache_dir();
            let ctx = WaveCtx {
                run: &run_id,
                node: "fix",
                prompts: &prompts,
                cache: cache.as_deref(),
                round: Some(round),
            };
            let (seat, out) =
                run_one(job.clone(), Arc::clone(&self.sem), &ctx, &mut self.state, 0).await;
            let agent_id = seat.agent.clone();

            let mut fix = FixRecord {
                agent: agent_id,
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: false,
                failed: None,
                duration_ms: 0,
                continuation: None,
            };
            let mut continuation = ContinuationRecord::not_needed();
            let mut final_seat = seat.clone();
            match out {
                AgentOutcome::Ok(o) => {
                    fix.duration_ms = o.duration_ms;
                    let parsed = verdict::extract_json::<FixReport>(&o.text);
                    // A parsed report standing next to a command this same
                    // reply's own CLI never confirmed the exit status of is
                    // not a resolved answer — the identical `CommandEvidence`
                    // `state.jobs` renders, read here instead of only on
                    // display, per the completion judgment and the shown
                    // record needing to agree.
                    let incomplete_reason = match &parsed {
                        Ok(_) if has_unconfirmed_command(&o.commands) => Some(
                            "the reply parsed, but it reported a command whose own CLI never \
                             confirmed an exit status"
                                .to_owned(),
                        ),
                        Ok(_) => None,
                        Err(e) => Some(e.to_string()),
                    };
                    match incomplete_reason {
                        None => {
                            let report = parsed.expect("checked Ok above");
                            fix.addressed = report.addressed;
                            fix.rejected = report.rejected;
                            fix.notes =
                                blind::sanitize_prose(&report.notes, &self.state.config.blind);
                        }
                        Some(reason) => {
                            let (resumed_seat, resolved, failure, cont) = self
                                .continue_fix_report(seat, reason, &job, &prompts, &run_id, round)
                                .await;
                            fix.duration_ms += cont.cumulative_wait_ms;
                            continuation = cont;
                            final_seat = resumed_seat;
                            match resolved {
                                Some(report) => {
                                    fix.addressed = report.addressed;
                                    fix.rejected = report.rejected;
                                    fix.notes = blind::sanitize_prose(
                                        &report.notes,
                                        &self.state.config.blind,
                                    );
                                }
                                None => fix.failed = failure,
                            }
                        }
                    }
                }
                // The CLI's raw error JSON is not a fix report to parse.
                AgentOutcome::Dropped(o) => {
                    fix.duration_ms = o.duration_ms;
                    let why = o
                        .dropped
                        .as_ref()
                        .map(|d| d.why.as_str())
                        .unwrap_or("the CLI ended the stream without delivering its answer");
                    fix.failed = Some(format!("the CLI dropped the stream ({why})"));
                }
                AgentOutcome::Quota(o) => {
                    self.state.quota.push(QuotaLoss {
                        seat: final_seat.key.clone(),
                        node: "fix".to_owned(),
                        at: Timestamp::now(),
                        reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                    });
                    fix.failed = Some("rate limited (quota); fixer could not run".to_owned());
                }
                AgentOutcome::Failed(e) => fix.failed = Some(e),
            }
            fix.continuation = Some(continuation);
            self.state.seats.insert(final_seat.key.clone(), final_seat);
            git::commit_all(
                &winner.worktree,
                &format!("magi: review round {round} fixes (uncommitted work)"),
            )
            .await
            .ok();
            let after = git::rev_parse(&winner.worktree, "HEAD").await?;
            fix.committed = after != before;
            // Judged by what `git` says moved against base, never by the
            // fixer's own `addressed`/`rejected` count — see
            // `ReviewRound::progressed`. Propagated with `?`, the same as the
            // `patch` snapshot above: swallowing this error would default
            // `diff_after` to empty, which almost always differs from a
            // non-empty `patch` and reads as "progressed" — exactly backwards
            // for a `git` failure the stagnation check cannot see through.
            let diff_after = git::diff(&winner.worktree, &base, "HEAD").await?;
            let progressed = diff_after != patch;
            let commit_note = if fix.committed {
                "committed"
            } else {
                "NO new commit"
            };
            let tree_note = if progressed {
                "changed vs base"
            } else {
                "unchanged vs base"
            };
            self.state.event(
                "fix",
                match &fix.failed {
                    // Distinct on purpose from "0 addressed, 0 rejected": the
                    // fixer's own diff still landed (blocking counts do keep
                    // falling round over round), only its adoption report did
                    // not come back, so this must never read like every
                    // finding was reviewed and declined.
                    Some(reason) => {
                        format!(
                            "round {round}: fixer's adoption report was lost ({reason}); \
                             {commit_note}, tree {tree_note}"
                        )
                    }
                    None => format!(
                        "round {round}: {} addressed, {} rejected, {commit_note}, tree \
                         {tree_note}{}",
                        fix.addressed.len(),
                        fix.rejected.len(),
                        if continuation.outcome == ContinuationOutcome::Resumed {
                            format!(
                                " (adoption report recovered after {} continuation(s))",
                                continuation.attempts
                            )
                        } else {
                            String::new()
                        },
                    ),
                },
            );
            round_record.fix = Some(fix);
            round_record.progressed = progressed;
            self.state.reviews.push(round_record);
            self.state.save()?;

            // The fixer's own report never came back this round, even after
            // `continue_fix_report`'s own budget was spent on it — not an
            // ordinary "no report" (dropped stream, quota, plain failure),
            // which already reads that way and is left to the existing round
            // budget. Stopping here, rather than opening another round, is
            // what keeps a next reviewer/fixer wave from ever being
            // dispatched onto `winner.worktree` while whatever the seat's
            // last call may still have running there is unaccounted for: no
            // process liveness check exists (and none is being added — see
            // AGENTS.md/this task's own scope), so the only way to honour
            // "nothing starts before a valid report returns" is to not start
            // anything further on this worktree from this run at all.
            if matches!(
                continuation.outcome,
                ContinuationOutcome::Exhausted
                    | ContinuationOutcome::QuotaLost
                    | ContinuationOutcome::NoSession
            ) {
                return self
                    .stop_reviewing(
                        "the fixer's adoption report never came back, even after resuming its \
                         own seat; refusing to start another round against the same worktree \
                         while that is unresolved",
                        &shell,
                        &winner.worktree,
                    )
                    .await;
            }

            let streak = self
                .state
                .reviews
                .iter()
                .rev()
                .take_while(|r| !r.progressed)
                .count();
            if streak >= STAGNANT_LIMIT {
                return self
                    .stop_reviewing(
                        &format!(
                            "the tree has not moved against base for {streak} round(s) in a row"
                        ),
                        &shell,
                        &winner.worktree,
                    )
                    .await;
            }
        }
        Ok(())
    }

    /// Decide, from the last recorded round's own verification, whether
    /// stopping the review loop is a hand-off or a genuine block.
    ///
    /// Called once the loop has given up trying — the round budget is spent,
    /// or the tree stopped moving (see [`STAGNANT_LIMIT`]) — with blocking
    /// findings still open, never while a round is still clean or the
    /// incomplete-panel case handled inline above. Gate and e2e are facts
    /// about the tree; a lingering review finding is an opinion, and this
    /// workload's own `magi stats` puts reviewer precision low enough
    /// (12-33%, 0.18-0.29 adopted per round) that a panel of open findings
    /// must not by itself stand between a green, verified change and the
    /// human who decides what to do with it. A red e2e is not an opinion, so
    /// that case still blocks, with the failing command and a tail of its
    /// output recorded here rather than left in `run.json` for someone to go
    /// find.
    ///
    /// A round that deferred its own e2e (see [`Config::graph`]'s
    /// `e2e_every_round`) is never read as that green: its `e2e` is empty
    /// only because nothing ran, and treating an empty list as a passing one
    /// here is exactly the "deferred painted green" bug this function exists
    /// to not have. When the last round's own verification never resolved —
    /// deferred on purpose, or a real attempt the shared build cache blocked
    /// — this makes (or retries) the real run, on the actual worktree this
    /// loop is about to stop touching, before deciding anything. A
    /// resource-blocked attempt is likewise never read as either green or
    /// red: it is evidence about the machine, not the patch (see
    /// [`CommandOutcome::resource_blocked`]'s own doc), so a persistently
    /// blocked cache leaves this call without deciding rather than guessing
    /// — the caller retries on a later reentry.
    async fn stop_reviewing(&mut self, why: &str, shell: &[String], worktree: &Path) -> Result<()> {
        let round_idx = self.state.reviews.len() - 1;
        // A deferred round and a resource-blocked one are the same shape
        // here: neither has a real result yet, and both get one more
        // attempt. Read off `e2e_status` — the single source for this —
        // rather than `e2e.is_empty()` alone, so a resource-blocked attempt
        // (whose `e2e` is *not* empty; see `CommandOutcome::resource_blocked`)
        // still retries instead of being read as a settled result the
        // instant it stops being empty.
        let needs_catchup_run = matches!(
            self.state.reviews[round_idx].e2e_status(),
            E2eStatus::Deferred | E2eStatus::ResourceBlocked
        );
        if needs_catchup_run {
            let round = self.state.reviews[round_idx].round;
            let timeout = Duration::from_secs(self.state.config.graph.verify_timeout());
            let commands = self.state.config.verify.e2e.clone();
            let attempted_head = git::rev_parse(worktree, "HEAD").await?;
            let cache_dir = self.state.config.cache_dir();
            let context = format!(
                "round {round}: verification unresolved, catching up before the final decision"
            );
            let (outcomes, verify_retried) = with_cache_lease(
                &mut self.state,
                cache_dir.as_deref(),
                "e2e",
                "e2e",
                worktree,
                &attempted_head,
                timeout,
                &context,
                |state, budget| {
                    let shell = shell.to_vec();
                    let commands = commands.clone();
                    let context = context.clone();
                    async move {
                        run_e2e_with_retry(state, &shell, &commands, worktree, budget, &context)
                            .await
                    }
                },
            )
            .await;
            let last = &mut self.state.reviews[round_idx];
            last.e2e = outcomes;
            last.verify_retried = verify_retried;
            // Always the commit and time this attempt actually targeted,
            // whether or not it happens to equal the reviewed `head` and
            // whether or not a command finished — see
            // `ReviewRound::verified_head`'s own doc. A still-inconclusive
            // attempt is recorded too, so a later reader sees "attempted
            // again at T2" rather than silence.
            last.verified_head = Some(attempted_head);
            last.verified_at = Some(Timestamp::now());
            if verify_inconclusive(&last.e2e) {
                // Still not a real result: `e2e_deferred` is left exactly
                // as it was, so `needs_catchup_run` above reads
                // `ResourceBlocked` (via `e2e_status`, which checks
                // `resource_blocked` before `e2e_deferred`) and retries
                // again on the next reentry, rather than recording
                // contention as a red e2e and blocking the run on it.
                self.state.save()?;
                return Ok(());
            }
            last.e2e_deferred = false;
        }
        let last = &self.state.reviews[round_idx];
        let open: usize = last.reviews.iter().map(|r| r.findings.len()).sum();

        match last.e2e_status() {
            E2eStatus::Failed => {
                let red: Vec<String> = last
                    .e2e
                    .iter()
                    .filter(|o| !o.ok())
                    .map(|o| {
                        format!(
                            "`{}` -> {:?}\n{}",
                            o.command,
                            o.code,
                            tail(&o.output_tail, EVENT_OUTPUT_TAIL)
                        )
                    })
                    .collect();
                self.state
                    .event("review", format!("{why}; e2e failed:\n{}", red.join("\n")));
                self.state.status = RunStatus::Blocked;
            }
            // `needs_catchup_run` above already retried once this call; if
            // it is still blocked, this is magi's own admission it could
            // not get a command to run, never a verdict on the patch — the
            // run is left exactly where a later reentry can retry again.
            E2eStatus::ResourceBlocked => {
                self.state.event(
                    "review",
                    format!(
                        "{why}; e2e could not run (shared build cache unavailable); not \
                         deciding yet"
                    ),
                );
            }
            E2eStatus::Passed | E2eStatus::Deferred | E2eStatus::NotConfigured => {
                self.state.event(
                    "review",
                    format!("{why}; e2e is green — handing off with {open} finding(s) still open"),
                );
                self.state.status = RunStatus::Gating;
            }
        }
        self.state.save()?;
        Ok(())
    }

    // ----------------------------------------------------------------- gate

    async fn gate(&mut self) -> Result<()> {
        // Judged by the review record itself, not by `status`: a solo
        // candidate's `judge`/`deliberate` skip rewrites `status` on every
        // reentry (see `judge`), and trusting it here is exactly how a run
        // that exhausted its review budget got gated and merged a second
        // time around. `review_conclusion` recomputes the review loop's own
        // verdict from the round records themselves — `Gating` for a clean
        // round or a hand-off (see `stop_reviewing`), anything else means the
        // loop is still going or genuinely blocked.
        // A base the winner could not be replayed onto is a decision, not a
        // round: there is no landing tree to gate. Read as its own record for
        // the same reason the review verdict is.
        if self.state.status == RunStatus::Failed
            || self
                .state
                .base_sync
                .as_ref()
                .is_some_and(|s| s.conflict.is_some())
            || review_conclusion(&self.state.reviews, self.state.config.graph.review_rounds)
                != Some(RunStatus::Gating)
        {
            return Ok(());
        }
        if self.state.gate_ran {
            // `review_loop` derives its conclusion from the clean review
            // record on every reentry and therefore puts a completed run back
            // in `Gating`. A recorded gate is a stronger, terminal fact:
            // retain its original command output (or lack of any, for a repo
            // with no `verify.gate` commands — see `RunState::gate_ran`'s own
            // doc) and restore `Blocked` on a real failure rather than
            // pretending the command is still running or running it a second
            // time. `gate_ran == false` remains the only shape — unattempted,
            // or a resource-blocked retry — that may still need to execute a
            // command.
            if self.state.gate.iter().any(|outcome| !outcome.ok()) {
                self.state.status = RunStatus::Blocked;
                self.state.save()?;
            }
            return Ok(());
        }
        let Some(winner) = self.state.winner().cloned() else {
            return Ok(());
        };
        self.state.status = RunStatus::Gating;
        let shell = self.state.config.shell();
        let gate_commands = self.state.config.verify.gate.clone();
        // Zero commands has nothing to run and nothing that could touch the
        // shared build cache, so it never needs a lease: `Config::cache_dir`
        // is derived from `verify.e2e` too, so a repo with no `verify.gate`
        // commands but a `CARGO_TARGET_DIR`-using `verify.e2e` would
        // otherwise queue behind an unrelated run's lease and come back
        // resource-blocked - `gate_ran` would stay false on nothing but
        // cache contention, for a step that had nothing to check in the
        // first place.
        let outcomes = if gate_commands.is_empty() {
            Vec::new()
        } else {
            let timeout = Duration::from_secs(self.state.config.graph.verify_timeout());
            let cache_dir = self.state.config.cache_dir();
            let head = git::rev_parse(&winner.worktree, "HEAD").await?;
            let (outcomes, _) = with_cache_lease(
                &mut self.state,
                cache_dir.as_deref(),
                "gate",
                "gate",
                &winner.worktree,
                &head,
                timeout,
                "final gate",
                |_state, budget| {
                    let shell = shell.clone();
                    let gate_commands = gate_commands.clone();
                    let worktree = winner.worktree.clone();
                    async move {
                        let (outcomes, timed_out_pids) =
                            run_commands(&shell, &gate_commands, &worktree, budget).await;
                        (outcomes, false, timed_out_pids)
                    }
                },
            )
            .await;
            outcomes
        };
        if outcomes.is_empty() {
            // Nothing configured to check — distinct from every other
            // silence in this run's event log, since an empty `gate` alone
            // no longer says whether the gate ran at all (see
            // `RunState::gate_ran`'s own doc).
            self.state.event(
                "gate",
                "no gate commands configured; nothing to check, passing",
            );
        }
        for o in &outcomes {
            self.state.event(
                "gate",
                format!(
                    "`{}` -> {}",
                    o.command,
                    if o.ok() {
                        "pass".to_owned()
                    } else {
                        format!(
                            "FAIL ({:?})\n{}",
                            o.code,
                            tail(&o.output_tail, EVENT_OUTPUT_TAIL)
                        )
                    }
                ),
            );
        }
        // A resource-blocked outcome means the gate command never actually
        // ran - the shared build cache could not be acquired or confirmed
        // fresh in time - which is evidence about the machine, not about the
        // tree (see `CommandOutcome::resource_blocked`'s own doc). Recording
        // it as a red gate would mark a run `Blocked` on nothing but
        // contention magi has already logged above; leaving `self.state.gate`
        // empty and `self.state.gate_ran` false instead keeps the shape this
        // function already treats as "still needs to run" (see the
        // early-return above), so the next call retries the command rather
        // than concluding anything.
        if verify_inconclusive(&outcomes) {
            self.state.save()?;
            return Ok(());
        }
        let passed = outcomes.iter().all(CommandOutcome::ok);
        self.state.gate = outcomes;
        self.state.gate_ran = true;
        if !passed {
            self.state.status = RunStatus::Blocked;
            self.state.event("gate", "gate failed; not merging");
        }
        self.state.save()?;
        Ok(())
    }

    // ---------------------------------------------------------------- merge

    async fn merge(&mut self) -> Result<()> {
        // Same reasoning as `gate`: ask the review and gate records directly
        // rather than `status`, which a solo-candidate `judge`/`deliberate`
        // skip can rewrite on reentry to something that no longer says
        // `Blocked`. `review_conclusion` is the same derivation `gate` uses,
        // so a hand-off (open findings, green verification) reaches merge
        // exactly like a genuinely clean round does.
        //
        // A run resumed mid-`land` never reaches here at all: `execute`
        // recognises `RunStatus::Landing` before it even calls `prep`, and
        // routes straight to `run_land` instead. That has to happen a level
        // up from this function, not with a check in here, because
        // `review_loop`'s own status recomputation (see its doc) runs
        // *before* `merge` on every reentry and would otherwise overwrite
        // the `Landing` marker with `Gating` before this node ever saw it.
        if self
            .state
            .base_sync
            .as_ref()
            .is_some_and(|s| s.conflict.is_some())
            || review_conclusion(&self.state.reviews, self.state.config.graph.review_rounds)
                != Some(RunStatus::Gating)
            // `gate_ran == false` is not "passed" - `gate` leaves it false
            // both before it has ever run and when its last attempt was
            // resource-blocked (see `Runner::gate`'s own doc), and neither is
            // permission to merge on nothing but the review record. Only a
            // gate that actually ran - zero commands configured and
            // vacuously passed, or one or more that all exited 0 - may
            // proceed; `RunState::gate_status` is the single place that
            // reading is computed.
            || !self.state.gate_status().ok()
        {
            return Ok(());
        }
        // This node's own record, not `status`: `status == Ready` is not
        // unique to the harmless `MergeMode::None` path this line was
        // written for. `land` (below) sets it too, when a `MergeMode::Pr`
        // run's PR was closed without merging — and on that run `mode` is
        // still `Pr`, so a reentry that fell through here would push and
        // open a second pull request. `self.state.merge` is set exactly once
        // this node (or `land`) has already produced a verdict, under every
        // mode, which is what "already done" actually means here.
        if self.state.merge.is_some() {
            return Ok(());
        }
        let Some(winner) = self.state.winner().cloned() else {
            return Ok(());
        };
        let repo = self.state.repo.clone();
        let base = self.state.base_branch.clone();
        let mode = self.state.config.merge.mode;
        let style = self.state.config.merge.style;
        let message = pr_body(&self.state, winner.label);

        let outcome = match mode {
            MergeMode::None => MergeOutcome {
                mode,
                ok: true,
                detail: manual_merge_command(style, &repo, &winner.branch, &message),
            },
            MergeMode::Local => {
                let on = git::current_branch(&repo).await?;
                if on.as_deref() != Some(base.as_str()) {
                    MergeOutcome {
                        mode,
                        ok: false,
                        detail: format!(
                            "{} has {} checked out, not the base branch {base}",
                            repo.display(),
                            on.unwrap_or_else(|| "a detached HEAD".to_owned())
                        ),
                    }
                } else if !git::is_clean(&repo).await? {
                    MergeOutcome {
                        mode,
                        ok: false,
                        detail: format!("{} is dirty; refusing to merge", repo.display()),
                    }
                } else {
                    let out = match style {
                        MergeStyle::Merge => {
                            git::merge_no_ff(&repo, &winner.branch, &message).await?
                        }
                        MergeStyle::Squash => {
                            git::merge_squash(&repo, &winner.branch, &message).await?
                        }
                        MergeStyle::Rebase => git::merge_ff_only(&repo, &winner.branch).await?,
                    };
                    MergeOutcome {
                        mode,
                        ok: out.ok(),
                        detail: if out.ok() { out.stdout } else { out.stderr },
                    }
                }
            }
            MergeMode::Pr => {
                let remote = self.state.config.merge.remote.clone();
                let pushed = git::push(&winner.worktree, &remote, &winner.branch).await?;
                if !pushed.ok() {
                    MergeOutcome {
                        mode,
                        ok: false,
                        detail: pushed.stderr,
                    }
                } else {
                    let out = gh_pr_create(&winner.worktree, &base, &winner.branch, &message).await;
                    match out {
                        Ok(url) => MergeOutcome {
                            mode,
                            ok: true,
                            detail: url,
                        },
                        Err(e) => MergeOutcome {
                            mode,
                            ok: false,
                            detail: e.to_string(),
                        },
                    }
                }
            }
        };

        self.state.status = match (mode, outcome.ok) {
            (MergeMode::None, _) => RunStatus::Ready,
            (_, true) => RunStatus::Merged,
            (_, false) => RunStatus::Blocked,
        };
        self.state.event(
            "merge",
            format!(
                "{:?}: {}",
                mode,
                outcome.detail.lines().next().unwrap_or("")
            ),
        );
        self.state.merge = Some(outcome);
        self.state.save()?;

        // The PR is open and the run would historically stop here, leaving the
        // operator to watch checks, feed review comments back to a fixer, and
        // merge. That was done by hand six times in one session before this
        // existed. Opt-in, because merging is the one irreversible thing magi
        // can do to a repository.
        if self.state.config.graph.land
            && mode == MergeMode::Pr
            && self.state.status == RunStatus::Merged
        {
            self.run_land().await?;
        }
        // `run_land` may have left `status` at `Landing` - still waiting on
        // CI or the owner's approval, not actually settled - so this has to
        // read whatever `status` ended up as here, not the `Merged` this
        // function set a few lines up.
        self.settle_questions();
        Ok(())
    }

    /// Enter `land`.
    ///
    /// Shared between a fresh run's first pass through [`Runner::merge`] and
    /// a resumed run's re-entry. `land::land` itself is what serialises the
    /// two git-mutating moments inside the loop — the rebase push and
    /// `gh pr merge` — per repository (see its own doc); nothing here needs
    /// to hold a lock across the whole call, and doing so would serialise
    /// this run's CI wait against a *different* run's land-approval resume
    /// in the same repository, which is exactly the "must not wait on
    /// another task" property the daemon's slot-freeing exists to give.
    async fn run_land(&mut self) -> Result<()> {
        let url = self
            .state
            .merge
            .as_ref()
            .map(|m| m.detail.clone())
            .unwrap_or_default();
        let url = url.lines().next().unwrap_or("").trim().to_owned();
        if !url.starts_with("http") {
            return Ok(());
        }
        // A land failure is not a lost run: the work is on a branch and the
        // pull request is open, which is exactly where a human takes over.
        match land::land(&mut self.state, &url).await {
            Ok(pr) if self.state.parked => {
                // `land` already saved the parked marker; nothing here
                // overrides `status` back to a terminal value while an
                // approval is still outstanding.
                let _ = pr;
            }
            Ok(pr) => {
                self.state.status = match pr.state {
                    land::PrLifecycle::Merged => RunStatus::Merged,
                    _ => RunStatus::Blocked,
                };
                // Downstream of a confirmed merge only - see
                // `bump::should_release_bump`'s own doc for why this one
                // check covers all three of `land`'s success paths.
                // Best-effort: the run already landed, so a failure here
                // (the decision call, `gh`, `cargo`) is recorded and never
                // turns a landed run into a failed one.
                if bump::should_release_bump(self.state.status)
                    && let Err(e) = bump::after_merge(&mut self.state, &pr.url).await
                {
                    self.state
                        .event("bump", format!("release bump skipped: {e:#}"));
                }
                self.state.save()?;
            }
            Err(e) => {
                self.state.status = RunStatus::Blocked;
                self.state.event("land", format!("gave up: {e}"));
                self.state.save()?;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- helpers

    /// Fetch or create a seat, keeping its conversation across nodes.
    fn seat(&mut self, key: &str, agent: &str) -> SeatState {
        if let Some(existing) = self.state.seats.get(key)
            && existing.agent == agent
        {
            return existing.clone();
        }
        let fresh = SeatState::new(key, agent, self.state.seed);
        self.state.seats.insert(key.to_owned(), fresh.clone());
        fresh
    }

    /// A candidate rendered for judging, with the leak policy applied.
    fn view(&self, c: &Candidate) -> CandidateView {
        let raw = crate::run::read_artifact(&self.state, &format!("cand-{}.patch", c.label))
            .unwrap_or_default();
        let (patch, _) = blind::sanitize_patch(
            &format!("candidate {} patch", c.label),
            &raw,
            &self.state.config.blind,
        );
        CandidateView {
            label: c.label,
            branch: c.branch.clone(),
            summary: c.summary.clone(),
            stat: c.stat.clone(),
            patch,
        }
    }

    /// The full candidate set as prompt text, for seats with no live session.
    fn candidate_block(&self, candidates: &[Candidate], base_short: &str) -> String {
        let views: Vec<CandidateView> = candidates.iter().map(|c| self.view(c)).collect();
        prompt::judge(
            "(see above)",
            &views,
            self.roles.judges.len(),
            base_short,
            "en",
        )
    }

    /// Anonymised transcript for judge `self_idx`.
    ///
    /// The initial rankings are always the opening statements. Seeding them
    /// only when no turn had been taken yet meant every judge after the first
    /// argued against a single voice instead of against the actual split — the
    /// disagreement is the information, so it is always on the table.
    fn transcript(&self, current: &[DeliberationTurn], self_idx: usize) -> Vec<Turn> {
        let mut turns = Vec::new();
        for j in &self.state.judgements {
            if j.ranking.is_empty() {
                continue;
            }
            let reasons = j
                .reasons
                .iter()
                .map(|(k, v)| format!("- {k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n");
            turns.push(Turn {
                who: format!("Judge {} (opening ranking)", j.judge),
                is_self: j.judge == self_idx + 1,
                body: format!(
                    "Ranked {}{}{reasons}",
                    j.ranking.iter().collect::<String>(),
                    if reasons.is_empty() {
                        ""
                    } else {
                        ", because:\n"
                    }
                ),
            });
        }
        for t in self
            .state
            .deliberation
            .iter()
            .flat_map(|r| r.turns.iter())
            .chain(current)
        {
            turns.push(Turn {
                who: format!("Judge {}", t.judge),
                is_self: t.judge == self_idx + 1,
                body: t.body.clone(),
            });
        }
        turns
    }
}

/// Does this seat still hold the context a follow-up prompt would rely on?
fn has_context(spec: &AgentSpec, seat: &SeatState, sessions: bool) -> bool {
    agent::has_session(spec.kind, seat, sessions)
}

/// The next entry in `roster` after `start`, never wrapping back to the
/// front, whose id is not in `tried` yet.
///
/// Starts one past `start` rather than at the front of `roster`: `start` is
/// the seat's own original position, and a seat whose candidate slot already
/// sits on the roster's second entry must fall through to the third next, not
/// restart at the first — which is very likely a different candidate's own
/// agent already. Never wraps back past `start`, for the same reason: an
/// entry earlier in the roster than the seat's own position is almost
/// certainly some *other* candidate slot's own agent, and once the tail of
/// the roster is exhausted there are no more untried agents for *this* seat
/// to fall through to — the caller's fallback chain ends there, exactly as
/// "no further untried agents remain in the list for that seat" asks for.
///
/// Matched by [`AgentSpec::id`], never the whole spec: a roster that names
/// the same id twice (an operator's `roles.implementers` typo, or a
/// `[[agents]]` list reused across roles) must not let
/// [`Runner::resume_quota_losses`] retry that id forever — one forward pass
/// over `roster` either finds an untried id or runs out, so this always
/// terminates regardless of duplicates.
fn next_untried_implementer<'a>(
    roster: &'a [AgentSpec],
    start: usize,
    tried: &BTreeSet<String>,
) -> Option<&'a AgentSpec> {
    roster
        .get(start + 1..)?
        .iter()
        .find(|s| !tried.contains(&s.id))
}

/// Did this reply report running a command whose own CLI never confirmed an
/// exit status?
///
/// An [`agent::CommandEvidence`] only ever exists when the CLI reported the
/// command *finished* (see that type's own doc), so this can only be `true`
/// for a command whose completion event carried no readable exit code — not
/// for one that simply is not mentioned at all. That is the one signal this
/// crate can read, from the same record `state.jobs` renders, about a reply
/// standing next to work its own CLI cannot vouch for finishing; it is
/// deliberately not a check on the exit code's *value* (a fixer legitimately
/// runs a command that fails mid-iteration before it succeeds) and not a
/// guess at a command still running in the background (which emits no event
/// at all, and so leaves no evidence here to find).
fn has_unconfirmed_command(commands: &[agent::CommandEvidence]) -> bool {
    commands.iter().any(|c| c.exit_code.is_none())
}

/// Whether a `NO CHANGE NEEDED` marker in an implementer's reply should be
/// trusted as a verified no-op — the adoption guard's own text-level half.
///
/// `usable` is the caller's `AgentOutput::usable()` (a clean CLI exit, not
/// timed out): a marker only earns the benefit of the doubt from a turn the
/// CLI itself vouches for finishing properly, the same house style
/// `resume_unconfirmed_commands` and `continue_fix_report` already hold a
/// *fix* report to for `commands`. A candidate that timed out, exited
/// non-zero, or left a command unconfirmed is read as the ordinary loss it
/// is, whatever prose it wrote — this returns `None` before it ever looks at
/// `text`. The remaining guards (the tree really is empty, the evidence is
/// non-empty) are the caller's: this only reads what the reply *claimed*.
fn verified_noop_claim(
    usable: bool,
    commands: &[agent::CommandEvidence],
    text: &str,
) -> Option<String> {
    (usable && !has_unconfirmed_command(commands))
        .then(|| verdict::verified_noop(text))
        .flatten()
}

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// What every seat in one batch shares: where the answers are attributed, the
/// prompt overlay they inherit, and the build cache they are told to use.
///
/// A struct rather than four more parameters: `wave` also needs the run's
/// state (to record who is answering right now) and the attempt number, and
/// eight positional arguments is both unreadable and a clippy error.
struct WaveCtx<'a> {
    /// Exported as `MAGI_RUN`, so a task an agent files names the run that
    /// paid for it.
    run: &'a str,
    /// Exported as `MAGI_NODE`, and the key the prompt overlay is chosen by.
    node: &'a str,
    prompts: &'a Prompts,
    /// The shared `CARGO_TARGET_DIR`, when the config declares one.
    cache: Option<&'a Path>,
    /// The review round this wave belongs to, for `"review"`/`"fix"` — see
    /// `JobRecord::round`. `None` for every other node.
    round: Option<usize>,
}

/// Run one job, honouring the parallelism budget.
async fn run_one(
    job: SeatJob,
    sem: Arc<Semaphore>,
    ctx: &WaveCtx<'_>,
    state: &mut RunState,
    attempt: usize,
) -> (SeatState, AgentOutcome) {
    let (_, seat, out) = wave(vec![job], sem, ctx, state, attempt)
        .await
        .pop()
        .expect("one job in, one result out");
    (seat, out)
}

/// Run every job concurrently, capped by the semaphore, preserving order.
///
/// Every seat in the batch is recorded into [`RunState::active`] before the
/// wave starts and cleared as each answer lands, so the run's own record says
/// who is still being waited on rather than only who finished.
async fn wave(
    jobs: Vec<SeatJob>,
    sem: Arc<Semaphore>,
    ctx: &WaveCtx<'_>,
    state: &mut RunState,
    attempt: usize,
) -> Vec<(usize, SeatState, AgentOutcome)> {
    let WaveCtx {
        run,
        node,
        prompts,
        cache,
        round,
    } = *ctx;
    for job in &jobs {
        state.seat_started(node, &job.seat.key, job.timeout, attempt);
    }
    if let Err(e) = state.save() {
        // A failed persist of "who is answering right now" must not abort the
        // wave: the seats are already being asked, and the alternative is
        // losing the answers to save a status line nobody may even be
        // watching.
        tracing::warn!("could not persist in-progress seats: {e:#}");
    }
    // Hold the shared build cache's lease for the whole batch, not per job:
    // several candidates (an implement wave) or a fixer legitimately share
    // one cache concurrently within this run, and that stays untouched — a
    // single lease taken once for the whole wave and released once it is
    // done is what stops a *different* borrower (another run's own wave, its
    // e2e/gate, a human's `magi review`) from interleaving a build into the
    // same directory while this one is in flight. Best-effort, not
    // all-or-nothing: a wave that cannot get the lease within its own
    // longest job's budget still runs — an hour of paid implementer calls is
    // not thrown away over cache contention — but every write-allowed seat
    // then goes without `CARGO_TARGET_DIR` for this wave too (see the filter
    // below), the same fallback a read-only seat always gets, rather than
    // building into a directory this run was never granted. The identity
    // record is still invalidated below either way, so the next tracked
    // caller (`e2e`/`gate`) never trusts a match it cannot vouch for.
    let jobs_had_a_writer = jobs.iter().any(|j| j.allow_write);
    let wait_started = Instant::now();
    let cache_guard = if let Some(cache_dir) = cache {
        if jobs_had_a_writer {
            let owner = crate::cache::Owner::here(run, node, "*", Path::new("(wave)"), "");
            let budget = jobs
                .iter()
                .map(|j| j.timeout)
                .max()
                .unwrap_or(Duration::from_secs(60));
            acquire_cache_lease(state, cache_dir, &owner, budget, node)
                .await
                .ok()
        } else {
            None
        }
    } else {
        None
    };
    // Carved out of each job's own budget, not added on top of it: a seat
    // that waited behind the lease must not also get its full timeout
    // afterward, or a run contended on the cache could double the time it
    // spends per wave. `saturating_sub` floors at zero rather than
    // wrapping - a job whose whole budget was spent waiting starts with
    // none left, which is the honest number, not a free minimum.
    let waited_for_lease = wait_started.elapsed();
    let mut set = tokio::task::JoinSet::new();
    let overlay = prompts.overlay(node);
    for (i, mut job) in jobs.into_iter().enumerate() {
        job.timeout = job.timeout.saturating_sub(waited_for_lease);
        job.prompt = prompt::with_overlay(job.prompt, overlay.clone());
        if cache.is_some() {
            job.prompt.push('\n');
            job.prompt
                .push_str(&prompt::build_cache_note(node, job.allow_write));
        }
        let sem = Arc::clone(&sem);
        let run = run.to_owned();
        let node = node.to_owned();
        // A read-only seat is never handed `CARGO_TARGET_DIR` — see
        // `prompt::build_cache_note`'s doc for why setting it anyway is
        // exactly how a sandboxed reviewer's write refusal got reported as a
        // defect in the patch, not a property of its own seat. And a
        // write-allowed one is handed it only when the lease above was
        // actually acquired: a wave that could not get it (`cache_guard` is
        // `None`, see its own comment) must not send seats to build into a
        // directory this run does not hold - that is the exact concurrent,
        // unmanaged-write race this module exists to prevent, not something
        // "proceeding anyway" is allowed to reintroduce.
        let cache = cache
            .filter(|_| job.allow_write && cache_guard.is_some())
            .map(Path::to_path_buf);
        set.spawn(async move {
            let _permit = sem.acquire().await;
            let mut seat = job.seat;
            let out = agent::invoke(
                &job.spec,
                &mut seat,
                &Invocation {
                    cwd: &job.cwd,
                    prompt: &job.prompt,
                    timeout: job.timeout,
                    allow_write: job.allow_write,
                    sessions: job.sessions,
                    artifacts: &job.artifacts,
                    stem: &job.stem,
                    run: &run,
                    node: &node,
                    cache_dir: cache.as_deref(),
                    attachments: &[],
                },
            )
            .await;
            let out = match out {
                Ok(o) if o.usable() => AgentOutcome::Ok(o),
                Ok(o) if o.quota_exhausted() => AgentOutcome::Quota(o),
                // Billed work the CLI failed to hand over is not an ordinary
                // failure, but its text is the CLI's raw error JSON, not an
                // answer — `Dropped` keeps it out of `Ok` so a caller cannot
                // read it as one by forgetting to check. `usable()` is always
                // false here (dropped implies an empty response), so this has
                // to be checked before the catch-all `Failed` below or the
                // one shape this exists for is lost with the rest.
                Ok(o) if o.work_undelivered() => AgentOutcome::Dropped(o),
                Ok(o) if o.timed_out => AgentOutcome::Failed("timed out".to_owned()),
                Ok(o) => AgentOutcome::Failed(format!(
                    "exited with {:?} and no usable output",
                    o.exit_code
                )),
                Err(e) => AgentOutcome::Failed(e.to_string()),
            };
            (i, seat, out)
        });
    }
    let mut collected: Vec<Option<(usize, SeatState, AgentOutcome)>> = Vec::new();
    while let Some(joined) = set.join_next().await {
        let (i, seat, out) = match joined {
            Ok(v) => v,
            // No seat to clear: a panicked task never reported which one it
            // was. The defensive sweep below this loop is what stops that
            // seat's `active` entry from surviving forever.
            Err(e) => {
                tracing::error!("agent task panicked: {e}");
                continue;
            }
        };
        state.seat_finished(&seat.key);
        record_jobs(state, node, round, &seat.key, &out);
        if let Err(e) = state.save() {
            tracing::warn!("could not persist a seat's completion: {e:#}");
        }
        if collected.len() <= i {
            collected.resize_with(i + 1, || None);
        }
        collected[i] = Some((i, seat, out));
    }
    // Belt-and-braces for the panic branch above: every seat this exact batch
    // started shares this `(node, attempt)` pair, and every seat that finished
    // normally already cleared itself, so anything left tagged with it here
    // can only be a panicked task's leftover. Cleared unconditionally rather
    // than left to read as still answering forever.
    if state
        .active
        .values()
        .any(|a| a.node == node && a.attempt == attempt)
    {
        state
            .active
            .retain(|_, a| !(a.node == node && a.attempt == attempt));
        if let Err(e) = state.save() {
            tracing::warn!("could not persist the end of a wave: {e:#}");
        }
    }
    // Whether or not the lease above was actually held, several worktrees
    // may just have built into the cache with nothing here able to name one
    // coherent (worktree, head) for it - see `cache::invalidate_identity`'s
    // own doc. Forgetting the old record costs the next `e2e`/`gate` one
    // clean it might not have strictly needed; trusting a stale match would
    // cost it a wrong answer.
    if let Some(cache_dir) = cache
        && jobs_had_a_writer
    {
        crate::cache::invalidate_identity(&crate::run::home(), cache_dir);
    }
    if let Some(guard) = cache_guard {
        guard.release();
    }
    collected.into_iter().flatten().collect()
}

/// Fold one seat's [`agent::CommandEvidence`] (if its outcome carries any)
/// into the run's [`JobRecord`] log — every node, every seat, uniformly:
/// this is data collection, not the fix-specific completion contract in
/// [`Runner::continue_fix_report`], and applies regardless of which node
/// asked.
///
/// Only `AgentOutcome::Ok`/`Quota`/`Dropped` carry an [`AgentOutput`] to read
/// evidence from; `Failed` does not, and correctly contributes nothing — a
/// timeout or crash is not itself evidence about a command the seat may have
/// started.
fn record_jobs(
    state: &mut RunState,
    node: &str,
    round: Option<usize>,
    seat: &str,
    out: &AgentOutcome,
) {
    let commands: &[agent::CommandEvidence] = match out {
        AgentOutcome::Ok(o) | AgentOutcome::Quota(o) | AgentOutcome::Dropped(o) => &o.commands,
        AgentOutcome::Failed(_) => &[],
    };
    let checked_at = Timestamp::now();
    for c in commands {
        state.jobs.push(JobRecord {
            node: node.to_owned(),
            round,
            seat: seat.to_owned(),
            id: c.id.clone(),
            description: c.description.clone(),
            checked_at,
            status: match c.exit_code {
                Some(0) => JobStatus::Completed,
                Some(_) => JobStatus::Failed,
                None => JobStatus::Unknown,
            },
            exit_code: c.exit_code,
            result_summary: c.result_summary.clone(),
            source: c.source.clone(),
        });
    }
}

/// Is a review round clean, given how many reviewer seats answered against
/// how many the round expected?
///
/// A seat that never answered (timeout, crash, unparsable output) is not a
/// seat that read the patch and found nothing — treating it as such is
/// exactly the bug this function exists to close. Under the default `block`
/// policy a missing seat can never be clean; `warn` still requires the seats
/// that *did* answer to have found nothing blocking and verification to be
/// green.
///
/// `quota_missing` narrows that `block` default for exactly one cause of
/// absence: a seat lost to its own rate limit this round. Re-reviewing hoping
/// a session limit lifts by the very next round buys nothing — the seat is
/// asked again with the same quota — so once every missing seat is accounted
/// for by a quota loss (and at least one seat *did* answer, so a decision has
/// something to rest on) the round is decided on the panel that could answer,
/// same as `warn` would. A panel that lost every seat to quota is not
/// decided here: `answered == 0` falls through to the existing `block`
/// fallback so a fully collapsed panel still waits rather than landing on no
/// review at all.
fn round_is_clean(
    blocking: usize,
    e2e_ok: bool,
    answered: usize,
    expected: usize,
    quota_missing: usize,
    policy: IncompleteReviewPolicy,
) -> bool {
    if blocking != 0 || !e2e_ok {
        return false;
    }
    if answered == expected || policy == IncompleteReviewPolicy::Warn {
        return true;
    }
    answered > 0 && expected - answered <= quota_missing
}

/// The review loop's own conclusion, derived entirely from its persisted
/// round records and the round budget that produced them — never from
/// `status`, so a reentry (or `gate`/`merge` reading it independently)
/// recomputes the identical answer regardless of what an earlier node in the
/// same walk, or a previous walk, did to `status`.
///
/// `None` while more rounds remain to try, including when review never ran
/// at all (`review_rounds = 0`, or nothing yet recorded). Once a round has
/// gone clean, or the budget is spent, or the tree has stopped moving (see
/// [`STAGNANT_LIMIT`]), the answer is one of two things:
///
/// - An incomplete panel that raised nothing is missing input, not a
///   verified tree — never a hand-off candidate, whatever verification said
///   (see [`ReviewRound::incomplete`], `IncompleteReviewPolicy`).
/// - Otherwise, green e2e on the last round hands off (see
///   [`Runner::stop_reviewing`]); red e2e blocks.
///
/// A last round whose own verification is still `ResourceBlocked` — magi
/// itself never got a command to run, not evidence the patch is broken —
/// is neither: this returns `None` for it too, the same as "more rounds
/// remain", so a reentry retries the check (see `Runner::review_loop`'s own
/// handling of that shape) instead of this cheap recomputation guessing a
/// verdict a real attempt never produced.
fn review_conclusion(reviews: &[ReviewRound], max_rounds: usize) -> Option<RunStatus> {
    if max_rounds == 0 || reviews.iter().any(|r| r.clean) {
        return Some(RunStatus::Gating);
    }
    let last = reviews.last()?;
    let stagnant = reviews.iter().rev().take_while(|r| !r.progressed).count() >= STAGNANT_LIMIT;
    if reviews.len() < max_rounds && !stagnant {
        return None;
    }
    if last.incomplete() && last.blocking == 0 {
        return Some(RunStatus::Blocked);
    }
    if last.e2e_status() == E2eStatus::ResourceBlocked {
        return None;
    }
    Some(if last.e2e.iter().all(CommandOutcome::ok) {
        RunStatus::Gating
    } else {
        RunStatus::Blocked
    })
}

/// How long a re-ask may take, given the budget the first attempt had.
///
/// A `nudged` retry is a request to restate an answer the seat has already
/// worked out: it carries no new work, so it does not deserve the original
/// budget. Measured on run 01c2, two judges restated their ranking in 41 and
/// 133 seconds while a third sat for over ten minutes on a resumed session
/// holding 410 KB of prior output - and because the retry had inherited the
/// full 1200s judge timeout, one stuck nudge nearly doubled the wall time of a
/// judging round whose other seats were long finished.
///
/// A quarter of the budget, with a floor so that a deliberately short timeout
/// does not collapse to nothing. A retry that re-sends the whole prompt
/// (because the seat kept no context) is the original job again, and keeps the
/// original budget.
fn retry_budget(full: Duration, nudged: bool) -> Duration {
    if nudged {
        (full / 4).max(Duration::from_secs(120)).min(full)
    } else {
        full
    }
}

/// Run a wave and parse each reply, re-asking the seats whose reply was
/// unusable.
///
/// The re-ask is a nudge rather than the whole prompt again when the seat still
/// holds its conversation, which is the difference between a cheap retry and
/// paying for the entire candidate set twice.
///
/// A seat that hits a rate limit is **not** re-asked: the same call will fail
/// the same way until the limit resets, so spending a retry attempt on it is
/// pure waste. Its loss is recorded in `losses` and it is returned as a failure
/// like any other absent seat — the caller decides whether the panel still has
/// a quorum.
#[allow(clippy::too_many_arguments)]
async fn ask_json_wave<T>(
    jobs: Vec<SeatJob>,
    sem: Arc<Semaphore>,
    retries: usize,
    ctx: &WaveCtx<'_>,
    losses: &mut Vec<QuotaLoss>,
    state: &mut RunState,
    validate: &(dyn Fn(&T) -> Result<()> + Send + Sync),
) -> Vec<(SeatState, Result<(T, AgentOutput)>)>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    let n = jobs.len();
    let originals: Vec<SeatJob> = jobs;
    let mut seats: Vec<SeatState> = originals.iter().map(|j| j.seat.clone()).collect();
    let mut done: Vec<Option<Result<(T, AgentOutput)>>> = (0..n).map(|_| None).collect();
    let mut pending: Vec<usize> = (0..n).collect();

    for attempt in 0..=retries {
        if pending.is_empty() {
            break;
        }
        let mut batch = Vec::with_capacity(pending.len());
        for &i in &pending {
            let src = &originals[i];
            // The prompt and the budget are one decision: a nudge restates
            // finished work, a re-sent prompt redoes it.
            let (prompt, timeout) = if attempt == 0 {
                (src.prompt.clone(), src.timeout)
            } else {
                let why = done[i]
                    .as_ref()
                    .and_then(|r| r.as_ref().err().map(ToString::to_string))
                    .unwrap_or_else(|| "no parsable answer".to_owned());
                let nudge = prompt::nudge(&why);
                let nudged = has_context(&src.spec, &seats[i], src.sessions);
                let prompt = if nudged {
                    nudge
                } else {
                    format!("{}\n\n---\n\n{}", src.prompt, nudge)
                };
                (prompt, retry_budget(src.timeout, nudged))
            };
            batch.push(SeatJob {
                spec: src.spec.clone(),
                seat: seats[i].clone(),
                cwd: src.cwd.clone(),
                prompt,
                timeout,
                allow_write: src.allow_write,
                sessions: src.sessions,
                artifacts: src.artifacts.clone(),
                stem: if attempt == 0 {
                    src.stem.clone()
                } else {
                    format!("{}-retry{attempt}", src.stem)
                },
            });
        }

        if attempt > 0 {
            let seats_out: Vec<&str> = pending
                .iter()
                .map(|&i| originals[i].seat.key.as_str())
                .collect();
            state.event(
                ctx.node,
                format!("retry {attempt}: re-asking {}", seats_out.join(", ")),
            );
        }
        let results = wave(batch, Arc::clone(&sem), ctx, state, attempt).await;
        let mut still = Vec::new();
        for (&i, (_wi, seat, out)) in pending.iter().zip(results) {
            seats[i] = seat;
            let (parsed, quota) = match out {
                AgentOutcome::Ok(o) => (
                    match verdict::extract_json::<T>(&o.text) {
                        Ok(v) => match validate(&v) {
                            Ok(()) => Ok((v, o)),
                            Err(e) => Err(e),
                        },
                        Err(e) => Err(e),
                    },
                    false,
                ),
                AgentOutcome::Quota(o) => {
                    losses.push(QuotaLoss {
                        seat: originals[i].seat.key.clone(),
                        node: ctx.node.to_owned(),
                        at: Timestamp::now(),
                        reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                    });
                    (
                        Err(anyhow::anyhow!("rate limited (quota); not retrying now")),
                        true,
                    )
                }
                // Not a parseable answer, but also not worth a special-cased
                // retry here: the nudge loop above already re-asks anything
                // that fails to parse, which is exactly what a dropped stream
                // needs. Just don't hand its raw error JSON to `extract_json`.
                AgentOutcome::Dropped(o) => {
                    let why = o
                        .dropped
                        .as_ref()
                        .map(|d| d.why.as_str())
                        .unwrap_or("the CLI ended the stream without delivering its answer");
                    (
                        Err(anyhow::anyhow!("the CLI dropped the stream ({why})")),
                        false,
                    )
                }
                AgentOutcome::Failed(e) => (Err(anyhow::anyhow!(e)), false),
            };
            let failed = parsed.is_err();
            done[i] = Some(parsed);
            // Do not re-ask a rate-limited seat (quota) — a retry is known to
            // fail the same way; and never re-ask a seat that already parsed.
            if failed && !quota {
                still.push(i);
            }
        }
        pending = still;
    }

    seats
        .into_iter()
        .zip(done)
        .map(|(seat, res)| {
            (
                seat,
                res.unwrap_or_else(|| Err(anyhow::anyhow!("no attempt was made"))),
            )
        })
        .collect()
}

/// Acquire the shared build cache's lease, waiting out contention within
/// `budget` (never past it — see AGENTS.md's build-cache section on why an
/// unbounded wait is never acceptable).
///
/// A first, non-blocking check happens before ever waiting; if it finds the
/// lease busy, that fact is logged as a `verify` event *and* flushed with
/// [`RunState::save`] immediately — not only once the wait finally succeeds
/// or gives up — so a `magi show` run by a different process while this one
/// is still waiting reads a `run.json` that says so, rather than whatever it
/// looked like before the wait started. The same applies to the terminal
/// failure: logged and saved before this returns `Err`, so a caller that
/// could not get the lease at all still leaves a legible record of why.
async fn acquire_cache_lease(
    state: &mut RunState,
    cache_dir: &Path,
    owner: &crate::cache::Owner,
    budget: Duration,
    context: &str,
) -> Result<crate::cache::Guard> {
    let home = crate::run::home();
    let started = Instant::now();
    let busy = match crate::cache::try_acquire(&home, cache_dir, owner) {
        Ok(crate::cache::AcquireOutcome::Acquired(g)) => return Ok(g),
        Ok(crate::cache::AcquireOutcome::Busy(busy)) => busy,
        Err(e) => {
            state.event(
                "verify",
                format!("{context}: could not check the shared build cache: {e:#}"),
            );
            if let Err(e2) = state.save() {
                tracing::warn!("could not persist a cache-check failure: {e2:#}");
            }
            return Err(e);
        }
    };
    state.event(
        "verify",
        format!(
            "{context}: waiting for the shared build cache at {} ({})",
            cache_dir.display(),
            busy.describe()
        ),
    );
    if let Err(e) = state.save() {
        tracing::warn!("could not persist a cache wait: {e:#}");
    }
    let remaining = budget.saturating_sub(started.elapsed());
    match crate::cache::wait_for(&home, cache_dir, owner, remaining, Duration::from_secs(5)).await {
        Ok(g) => Ok(g),
        Err(e) => {
            state.event("verify", format!("{context}: {e:#}"));
            if let Err(e2) = state.save() {
                tracing::warn!("could not persist a cache wait timeout: {e2:#}");
            }
            Err(e)
        }
    }
}

/// Run `body` — a verify command batch — while holding the shared build
/// cache's lease, so this run's own full verification (`e2e`, `gate`) can
/// never interleave with another borrower's build against the same
/// `CARGO_TARGET_DIR`: a different run, a lingering reviewer past its
/// timeout, or a human's own `magi review`. See the `cache` module doc for
/// why this matters more than Cargo's own per-target locking covers — two
/// *different* worktrees building the same package name/version into one
/// cache directory is a staleness bug, not a lock contention one.
///
/// The wait for the lease is carved out of `budget`, never on top of it —
/// `body` is handed whatever is left, so a caller's own node timeout is the
/// only clock involved, exactly what AGENTS.md's build-cache section asks
/// for ("never an unbounded wait"). When `cache_dir` is `None` — no shared
/// cache configured at all — this is a pass-through: `body` runs with the
/// full budget and nothing is leased.
///
/// A lease that cannot be acquired within `budget` is reported as a single
/// synthetic [`CommandOutcome`] (`code: None`) rather than silently skipping
/// verification — the same shape a spawn failure already takes in
/// [`run_commands`], so a caller need not special-case it.
#[allow(clippy::too_many_arguments)]
async fn with_cache_lease<'s, F, Fut>(
    state: &'s mut RunState,
    cache_dir: Option<&Path>,
    node: &str,
    seat: &str,
    worktree: &Path,
    head: &str,
    budget: Duration,
    context: &str,
    body: F,
) -> (Vec<CommandOutcome>, bool)
where
    F: FnOnce(&'s mut RunState, Duration) -> Fut,
    Fut: std::future::Future<Output = (Vec<CommandOutcome>, bool, Vec<u32>)>,
{
    let Some(cache_dir) = cache_dir else {
        let (outcomes, retried, _timed_out_pids) = body(state, budget).await;
        return (outcomes, retried);
    };
    let home = crate::run::home();
    let owner = crate::cache::Owner::here(&state.id, node, seat, worktree, head);
    let started = Instant::now();
    let guard = match acquire_cache_lease(state, cache_dir, &owner, budget, context).await {
        Ok(g) => g,
        Err(e) => {
            return (
                vec![CommandOutcome {
                    command: "(waiting for the shared build cache)".to_owned(),
                    code: None,
                    output_tail: e.to_string(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    resource_blocked: true,
                }],
                false,
            );
        }
    };
    let identity = crate::cache::Identity::new(worktree, head);
    if let Err(e) = crate::cache::ensure_fresh(&home, cache_dir, &identity) {
        // A failed freshness check means this process cannot vouch for what
        // is sitting in the cache right now - on Windows this is exactly the
        // "a stale test executable is still locked, `cargo clean -p` cannot
        // remove it" case the evidence log records. Running verify anyway
        // and reporting whatever it says would let a result nobody can trust
        // stand for the tree it claims to have checked; fail the step
        // instead of the patch.
        state.event(
            "verify",
            format!(
                "{context}: could not confirm the shared build cache matches {} at {}: {e:#}",
                worktree.display(),
                short(head)
            ),
        );
        guard.release();
        return (
            vec![CommandOutcome {
                command: "(confirming the shared build cache is fresh)".to_owned(),
                code: None,
                output_tail: e.to_string(),
                duration_ms: started.elapsed().as_millis() as u64,
                resource_blocked: true,
            }],
            false,
        );
    }
    let remaining = budget.saturating_sub(started.elapsed());
    let (outcomes, retried, timed_out_pids) = body(state, remaining).await;
    // A timed-out command's process was only *asked* to die (`kill_on_drop`,
    // `start_kill`); confirm it actually has before handing the directory to
    // the next acquirer. See `wait_for_timed_out_children_to_die`'s own doc
    // for what this can and cannot see.
    if !timed_out_pids.is_empty() {
        wait_for_timed_out_children_to_die(&timed_out_pids).await;
    }
    guard.release();
    (outcomes, retried)
}

/// Poll `pids` — commands [`run_commands`] reports as still running when its
/// own timeout elapsed — until every one is confirmed gone, or
/// [`LEASE_RELEASE_MAX_WAIT`] passes, whichever comes first.
///
/// Real confirmation where confirmation is possible, not a substitute for
/// full process-tree observation: a grandchild the timed-out process spawned
/// and that survives independently of it is invisible to a pid check the
/// same way it always was, and continuing to observe and collect *that*
/// stays a different piece of work with its own owner. This only narrows a
/// fixed blind wait into an actual check of the pids this process does know
/// about.
async fn wait_for_timed_out_children_to_die(pids: &[u32]) {
    wait_for_pids_with(
        pids,
        crate::proc::pid_alive,
        LEASE_RELEASE_POLL,
        LEASE_RELEASE_MAX_WAIT,
    )
    .await;
}

/// [`wait_for_timed_out_children_to_die`] with its liveness query, poll
/// interval and ceiling supplied by the caller, so the polling *logic* -
/// returns as soon as every pid reports dead, gives up at the ceiling
/// otherwise - is testable on millisecond durations without asking the real
/// OS about a pid at all.
async fn wait_for_pids_with<F: Fn(u32) -> bool>(
    pids: &[u32],
    alive: F,
    poll: Duration,
    max_wait: Duration,
) {
    let deadline = Instant::now() + max_wait;
    loop {
        if pids.iter().all(|&pid| !alive(pid)) {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Are any of `outcomes` [`CommandOutcome::resource_blocked`] - magi's own
/// admission that it could not even get a verify command to run, as opposed
/// to evidence the command actually produced? A caller that would otherwise
/// read a resource-blocked outcome as a red command must check this first:
/// see [`Runner::gate`], which retries rather than records `Blocked` when
/// this is true.
fn verify_inconclusive(outcomes: &[CommandOutcome]) -> bool {
    outcomes.iter().any(|o| o.resource_blocked)
}

/// Describe one verify command's outcome for the event log, distinguishing a
/// build/link failure — the toolchain never produced a binary to run — from
/// an actual test failure, since only the latter is a verdict on the patch.
fn e2e_outcome_label(o: &CommandOutcome) -> String {
    if o.ok() {
        return "pass".to_owned();
    }
    let reason = if o.build_failed() {
        format!("COULD NOT RUN ({:?}, build/link failure)", o.code)
    } else {
        format!("FAIL ({:?})", o.code)
    };
    format!("{reason}\n{}", tail(&o.output_tail, EVENT_OUTPUT_TAIL))
}

/// Run `verify.e2e`, retrying once if the first attempt could not build or
/// link — a build/link failure is frequently a race against a shared
/// `CARGO_TARGET_DIR` (see AGENTS.md), not a verdict on the patch. Emits one
/// `verify` event per command, tagged with `context` (normally `"round N"`)
/// so the two call sites that need this — the ordinary per-round leg in
/// `review_loop`, and the deferred catch-up run `stop_reviewing` makes before
/// it will ever call a round green — read identically in the event log.
async fn run_e2e_with_retry(
    state: &mut RunState,
    shell: &[String],
    commands: &[String],
    worktree: &Path,
    timeout: Duration,
    context: &str,
) -> (Vec<CommandOutcome>, bool, Vec<u32>) {
    let (mut e2e, mut timed_out_pids) = run_commands(shell, commands, worktree, timeout).await;
    for o in &e2e {
        state.event(
            "verify",
            format!("{context}: `{}` -> {}", o.command, e2e_outcome_label(o)),
        );
    }
    // A build/link failure is not a verdict on the patch — it is frequently a
    // race against a shared `CARGO_TARGET_DIR` (see AGENTS.md). Give verify
    // one retry before letting a red like that decide the round.
    let verify_retried = e2e.iter().any(CommandOutcome::build_failed);
    if verify_retried {
        state.event(
            "verify",
            format!(
                "{context}: verify could not build/link, not a test result — retrying once \
                 before concluding"
            ),
        );
        let retried = run_commands(shell, commands, worktree, timeout).await;
        e2e = retried.0;
        // Both attempts' timeouts matter, not just the last one: the first
        // attempt's descendants may still be alive alongside the retry's.
        timed_out_pids.extend(retried.1);
        for o in &e2e {
            state.event(
                "verify",
                format!(
                    "{context}: retry `{}` -> {}",
                    o.command,
                    e2e_outcome_label(o)
                ),
            );
        }
    }
    (e2e, verify_retried, timed_out_pids)
}

/// Run configured shell commands in `cwd`, in order. The second element is
/// the pid of every command that hit `timeout` and was still running when
/// this stopped waiting on it (best-effort: `None` when the platform did not
/// hand one back) — see [`with_cache_lease`]'s use of it for why a caller
/// that releases a shared resource afterward needs to know.
async fn run_commands(
    shell: &[String],
    commands: &[String],
    cwd: &Path,
    timeout: Duration,
) -> (Vec<CommandOutcome>, Vec<u32>) {
    let mut out = Vec::new();
    let mut timed_out_pids = Vec::new();
    for command in commands {
        let started = Instant::now();
        let mut cmd = tokio::process::Command::new(&shell[0]);
        cmd.quiet();
        cmd.args(&shell[1..])
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let spawned = cmd.spawn();
        let (code, body) = match spawned {
            Ok(child) => {
                // Captured before the child is consumed below: `kill_on_drop`
                // only *asks* the process to die when the timeout branch
                // drops it, and the pid is the only way anyone downstream can
                // later check whether that request actually took.
                let pid = child.id();
                match tokio::time::timeout(timeout, child.wait_with_output()).await {
                    Ok(Ok(o)) => {
                        let mut body = String::from_utf8_lossy(&o.stdout).into_owned();
                        body.push_str(&String::from_utf8_lossy(&o.stderr));
                        (o.status.code(), body)
                    }
                    Ok(Err(e)) => (None, format!("failed to run: {e}")),
                    Err(_) => {
                        if let Some(pid) = pid {
                            timed_out_pids.push(pid);
                        }
                        (None, format!("timed out after {}s", timeout.as_secs()))
                    }
                }
            }
            Err(e) => (None, format!("failed to spawn `{}`: {e}", shell[0])),
        };
        out.push(CommandOutcome {
            command: command.clone(),
            code,
            output_tail: tail(&body, OUTPUT_TAIL),
            duration_ms: started.elapsed().as_millis() as u64,
            resource_blocked: false,
        });
    }
    (out, timed_out_pids)
}

/// The shell command line `mode = "none"` prints — in `magi show`'s `merge`
/// section (`report::run`) and in the `merge` event this node records — for
/// the operator to run by hand.
///
/// Built from [`MergeStyle`] rather than always `git merge --no-ff`: a base
/// branch whose ruleset forbids merge commits (GitHub's "must not contain
/// merge commits", or "require linear history") rejects the push a `--no-ff`
/// merge would produce, which is exactly the guidance this function replaces.
/// `message`'s first line becomes the squash commit's subject, matching the
/// note `report::run` prints alongside this command — see that function for
/// why an explicit subject is not optional there.
fn manual_merge_command(style: MergeStyle, repo: &Path, branch: &str, message: &str) -> String {
    let repo = repo.display();
    match style {
        MergeStyle::Merge => format!("git -C {repo} merge --no-ff {branch}"),
        MergeStyle::Squash => {
            let subject = message.lines().next().unwrap_or(branch);
            format!(
                "git -C {repo} merge --squash {branch} && git -C {repo} commit -m \"{subject}\""
            )
        }
        MergeStyle::Rebase => format!("git -C {repo} merge --ff-only {branch}"),
    }
}

/// The merge commit / pull request body: the task, and — when the winning
/// review round was not clean — the findings still open and whatever the
/// fixer declined, so `merge = "pr"` hands the reader the same material
/// `magi show` does rather than a pull request that reads clean while
/// `run.json` disagrees.
///
/// The first line doubles as the squash/merge commit subject
/// (`manual_merge_command`), which takes it via `message.lines().next()`
/// verbatim — so it has to be the task's own opening line, not run/candidate
/// bookkeeping. The pull request title (`gh_pr_create`) starts from the same
/// line but is further reshaped and truncated by `pr_title` to stay inside
/// GitHub's limit; see that function for why. "Merge magi run ec12 (candidate
/// B)" told a reader nothing about what landed once the run id had scrolled
/// off the PR list. That bookkeeping still needs to be findable, just not
/// from the title: the branch name already carries it
/// (`RunState::branch_for`), and the footer below repeats it as plain tags
/// for a reader holding only the merged commit or the PR body.
///
/// `state.instruction` can open with blank lines — a `--file` task is passed
/// through verbatim (`task_text` only rejects a body that is blank
/// *entirely*) — and `.lines().next()` on those reads back as `Some("")`, not
/// `None`. `trim_start` drops exactly those leading blank lines so the first
/// line is the task's real opening line, and the empty-after-trim case (a
/// whitespace-only instruction) falls back the same way `queue::title_from`
/// does for the same situation.
fn pr_body(state: &RunState, winner: char) -> String {
    let instruction = state.instruction.trim_start();
    let mut message = if instruction.is_empty() {
        "(empty task)".to_owned()
    } else {
        instruction.to_owned()
    };

    let open = state.open_findings();
    if !open.is_empty() {
        message.push_str("\n\n## Open review findings\n\n");
        for f in &open {
            message.push_str(&format!("- `{}` [{:?}] {}\n", f.id, f.severity, f.title));
        }
    }

    if let Some(fix) = state.reviews.last().and_then(|r| r.fix.as_ref())
        && !fix.rejected.is_empty()
    {
        message.push_str("\n## Declined by the fixer\n\n");
        for r in &fix.rejected {
            message.push_str(&format!("- `{}`: {}\n", r.id, r.why));
        }
    }

    message.push_str(&format!(
        "\n\n---\nmagi:run/{} magi:candidate-{}\n",
        state.id,
        winner.to_ascii_lowercase()
    ));

    message
}

/// GitHub's `createPullRequest` GraphQL mutation, which `gh pr create` calls
/// under the hood, rejects a `title` over 256 characters and the whole
/// command fails — no PR at all, for a run whose body was otherwise fine
/// (this is what happened to run 2963; see AGENTS.md). 240 leaves room below
/// that limit: `title_from` counts `chars()` (Unicode scalars), which is not
/// always how GitHub counts, plus one character for the trailing ellipsis
/// `title_from` may add. It is a margin, not a guarantee — a title packed
/// with multi-unit characters could still in principle land close to the
/// edge, but a real task title's occasional emoji or accented letter fits
/// comfortably inside it.
const PR_TITLE_MAX: usize = 240;

/// The pull request title: the PR body's first line, reshaped and truncated
/// by [`queue::title_from`] the same way `magi show`'s task list titles are,
/// so it stays inside GitHub's limit on `--title` (see [`PR_TITLE_MAX`]).
fn pr_title(body: &str) -> String {
    queue::title_from(body, PR_TITLE_MAX)
}

/// `gh pr create`, returning the PR url.
async fn gh_pr_create(cwd: &Path, base: &str, head: &str, body: &str) -> Result<String> {
    let title = pr_title(body);
    let out = tokio::process::Command::new("gh")
        .args([
            "pr", "create", "--base", base, "--head", head, "--title", &title, "--body", body,
        ])
        .current_dir(cwd)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh")?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Tear a run's worktrees and branches down.
///
/// `home` is where the updated `run.json` is saved (via
/// [`RunState::save_under`]), never the process-global [`crate::run::home`]:
/// a housekeeping pass already has its own honest `home` handed to it, and
/// falling through to the global here would write back through whichever
/// directory some other process or test pinned into that `OnceLock` first,
/// not the one the caller actually resolved its `runs` and `state` from.
pub async fn fold_run(state: &mut RunState, drop_winner: bool, home: &Path) -> Result<Vec<String>> {
    let repo = state.repo.clone();
    let root = state.worktree_root();
    let winner = state.tally.as_ref().map(|t| t.winner);
    let mut removed = Vec::new();

    for i in 0..state.candidates.len() {
        let c = state.candidates[i].clone();
        let is_winner = Some(c.label) == winner;
        if is_winner && !drop_winner {
            continue;
        }
        if c.worktree.exists() {
            git::worktree_remove(&repo, &c.worktree).await.ok();
            removed.push(c.worktree.to_string_lossy().into_owned());
        }
        if git::branch_exists(&repo, &c.branch).await.unwrap_or(false) {
            git::branch_delete(&repo, &c.branch).await.ok();
            removed.push(c.branch.clone());
        }
        state.candidates[i].folded = true;
    }

    for name in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let path = name.path();
        let keep = !drop_winner
            && winner.is_some_and(|w| {
                path.file_name()
                    .is_some_and(|n| n == format!("cand-{w}").as_str())
            });
        if keep {
            continue;
        }
        git::worktree_remove(&repo, &path).await.ok();
        removed.push(path.to_string_lossy().into_owned());
    }

    // `root` (`wt/<...>/<short>/`) held nothing but this run's candidate and
    // judge worktrees, so once the loop above has cleared all of them out,
    // the parent is a bare directory nobody else was ever going to remove -
    // git only ever managed what was inside it. Left alone, one of these
    // accumulates per fully-folded run; the operator's own machine had 74.
    // `remove_if_empty` re-checks rather than assuming: a run whose winner
    // was kept (`!drop_winner`) leaves its directory behind on purpose, and
    // so does anything a run never claimed that happens to share the bay.
    remove_if_empty(&root);

    if state.enabled_worktree_config && drop_winner {
        // A release, not a raw disable: some sibling run in this repository
        // may still hold its own reference (see `git::acquire_worktree_config`),
        // and only the last release actually turns the setting back off.
        git::release_worktree_config(&repo).await.ok();
        state.enabled_worktree_config = false;
    }
    state.save_under(home)?;
    Ok(removed)
}

/// Remove `dir` if it exists and has nothing in it.
///
/// Best-effort and silent by design: a directory that is not empty (a run
/// whose winner is still parked there, a stray file some other process left)
/// is exactly the case this must refuse, and a directory that is already gone
/// is not a failure worth reporting either. `std::fs::remove_dir` itself
/// already refuses a non-empty directory, so the emptiness check below is
/// belt, not suspenders - it is what keeps this from ever attempting the
/// removal in the case that matters, rather than trusting `remove_dir`'s
/// error path to have no side effects if it ever changed.
fn remove_if_empty(dir: &Path) {
    if dir.is_dir() && std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none()) {
        std::fs::remove_dir(dir).ok();
    }
}

/// Severity of the worst open finding in the last review round, for reporting.
pub fn worst_open(state: &RunState) -> Option<Severity> {
    state
        .reviews
        .last()?
        .reviews
        .iter()
        .flat_map(|r| r.findings.iter())
        .map(|f| f.severity)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::GateStatus;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn conductor() -> AgentSpec {
        AgentSpec {
            id: "conductor".to_owned(),
            kind: crate::config::AgentKind::Command,
            model: None,
            command: vec!["true".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }
    }

    fn spec(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            kind: crate::config::AgentKind::Command,
            model: None,
            command: vec!["true".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }
    }

    // `next_untried_implementer` is the property `resume_quota_losses`'s own
    // fallback loop depends on to terminate: it must walk forward from the
    // seat's own position, never restart at the front of the roster, and it
    // must never hand back an id already tried, however many times that id
    // happens to appear.

    #[test]
    fn next_untried_implementer_walks_forward_from_the_seats_own_position() {
        let roster = vec![spec("alpha"), spec("beta"), spec("gamma")];
        let tried = BTreeSet::from(["beta".to_owned()]);
        // beta sits at index 1; the next candidate is gamma, never alpha —
        // which is very likely a different candidate slot's own agent.
        let next = next_untried_implementer(&roster, 1, &tried);
        assert_eq!(next.map(|s| s.id.as_str()), Some("gamma"));
    }

    #[test]
    fn next_untried_implementer_does_not_wrap_back_past_its_own_start() {
        let roster = vec![spec("alpha"), spec("beta")];
        let tried = BTreeSet::from(["beta".to_owned()]);
        // beta is the roster's last entry: nothing follows it, and alpha —
        // earlier in the roster, almost certainly a different candidate
        // slot's own agent — must not be reached by wrapping back to it.
        assert!(next_untried_implementer(&roster, 1, &tried).is_none());
    }

    #[test]
    fn next_untried_implementer_stops_once_the_tail_is_exhausted_even_if_earlier_ids_are_untried() {
        let roster = vec![spec("alpha"), spec("beta"), spec("gamma")];
        let tried = BTreeSet::from(["beta".to_owned(), "gamma".to_owned()]);
        // beta (index 1) and gamma (index 2, the only entry after it) have
        // both been tried; alpha (index 0) never has, but it comes before
        // beta's own position, so there is nothing further for this seat.
        assert!(next_untried_implementer(&roster, 1, &tried).is_none());
    }

    #[test]
    fn next_untried_implementer_skips_ids_already_tried_even_when_duplicated() {
        let roster = vec![spec("a"), spec("a"), spec("b")];
        let tried = BTreeSet::from(["a".to_owned()]);
        let next = next_untried_implementer(&roster, 0, &tried);
        assert_eq!(next.map(|s| s.id.as_str()), Some("b"));
    }

    #[test]
    fn next_untried_implementer_returns_none_once_every_id_is_tried() {
        let roster = vec![spec("a"), spec("b")];
        let tried = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        assert!(next_untried_implementer(&roster, 0, &tried).is_none());
    }

    #[test]
    fn remove_if_empty_only_ever_takes_a_bare_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bay = dir.path().join("ffff");

        // Not there yet: nothing to do, nothing to panic on.
        remove_if_empty(&bay);
        assert!(!bay.exists());

        // Something still inside - the winner's worktree, or a stray file -
        // keeps the directory standing.
        std::fs::create_dir_all(bay.join("cand-A")).unwrap();
        remove_if_empty(&bay);
        assert!(bay.exists(), "non-empty directory must survive");

        // Once the last entry is gone, so is the directory itself.
        std::fs::remove_dir(bay.join("cand-A")).unwrap();
        remove_if_empty(&bay);
        assert!(!bay.exists(), "an empty bay is a leftover, not a record");
    }

    // `round_is_clean` is the exact decision this task fixed: a round with a
    // seat that never answered must not read the same as a round every seat
    // actually reviewed. These are deterministic and process-free by design —
    // the equivalent end-to-end check (a real reviewer timing out under a
    // live graph run) is a genuine race against wall-clock contention, and a
    // spawn slow enough to blow even a generous budget under a loaded test
    // run must not turn this specific regression check flaky.

    #[test]
    fn a_full_panel_that_found_nothing_is_clean() {
        assert!(round_is_clean(
            0,
            true,
            2,
            2,
            0,
            IncompleteReviewPolicy::Block
        ));
    }

    #[test]
    fn a_missing_seat_is_never_clean_under_the_default_policy() {
        assert!(!round_is_clean(
            0,
            true,
            1,
            2,
            0,
            IncompleteReviewPolicy::Block
        ));
    }

    #[test]
    fn warn_policy_still_refuses_a_missing_seat_with_open_findings() {
        assert!(!round_is_clean(
            1,
            true,
            1,
            2,
            0,
            IncompleteReviewPolicy::Warn
        ));
    }

    #[test]
    fn warn_policy_gates_a_missing_seat_once_what_answered_is_clean() {
        assert!(round_is_clean(
            0,
            true,
            1,
            2,
            0,
            IncompleteReviewPolicy::Warn
        ));
    }

    #[test]
    fn a_full_panel_with_an_open_finding_is_not_clean() {
        assert!(!round_is_clean(
            1,
            true,
            2,
            2,
            0,
            IncompleteReviewPolicy::Block
        ));
    }

    #[test]
    fn a_full_panel_with_a_red_e2e_is_not_clean() {
        assert!(!round_is_clean(
            0,
            false,
            2,
            2,
            0,
            IncompleteReviewPolicy::Block
        ));
    }

    // The stall this task closes: under the default `block` policy, a seat
    // missing only because it was rate limited must not force a wait for a
    // session limit that will not lift by the next round. `round_is_clean`
    // is where that quorum carve-out lives; the review loop around it never
    // changes what a reviewer's vote or a finding's severity means.

    #[test]
    fn a_seat_missing_only_to_its_own_quota_is_clean_under_the_default_policy() {
        // 1 of 2 answered, and the one missing was quota'd — the exact
        // "review-2 rate limited (quota)" shape from the field report.
        assert!(round_is_clean(
            0,
            true,
            1,
            2,
            1,
            IncompleteReviewPolicy::Block
        ));
    }

    #[test]
    fn a_seat_missing_for_a_reason_other_than_quota_still_waits() {
        // 1 of 2 answered, but the miss was a crash/timeout/parse failure,
        // not a quota loss (`quota_missing` stays 0) — worth another try.
        assert!(!round_is_clean(
            0,
            true,
            1,
            2,
            0,
            IncompleteReviewPolicy::Block
        ));
    }

    #[test]
    fn a_quota_loss_does_not_excuse_an_open_finding_or_a_red_e2e() {
        assert!(!round_is_clean(
            1,
            true,
            1,
            2,
            1,
            IncompleteReviewPolicy::Block
        ));
        assert!(!round_is_clean(
            0,
            false,
            1,
            2,
            1,
            IncompleteReviewPolicy::Block
        ));
    }

    #[test]
    fn a_panel_lost_entirely_to_quota_still_waits_rather_than_deciding_on_nobody() {
        // Every seat quota'd, nobody answered: there is no panel to decide
        // on, so this must fall through to the existing block-and-retry
        // fallback rather than call an unreviewed patch clean.
        assert!(!round_is_clean(
            0,
            true,
            0,
            2,
            2,
            IncompleteReviewPolicy::Block
        ));
    }

    fn outcome(code: Option<i32>, resource_blocked: bool) -> CommandOutcome {
        CommandOutcome {
            command: "test".to_owned(),
            code,
            output_tail: String::new(),
            duration_ms: 0,
            resource_blocked,
        }
    }

    #[test]
    fn verify_is_inconclusive_only_when_a_resource_blocked_outcome_is_present() {
        assert!(!verify_inconclusive(&[outcome(Some(0), false)]));
        assert!(
            !verify_inconclusive(&[outcome(Some(1), false)]),
            "an ordinary failure is still evidence about the patch"
        );
        assert!(verify_inconclusive(&[outcome(None, true)]));
        assert!(
            verify_inconclusive(&[outcome(Some(0), false), outcome(None, true)]),
            "one inconclusive outcome taints the whole batch"
        );
        assert!(!verify_inconclusive(&[]));
    }

    #[tokio::test]
    async fn timed_out_pid_waiting_returns_as_soon_as_every_pid_is_confirmed_dead() {
        // Alive for the first two checks, then dead - confirms the loop
        // actually re-polls rather than deciding once and sleeping out the
        // ceiling regardless.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let started = Instant::now();
        wait_for_pids_with(
            &[123],
            |_| calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2,
            Duration::from_millis(5),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= 3,
            "must keep checking rather than deciding on the first answer"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "must return the moment it is confirmed dead, not wait out the ceiling"
        );
    }

    #[tokio::test]
    async fn timed_out_pid_waiting_gives_up_at_its_ceiling_if_never_confirmed_dead() {
        let started = Instant::now();
        wait_for_pids_with(
            &[123],
            |_| true, // never reports dead
            Duration::from_millis(5),
            Duration::from_millis(30),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(30),
            "must not give up before its own ceiling: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "must not wait past its own ceiling either: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn timed_out_pid_waiting_is_a_no_op_when_nothing_was_still_running() {
        let started = Instant::now();
        wait_for_pids_with(
            &[],
            |_| true,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "an empty pid list has nothing to confirm"
        );
    }

    // `review_conclusion` is the exact decision the review hand-off task
    // fixed: a round budget spent (or a tree that stopped moving) must not
    // collapse into `Blocked` regardless of what verification actually
    // said. Deterministic and process-free for the same reason the
    // `round_is_clean` family above is.
    fn review_round(
        clean: bool,
        blocking: usize,
        answered: usize,
        expected: usize,
        progressed: bool,
        e2e_ok: bool,
    ) -> ReviewRound {
        ReviewRound {
            round: 1,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: vec![CommandOutcome {
                command: "test".to_owned(),
                code: Some(if e2e_ok { 0 } else { 1 }),
                output_tail: String::new(),
                duration_ms: 0,
                resource_blocked: false,
            }],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking,
            answered,
            expected,
            clean,
            progressed,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }
    }

    #[test]
    fn review_conclusion_is_none_when_nothing_has_run() {
        assert_eq!(review_conclusion(&[], 3), None);
    }

    #[test]
    fn review_conclusion_is_none_while_rounds_remain() {
        let rounds = vec![review_round(false, 1, 2, 2, true, true)];
        assert_eq!(review_conclusion(&rounds, 3), None);
    }

    #[test]
    fn review_conclusion_is_gating_once_a_round_is_clean() {
        let rounds = vec![review_round(true, 0, 2, 2, false, true)];
        assert_eq!(review_conclusion(&rounds, 3), Some(RunStatus::Gating));
    }

    #[test]
    fn review_conclusion_hands_off_when_the_budget_is_spent_and_e2e_is_green() {
        let rounds = vec![
            review_round(false, 1, 2, 2, true, true),
            review_round(false, 1, 2, 2, true, true),
        ];
        assert_eq!(review_conclusion(&rounds, 2), Some(RunStatus::Gating));
    }

    #[test]
    fn review_conclusion_blocks_when_the_budget_is_spent_and_e2e_is_red() {
        let rounds = vec![
            review_round(false, 1, 2, 2, true, true),
            review_round(false, 1, 2, 2, true, false),
        ];
        assert_eq!(review_conclusion(&rounds, 2), Some(RunStatus::Blocked));
    }

    #[test]
    fn review_conclusion_stays_none_when_the_budget_is_spent_but_the_last_round_could_not_run() {
        // Magi never got a command to run against this round's own head — a
        // resource-blocked attempt, not a red one — so this must never
        // settle on `Blocked` the way a genuine e2e failure would. `None`
        // here is what tells `Runner::review_loop` to retry the check
        // itself rather than trust this cheap recomputation with a verdict
        // it cannot actually produce.
        let mut blocked = review_round(false, 1, 2, 2, true, false);
        blocked.e2e[0].resource_blocked = true;
        let rounds = vec![review_round(false, 1, 2, 2, true, true), blocked];
        assert_eq!(review_conclusion(&rounds, 2), None);
    }

    #[test]
    fn review_conclusion_blocks_an_incomplete_panel_that_raised_nothing_even_with_green_e2e() {
        // Missing input, not a verified tree — never a hand-off candidate.
        let rounds = vec![review_round(false, 0, 1, 2, false, true)];
        assert_eq!(review_conclusion(&rounds, 1), Some(RunStatus::Blocked));
    }

    #[test]
    fn review_conclusion_hands_off_when_the_tree_stagnates_before_the_budget_is_spent() {
        let rounds = vec![
            review_round(false, 1, 2, 2, false, true),
            review_round(false, 1, 2, 2, false, true),
        ];
        assert_eq!(review_conclusion(&rounds, 10), Some(RunStatus::Gating));
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A throwaway repo with one commit on `main`, for tests that need `merge`
    /// to make real (and, if it runs at all, real*ly fail*) git calls.
    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .quiet()
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "magi test"]);
        run(&["config", "user.email", "magi@example.com"]);
        std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "init"]);
    }

    // `settle_questions` is what closes the ghost the phone showed: a run's
    // seat asked something, the run then ended, and nothing was left to
    // abandon the question it left `open`. `HOME` is a process-wide
    // `OnceLock` (see `run::set_home`'s doc), so this only wins the race the
    // first time it runs in the binary — every test below still reaches the
    // same directory whichever call won, and each gets its own run id from
    // `RunState::new`, so they never collide there.
    fn ask_test_home() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-ask-tests-home"));
    }

    /// A minimal, git-free `Runner` at a given status — `settle_questions`
    /// reads nothing else off it.
    fn runner_at(status: RunStatus) -> Runner {
        let mut state = RunState::new(
            PathBuf::from("/nonexistent/repo"),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        state.status = status;
        Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        }
    }

    /// `park_here` folding in the reason `Pause::park_because` recorded -
    /// this is what lets an operator reading a run's events tell an
    /// interrupt-driven park from an ordinary shutdown park.
    #[test]
    fn park_here_folds_the_interrupt_reason_into_the_park_event() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-interrupt-tests-home"));
        let mut runner = runner_at(RunStatus::Implementing);
        let interrupt = Pause::new();
        runner.watch_interrupt(interrupt.clone());

        interrupt.park_because("task a1b2 asked to run first");

        assert!(runner.park_here().expect("park_here"));
        assert!(runner.state.parked);
        let last = runner.state.events.last().expect("a park event");
        assert_eq!(last.node, "park");
        assert!(
            last.message.contains("task a1b2 asked to run first"),
            "expected the interrupt reason in {:?}",
            last.message
        );
    }

    /// `watch_interrupt` and `on_pause` are genuinely independent: an ordinary
    /// shutdown `Pause` (what `Stop::park` hands every run, shared and never
    /// cleared) must not make a *different* run - one only watching its own,
    /// unshared interrupt `Pause` - see itself as parked. If a future change
    /// ever collapsed these back into one handle, the interrupt scheduler
    /// would park every run for the rest of the daemon's life, not just the
    /// one it meant to interrupt.
    #[test]
    fn the_stop_level_pause_and_a_runs_interrupt_pause_do_not_leak_into_each_other() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-interrupt-tests-home"));
        let mut runner = runner_at(RunStatus::Implementing);
        let shutdown = Pause::new();
        runner.on_pause(shutdown.clone());
        let interrupt = Pause::new();
        runner.watch_interrupt(interrupt.clone());

        // Nobody has asked for anything yet.
        assert!(!runner.park_here().expect("park_here"));
        assert!(!runner.state.parked);

        // Only the interrupt handle fires; the shutdown handle stays clear.
        interrupt.park_because("test");
        assert!(!shutdown.parked());
        assert!(runner.park_here().expect("park_here"));
    }

    /// The property every prior attempt at this feature failed to pin down:
    /// asking a run to park while one of its nodes has a real, in-flight
    /// async operation running (an agent call, in production) must not cut
    /// that operation short. `park_here` is only ever consulted *between*
    /// `execute`'s node calls - see its own doc - so nothing inside a node
    /// can observe a park request until the node itself returns. This proves
    /// that structurally, with real `tokio` concurrency and a channel
    /// handshake (never a sleep, which would only prove "usually", not
    /// "cannot"): the "node" below reports that it has genuinely started,
    /// and only then is the park requested; the node still has to be told to
    /// finish before `park_here` is ever called, exactly mirroring every
    /// `self.some_node().await; if self.park_here()? { return Ok(()); }` pair
    /// in `execute`.
    #[tokio::test]
    async fn a_park_request_made_mid_node_only_takes_effect_at_the_next_boundary() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-interrupt-tests-home"));
        let mut runner = runner_at(RunStatus::Implementing);
        let interrupt = Pause::new();
        runner.watch_interrupt(interrupt.clone());

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();

        // Stands in for one node's in-flight agent call: it proves it has
        // genuinely started, then blocks - exactly as a spawned CLI process
        // does - until told to finish.
        let node = async move {
            started_tx.send(()).expect("send started");
            finish_rx.await.expect("recv finish");
            "node finished"
        };

        let interrupter = async move {
            started_rx.await.expect("recv started");
            // The call is now genuinely in flight. Ask it to park.
            interrupt.park_because("higher-priority task waiting");
            // Nothing the node does can observe this yet - there is no
            // check inside it, by construction - so let the executor run
            // anything pending and then let the node finish on its own.
            tokio::task::yield_now().await;
            finish_tx.send(()).expect("send finish");
        };

        let (node_result, ()) = tokio::join!(node, interrupter);
        assert_eq!(
            node_result, "node finished",
            "the in-flight call ran to completion"
        );

        // Only now, at the boundary the real `execute` would check right
        // after this node, does the park take effect.
        assert!(runner.park_here().expect("park_here"));
        assert!(runner.state.parked);
    }

    /// A run parked mid-competition carries every field it had accumulated
    /// through the exact same disk round-trip an ordinary resume uses -
    /// `RunState::save`/`RunState::load`, which is all `Runner::resume` is.
    /// Nothing about parking for an interrupt is a special case of that path;
    /// this is what proves it rather than assuming it.
    #[test]
    fn a_run_parked_for_an_interrupt_resumes_with_nothing_lost() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-interrupt-tests-home"));
        let mut runner = runner_at(RunStatus::Judging);
        // `Runner::resume` re-resolves roles from the saved config, which
        // refuses an empty roster - give it the same minimal one `conductor`
        // itself uses.
        runner.state.config.agents = vec![conductor()];
        runner.state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "magi/x/A".to_owned(),
            worktree: PathBuf::from("/nonexistent/worktree"),
            summary: "did the thing".to_owned(),
            stat: "1 file changed".to_owned(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 1234,
            folded: false,
        }];
        let run_id = runner.state.id.clone();

        let interrupt = Pause::new();
        runner.watch_interrupt(interrupt.clone());
        interrupt.park_because("task c3d4 asked to run first");
        assert!(runner.park_here().expect("park_here"));

        let resumed = Runner::resume(&run_id).expect("resume");
        assert_eq!(resumed.state.candidates.len(), 1);
        assert_eq!(resumed.state.candidates[0].summary, "did the thing");
        assert_eq!(resumed.state.candidates[0].branch, "magi/x/A");
        assert_eq!(resumed.state.status, runner.state.status);
        assert!(
            resumed.state.parked,
            "still parked until `execute` actually walks the graph again"
        );
        assert!(resumed.state.events.iter().any(|e| e.node == "park"));
    }

    /// A fresh open question on `run`, stored and handed back for assertions.
    fn ask_open_question(store: &ask::Questions, run: &str) -> ask::Question {
        let mut q = ask::Question::new(
            run.to_owned(),
            "implement".to_owned(),
            "impl-A".to_owned(),
            "Which storage backend should the cache use?".to_owned(),
            String::new(),
            vec!["SQLite".to_owned(), "Redis".to_owned()],
        );
        store.put(&mut q).unwrap();
        q
    }

    #[test]
    fn a_failed_runs_open_question_is_abandoned() {
        ask_test_home();
        let store = ask::Questions::open();
        let mut runner = runner_at(RunStatus::Failed);
        let run = runner.state.id.clone();
        let q = ask_open_question(&store, &run);

        runner.settle_questions();

        let back = store.get(&q.id).unwrap();
        assert!(
            !back.status.open(),
            "the seat that asked died with the run; nobody is left to read an answer"
        );
        assert!(
            back.detail.contains(&run) && back.detail.contains("failed"),
            "the reason names what the run became, not just that it is gone: {}",
            back.detail
        );
    }

    #[test]
    fn a_merged_runs_open_question_is_abandoned_too() {
        ask_test_home();
        let store = ask::Questions::open();
        // A run that finishes cleanly still leaves nobody to read an answer -
        // this is not only a failure-path cleanup.
        for status in [RunStatus::Merged, RunStatus::Ready] {
            let mut runner = runner_at(status);
            let run = runner.state.id.clone();
            let q = ask_open_question(&store, &run);

            runner.settle_questions();

            let back = store.get(&q.id).unwrap();
            assert!(
                !back.status.open(),
                "{status:?} run's question must not outlive the run"
            );
        }
    }

    #[test]
    fn a_still_resumable_runs_open_question_is_left_alone() {
        ask_test_home();
        let store = ask::Questions::open();
        // `Blocked` and `Stalled` can still be resumed — the candidates, the
        // review round and the seat sessions are all still on disk — so a
        // question asked mid-round may yet get a real answer from a real
        // resume. Sweeping it here would be exactly the failure mode this
        // whole feature exists to avoid on the other side.
        for status in [RunStatus::Blocked, RunStatus::Stalled] {
            let mut runner = runner_at(status);
            let run = runner.state.id.clone();
            let q = ask_open_question(&store, &run);

            runner.settle_questions();

            let back = store.get(&q.id).unwrap();
            assert!(
                back.status.open(),
                "{status:?} is still alive; the question must still be waiting"
            );
        }
    }

    #[test]
    fn settle_questions_never_touches_an_already_answered_question() {
        ask_test_home();
        let store = ask::Questions::open();
        let mut runner = runner_at(RunStatus::Failed);
        let run = runner.state.id.clone();
        let mut q = ask_open_question(&store, &run);
        q.answer(crate::ask::Answer::Choice("SQLite".to_owned()))
            .unwrap();
        store.put(&mut q).unwrap();

        // Called twice, the way a crash-recovered daemon reclaim and the
        // graph's own cleanup both can for the same run — `abandon_for_run`
        // only ever touches what is still open, so this must be inert both
        // times, not merely the second.
        runner.settle_questions();
        runner.settle_questions();

        let back = store.get(&q.id).unwrap();
        assert_eq!(
            back.status,
            ask::QuestionStatus::Answered,
            "a real answer is a decision on record, never overwritten by a sweep"
        );
    }

    /// `fold_run(&mut state, drop_winner = false)` is exactly the call
    /// `clean::fold_due` makes for a `Ready`/`Failed` run - one that finished
    /// without merging, whose winner is still the operator's answer to read.
    /// Nothing previously called `fold_run` itself with a real `tally`, so
    /// this is the first test to pin down the one distinction the whole
    /// automatic-fold feature depends on: the winner's worktree and branch
    /// must survive, everything else sharing the run's worktree bay - a
    /// loser, standing in for a judge/review worktree too, since `fold_run`'s
    /// second sweep treats every non-winner directory under the bay alike -
    /// must not.
    #[tokio::test]
    async fn fold_run_keeps_only_the_winner_when_the_winner_is_not_dropped() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-fold-run-tests-home"));
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let mut config = Config::default();
        config.graph.worktree_root = Some(tmp.path().join("wt"));

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            config,
        );
        let root = state.worktree_root();
        let wt_a = root.join("cand-A");
        let wt_b = root.join("cand-B");
        git::worktree_add_branch(&repo, &wt_a, "magi/x/A", "main")
            .await
            .expect("worktree A");
        git::worktree_add_branch(&repo, &wt_b, "magi/x/B", "main")
            .await
            .expect("worktree B");

        state.candidates = vec![
            Candidate {
                index: 0,
                label: 'A',
                agent: "alpha".to_owned(),
                branch: "magi/x/A".to_owned(),
                worktree: wt_a.clone(),
                summary: String::new(),
                stat: String::new(),
                files: 0,
                commits: 0,
                empty: false,
                failed: None,
                verified_noop: None,
                duration_ms: 0,
                folded: false,
            },
            Candidate {
                index: 1,
                label: 'B',
                agent: "beta".to_owned(),
                branch: "magi/x/B".to_owned(),
                worktree: wt_b.clone(),
                summary: String::new(),
                stat: String::new(),
                files: 0,
                commits: 0,
                empty: false,
                failed: None,
                verified_noop: None,
                duration_ms: 0,
                folded: false,
            },
        ];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 1,
            present: 1,
            quorum: 1,
            met_quorum: true,
            uncontested: None,
        });
        state.status = RunStatus::Ready;

        fold_run(&mut state, false, &crate::run::home())
            .await
            .expect("fold_run");

        assert!(wt_a.exists(), "the unmerged winner's worktree survives");
        assert!(
            git::branch_exists(&repo, "magi/x/A").await.unwrap(),
            "the unmerged winner's branch survives"
        );
        assert!(
            !state.candidates[0].folded,
            "the winner is not marked folded"
        );

        assert!(!wt_b.exists(), "the loser's worktree is removed");
        assert!(
            !git::branch_exists(&repo, "magi/x/B").await.unwrap(),
            "the loser's branch is removed"
        );
        assert!(state.candidates[1].folded, "the loser is marked folded");
    }

    /// `status == Ready` used to be read as "this is the harmless
    /// `MergeMode::None` no-op path, nothing to guard" (graph.rs, prior to
    /// this test). But `land` sets the very same status when a `MergeMode::Pr`
    /// run's PR was closed without merging — and reentering `merge` with
    /// `mode` still `Pr` does not know the difference, so it pushed and
    /// opened a second pull request. `mode == Local` reproduces the same
    /// blind spot without a network call: reentry must not attempt another
    /// git merge once this node has already recorded an outcome.
    #[tokio::test]
    async fn merge_does_not_reattempt_once_a_run_has_concluded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let mut config = Config::default();
        config.merge.mode = MergeMode::Local;

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            config,
        );
        state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "does-not-exist".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a change".to_owned()),
        });
        state.reviews = vec![ReviewRound {
            round: 1,
            head: "deadbeef".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: true,
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        state.gate = vec![CommandOutcome {
            command: "test".to_owned(),
            code: Some(0),
            output_tail: String::new(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        state.gate_ran = true;
        // Reached its conclusion already — e.g. `land` closing the PR without
        // merging it, which (like the honest `MergeMode::None` path) leaves
        // `status` at `Ready`. The recorded outcome is what actually marks
        // this node done.
        state.status = RunStatus::Ready;
        state.merge = Some(MergeOutcome {
            mode: MergeMode::Local,
            ok: false,
            detail: "already concluded".to_owned(),
        });

        let mut runner = Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        };

        runner.merge().await.expect("merge");

        assert_eq!(
            runner.state.status,
            RunStatus::Ready,
            "a concluded run's status must not change on reentry"
        );
        assert_eq!(
            runner.state.merge.as_ref().map(|m| m.detail.as_str()),
            Some("already concluded"),
            "merge must not run again once the node already recorded an outcome"
        );
    }

    /// `gate` leaves `state.gate_ran` false both before it has ever run and
    /// when its last attempt was resource-blocked (the shared build cache
    /// could not be acquired or confirmed fresh in time - see
    /// `CommandOutcome::resource_blocked`'s own doc). Trusting the empty
    /// `Vec` this also leaves behind used to read as "nothing failed" and let
    /// a run merge a tree the gate never actually checked - exactly the case
    /// a contended cache produces on every retry until it clears. `merge`
    /// must refuse until `gate` has actually recorded an attempt.
    #[tokio::test]
    async fn merge_refuses_a_gate_that_has_not_actually_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let mut config = Config::default();
        config.merge.mode = MergeMode::Local;

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            config,
        );
        state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "does-not-exist".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a change".to_owned()),
        });
        state.reviews = vec![ReviewRound {
            round: 1,
            head: "deadbeef".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: true,
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        // The point: `gate` has not recorded anything yet.
        state.gate = Vec::new();
        state.gate_ran = false;
        state.status = RunStatus::Gating;

        let mut runner = Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        };

        runner.merge().await.expect("merge");

        assert!(
            runner.state.merge.is_none(),
            "an empty gate must never be read as a passing one: {:?}",
            runner.state.merge
        );
    }

    /// The `shoka` repro this schema bump exists for: `verify.gate` has no
    /// commands configured and `merge.mode` is `none` (a review-only run).
    /// `gate` must still record a real attempt — zero commands, vacuously
    /// passed — rather than leaving `state.gate` empty in a way `merge`
    /// cannot tell apart from "never ran"; otherwise the run reaches
    /// `Gating` and can never leave it. See `RunState::gate_ran`'s own doc.
    #[tokio::test]
    async fn gate_and_merge_reach_ready_when_no_gate_commands_are_configured() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        // Default config: `verify.gate` empty, `merge.mode` is `none`.
        let config = Config::default();

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            config,
        );
        state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "does-not-exist".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a change".to_owned()),
        });
        state.reviews = vec![ReviewRound {
            round: 1,
            head: "deadbeef".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: true,
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];

        let mut runner = Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        };

        runner.gate().await.expect("gate");
        assert!(
            runner.state.gate_ran,
            "zero configured commands is still a real attempt, not an unrun gate"
        );
        assert!(runner.state.gate.is_empty());
        assert_eq!(runner.state.gate_status(), GateStatus::PassedWithNoCommands);
        assert_ne!(
            runner.state.status,
            RunStatus::Blocked,
            "a gate with nothing to check must not read as failed"
        );

        runner.merge().await.expect("merge");
        assert_eq!(
            runner.state.status,
            RunStatus::Ready,
            "a clean review-only run with no gate commands must reach Ready, not stay stuck in Gating"
        );
    }

    /// `Config::cache_dir` is derived from `verify.e2e` as well as
    /// `verify.gate` (so the e2e leg and the final gate never build against
    /// different directories). With zero `verify.gate` commands but a
    /// `CARGO_TARGET_DIR`-using `verify.e2e`, `gate` used to still queue for
    /// that lease before discovering it had nothing to run - so a repo with
    /// no gate commands could come back `resource_blocked` (and therefore
    /// still `gate_ran == false`) on nothing but an unrelated run holding the
    /// cache, exactly the contention this run's own zero commands could
    /// never have touched. `gate` must recognise there is nothing to check
    /// before it ever asks for the lease.
    #[tokio::test]
    async fn gate_never_asks_for_the_cache_lease_when_it_has_no_commands_to_run() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-test-home"));
        let home = crate::run::home();

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        // Unique to this test, so holding its lease cannot collide with
        // another test sharing the same process-wide `home`.
        let cache_dir = tmp.path().join("target");

        let mut config = Config::default();
        config.verify.e2e = vec![format!("CARGO_TARGET_DIR='{}' true", cache_dir.display())];
        // `verify.gate` stays empty (the default). Bounded so a regression
        // that does start waiting fails the test in seconds, not hangs it.
        config.graph.timeout_verify = Some(2);

        let other = crate::cache::Owner::here("other-run", "e2e", "e2e", &repo, "deadbeef");
        let _held = match crate::cache::try_acquire(&home, &cache_dir, &other)
            .expect("no io error acquiring directly")
        {
            crate::cache::AcquireOutcome::Acquired(g) => g,
            crate::cache::AcquireOutcome::Busy(b) => {
                panic!("expected the direct acquire to win the lease first: {b:?}")
            }
        };

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            config,
        );
        state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "does-not-exist".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a change".to_owned()),
        });
        state.reviews = vec![ReviewRound {
            round: 1,
            head: "deadbeef".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: true,
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];

        let mut runner = Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        };

        let started = std::time::Instant::now();
        runner.gate().await.expect("gate");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a gate with nothing to run must never wait on a lease it never needed"
        );
        assert!(
            runner.state.gate_ran,
            "zero commands is still a real, immediate attempt"
        );
        assert!(runner.state.gate.is_empty());
        assert_ne!(
            runner.state.status,
            RunStatus::Blocked,
            "must not read as resource-blocked on a lease it never asked for"
        );
    }

    /// The shape the incident this whole fix responds to actually had: the
    /// round budget spent, the last round's own e2e blocked on the shared
    /// build cache (held here by a live pid — this test process — exactly
    /// `cache`'s own unit tests' pattern for "another owner, still alive"
    /// without forking a process). `stop_reviewing` must retry it — not
    /// silently leave the round looking untouched (the catch-up-only half of
    /// the bug), and not read the contention as a red `e2e` and block the
    /// run on it (the other half). Called directly, the same way
    /// `gate_never_asks_for_the_cache_lease_when_it_has_no_commands_to_run`
    /// above exercises `gate`, so this never needs a real cargo build to
    /// reach: the lease is never released, so `with_cache_lease` never gets
    /// past acquiring it into anything that would need a real workspace.
    #[tokio::test]
    async fn stop_reviewing_retries_a_resource_blocked_e2e_instead_of_reading_it_as_red() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-test-home"));
        let home = crate::run::home();

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let head = crate::git::rev_parse(&repo, "HEAD")
            .await
            .expect("rev-parse");
        // Unique to this test, so holding its lease cannot collide with
        // another test sharing the same process-wide `home`.
        let cache_dir = tmp.path().join("target");

        let mut config = Config::default();
        config.verify.e2e = vec![format!(
            "CARGO_TARGET_DIR='{}' test -f README.md",
            cache_dir.display()
        )];
        config.graph.review_rounds = 1;
        // Bounded so a regression that does start waiting fails the test in
        // seconds, not hangs it.
        config.graph.timeout_verify = Some(2);

        let other = crate::cache::Owner::here("other-run", "e2e", "e2e", &repo, "deadbeef");
        let held = match crate::cache::try_acquire(&home, &cache_dir, &other)
            .expect("no io error acquiring directly")
        {
            crate::cache::AcquireOutcome::Acquired(g) => g,
            crate::cache::AcquireOutcome::Busy(b) => {
                panic!("expected the direct acquire to win the lease first: {b:?}")
            }
        };

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            head.clone(),
            "task".to_owned(),
            config,
        );
        state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "does-not-exist".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a change".to_owned()),
        });
        // The round budget's last round, deferred: `needs_catchup_run`'s
        // other trigger. `stop_reviewing`'s retry machinery must treat this
        // exactly like a resource-blocked attempt once it actually runs.
        state.reviews = vec![ReviewRound {
            round: 1,
            head: head.clone(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 1,
            answered: 1,
            expected: 1,
            clean: false,
            verify_retried: false,
            e2e_deferred: true,
            e2e_defer_reason: Some("1 blocking finding(s) already required a fix".to_owned()),
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];

        let mut runner = Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        };

        let shell = runner.state.config.shell();
        runner
            .stop_reviewing("round budget spent", &shell, &repo)
            .await
            .expect("stop_reviewing");

        let last = runner.state.reviews.last().expect("round record");
        assert_eq!(
            last.e2e_status(),
            E2eStatus::ResourceBlocked,
            "the shared cache is still held; the attempt must read as blocked, not deferred or \
             failed: {last:?}"
        );
        assert_eq!(
            last.verified_head.as_deref(),
            Some(head.as_str()),
            "which commit this attempt targeted is known even though nothing finished checking \
             it"
        );
        let first_attempt_at = last
            .verified_at
            .expect("when this attempt ran is known too");
        assert_ne!(
            runner.state.status,
            RunStatus::Blocked,
            "contention is evidence about the machine, not the patch — it must not settle the \
             run as blocked: {:?}",
            runner.state.status
        );
        assert!(
            !runner
                .state
                .events
                .iter()
                .any(|e| e.node == "review" && e.message.contains("e2e failed")),
            "a resource-blocked attempt must never be logged as a failed e2e: {:?}",
            runner.state.events
        );

        // The cache is still held: a later reentry must retry the same
        // round's verification again — not leave it looking exactly as
        // untouched as the first blocked attempt, which is indistinguishable
        // from never having tried again at all.
        runner
            .stop_reviewing("round budget spent", &shell, &repo)
            .await
            .expect("stop_reviewing retry");
        assert_eq!(
            runner.state.reviews.len(),
            1,
            "no new round was started: {:?}",
            runner.state.reviews
        );
        let last = runner.state.reviews.last().expect("round record");
        assert_eq!(last.e2e_status(), E2eStatus::ResourceBlocked, "{last:?}");
        assert!(
            last.verified_at.expect("still known") > first_attempt_at,
            "a second reentry must be a fresh attempt, not a stale copy of the first"
        );
        assert_ne!(runner.state.status, RunStatus::Blocked);

        held.release();
    }

    /// A resumed run — a fresh `Runner`, `self.state.reviews` already
    /// holding the round `stop_reviewing` left `ResourceBlocked` from a
    /// prior process — must not sit at `Reviewing` forever: `review_loop`'s
    /// own top-of-function fast path (`review_conclusion`) correctly reads
    /// this shape as `None` rather than guessing `Blocked`, and the loop's
    /// own `for` range is empty once the round budget is spent, so
    /// `review_loop` must retry the check itself rather than silently doing
    /// nothing. Reaches the exact same retry `stop_reviewing_retries_a_*`
    /// above exercises directly, but through `review_loop`'s own entry point
    /// this time, proving the wiring between the two rather than just the
    /// retry logic in isolation.
    #[tokio::test]
    async fn a_resumed_review_loop_retries_a_last_round_left_resource_blocked() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-test-home"));
        let home = crate::run::home();

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let head = crate::git::rev_parse(&repo, "HEAD")
            .await
            .expect("rev-parse");
        let cache_dir = tmp.path().join("target");

        let mut config = Config::default();
        config.verify.e2e = vec![format!(
            "CARGO_TARGET_DIR='{}' test -f README.md",
            cache_dir.display()
        )];
        config.graph.review_rounds = 1;
        config.graph.timeout_verify = Some(2);

        let other = crate::cache::Owner::here("other-run", "e2e", "e2e", &repo, "deadbeef");
        let held = match crate::cache::try_acquire(&home, &cache_dir, &other)
            .expect("no io error acquiring directly")
        {
            crate::cache::AcquireOutcome::Acquired(g) => g,
            crate::cache::AcquireOutcome::Busy(b) => {
                panic!("expected the direct acquire to win the lease first: {b:?}")
            }
        };

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            head.clone(),
            "task".to_owned(),
            config,
        );
        state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "does-not-exist".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a change".to_owned()),
        });
        // The exact shape a prior process's `stop_reviewing` would have left
        // on disk: the round budget's last round, a real attempt already
        // made and already resource-blocked.
        state.reviews = vec![ReviewRound {
            round: 1,
            head: head.clone(),
            verified_head: Some(head.clone()),
            verified_at: Some(jiff::Timestamp::now()),
            reviews: Vec::new(),
            e2e: vec![CommandOutcome {
                command: format!(
                    "CARGO_TARGET_DIR='{}' test -f README.md",
                    cache_dir.display()
                ),
                code: None,
                output_tail: "waiting for the shared build cache".to_owned(),
                duration_ms: 0,
                resource_blocked: true,
            }],
            fix: None,
            blocking: 1,
            answered: 1,
            expected: 1,
            clean: false,
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];

        let first_attempt_at = state.reviews[0].verified_at.expect("set above");
        let mut runner = Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        };

        // The lease is still held throughout, so this reentry's own retry is
        // also contended — proving `review_loop` actually tried again (not
        // that it happened to succeed) is what the timestamp comparison
        // below is for.
        runner.review_loop().await.expect("review_loop");

        assert_eq!(
            runner.state.reviews.len(),
            1,
            "no new round was started on top of the unresolved one: {:?}",
            runner.state.reviews
        );
        let last = &runner.state.reviews[0];
        assert_eq!(
            last.e2e_status(),
            E2eStatus::ResourceBlocked,
            "still contended: {last:?}"
        );
        assert!(
            last.verified_at.expect("still known") > first_attempt_at,
            "review_loop must have actually retried the check, not left it exactly as found"
        );
        assert_ne!(
            runner.state.status,
            RunStatus::Blocked,
            "a resumed run must not read leftover contention as a verdict on the patch: {:?}",
            runner.state.status
        );

        held.release();
    }

    #[tokio::test]
    async fn a_run_resumed_mid_landing_reenters_land_instead_of_opening_a_second_pull_request() {
        crate::run::set_home(std::env::temp_dir().join("magi-graph-test-home"));
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let mut config = Config::default();
        config.merge.mode = MergeMode::Pr;
        config.graph.land = true;
        config.graph.land_approval = false;

        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            config,
        );
        state.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "alpha".to_owned(),
            branch: "does-not-exist".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        }];
        state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a change".to_owned()),
        });
        state.reviews = vec![ReviewRound {
            round: 1,
            head: "deadbeef".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: true,
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        state.gate = vec![CommandOutcome {
            command: "test".to_owned(),
            code: Some(0),
            output_tail: String::new(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        state.gate_ran = true;
        // A first pass through `merge` already pushed and opened this pull
        // request; `status` is `Landing` because a previous call into `land`
        // parked or was interrupted before it reached a terminal outcome.
        state.status = RunStatus::Landing;
        state.merge = Some(MergeOutcome {
            mode: MergeMode::Pr,
            ok: true,
            detail: "https://example.invalid/x/y/pull/1".to_owned(),
        });

        // The Landing-resume shortcut calls `run_land` directly rather than
        // through `merge`, which is exactly the call site that used to skip
        // `settle_questions` - see the fixture below.
        ask_test_home();
        let store = ask::Questions::open();
        let q = ask_open_question(&store, &state.id);

        let mut runner = Runner {
            state,
            roles: ResolvedRoles {
                implementers: Vec::new(),
                judges: Vec::new(),
                reviewers: Vec::new(),
                fixer: None,
                conductor: conductor(),
                implementer_roster: Vec::new(),
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
            interrupt: Pause::new(),
        };

        // `execute`, not `merge` directly: the Landing-resume shortcut lives
        // at the top of `execute`, not inside `merge` (see `execute`'s doc)
        // exactly because `review_loop` would otherwise clobber the marker
        // first.
        runner.execute().await.expect("execute");

        assert_eq!(
            runner.state.merge.as_ref().map(|m| m.detail.as_str()),
            Some("https://example.invalid/x/y/pull/1"),
            "reentry must not push again or open a second pull request over the \
             one `land` is already watching"
        );
        assert_ne!(
            runner.state.status,
            RunStatus::Landing,
            "land could not actually reach the fake pull request, so it must \
             have given up rather than left the run silently parked forever"
        );
        // `land` could not reach the fake pull request, so it gave up into
        // `Blocked` - still resumable, so the question must not have been
        // swept just because this branch now also calls `settle_questions`.
        assert_eq!(runner.state.status, RunStatus::Blocked);
        assert!(
            store.get(&q.id).unwrap().status.open(),
            "Blocked is still alive; settle_questions must have been a no-op here"
        );
    }

    fn state_with_round(round: ReviewRound) -> RunState {
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        );
        s.reviews = vec![round];
        s
    }

    fn finding(id: &str, severity: Severity, title: &str) -> crate::verdict::Finding {
        crate::verdict::Finding {
            id: id.to_owned(),
            severity,
            file: None,
            line: None,
            title: title.to_owned(),
            detail: String::new(),
        }
    }

    #[test]
    fn pr_body_names_open_findings_and_declined_ones() {
        let round = ReviewRound {
            round: 2,
            head: "deadbee".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![ReviewRecord {
                reviewer: 1,
                agent: "alpha".to_owned(),
                summary: String::new(),
                findings: vec![finding("R2-1-1", Severity::Minor, "unused import")],
                vote: None,
                failed: None,
                duration_ms: 0,
            }],
            e2e: vec![CommandOutcome {
                command: "cargo test".to_owned(),
                code: Some(0),
                output_tail: String::new(),
                duration_ms: 0,
                resource_blocked: false,
            }],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(FixRecord {
                agent: "alpha".to_owned(),
                addressed: Vec::new(),
                rejected: vec![crate::verdict::Rejection {
                    id: "R1-1-1".to_owned(),
                    why: "not reachable from any caller".to_owned(),
                }],
                notes: String::new(),
                committed: true,
                failed: None,
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 0,
            answered: 1,
            expected: 1,
            clean: false,
            progressed: true,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let state = state_with_round(round);
        let body = pr_body(&state, 'A');

        assert!(body.contains("add retries"), "the task must still be there");
        assert!(body.contains("R2-1-1"), "{body}");
        assert!(body.contains("unused import"), "{body}");
        assert!(body.contains("R1-1-1"), "the declined finding: {body}");
        assert!(
            body.contains("not reachable from any caller"),
            "the reason it was declined: {body}"
        );
    }

    #[test]
    fn pr_body_says_nothing_extra_when_the_round_was_clean() {
        let round = ReviewRound {
            round: 1,
            head: "deadbee".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![ReviewRecord {
                reviewer: 1,
                agent: "alpha".to_owned(),
                summary: String::new(),
                findings: Vec::new(),
                vote: None,
                failed: None,
                duration_ms: 0,
            }],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 1,
            expected: 1,
            clean: true,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        };
        let state = state_with_round(round);
        let body = pr_body(&state, 'A');
        assert!(!body.contains("Open review findings"), "{body}");
        assert!(!body.contains("Declined"), "{body}");
    }

    #[test]
    fn pr_body_titles_itself_from_the_task_not_run_or_candidate() {
        let state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        );
        let body = pr_body(&state, 'A');
        let title = body.lines().next().unwrap();

        assert_eq!(
            title, "add retries",
            "the title must be the task, not run/candidate bookkeeping: {body}"
        );
        assert!(
            body.contains(&format!("magi:run/{}", state.id)),
            "the run id must still be recoverable from the footer: {body}"
        );
        assert!(
            body.contains("magi:candidate-a"),
            "the candidate must still be recoverable from the footer: {body}"
        );
    }

    #[test]
    fn pr_body_never_titles_itself_off_a_blank_first_line() {
        let leading_blank = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "\n\n  \nadd retries\n\ndetails".to_owned(),
            Config::default(),
        );
        let body = pr_body(&leading_blank, 'A');
        assert_eq!(
            body.lines().next(),
            Some("add retries"),
            "a leading blank line must not become an empty title: {body}"
        );

        let whitespace_only = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "   \n  \n".to_owned(),
            Config::default(),
        );
        let body = pr_body(&whitespace_only, 'A');
        let title = body.lines().next().unwrap_or_default();
        assert!(
            !title.is_empty(),
            "a whitespace-only instruction must still fall back to a non-empty title: {body}"
        );
    }

    #[test]
    fn pr_title_truncates_a_first_line_over_githubs_limit() {
        // A run 2963-shaped instruction: a single first line well past
        // GitHub's 256-character createPullRequest limit, with a multi-byte
        // character mixed in so the truncation is exercised on `chars()`
        // counting rather than bytes.
        let long_line = format!("fix the thing 🎉 {}", "x".repeat(400));
        let title = pr_title(&long_line);

        assert!(
            title.chars().count() <= PR_TITLE_MAX,
            "title must stay within PR_TITLE_MAX: {title:?} ({} chars)",
            title.chars().count()
        );
        assert!(
            title.chars().count() < 256,
            "title must stay within GitHub's 256-character limit: {title:?}"
        );
        assert!(
            title.ends_with('…'),
            "a truncated title must say so: {title:?}"
        );
    }

    #[test]
    fn pr_title_leaves_a_short_title_untouched() {
        let title = pr_title("add retries\n\nmore detail below");
        assert_eq!(title, "add retries");
    }

    #[test]
    fn pr_title_strips_markdown_heading_markers() {
        let title = pr_title("# Rework the config loader\n\ndetails");
        assert_eq!(title, "Rework the config loader");
    }

    #[test]
    fn pr_title_of_pr_body_stays_within_githubs_limit() {
        let state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            format!("fix the thing 🎉 {}", "x".repeat(400)),
            Config::default(),
        );
        let body = pr_body(&state, 'A');
        let title = pr_title(&body);

        assert!(
            title.chars().count() < 256,
            "the title gh_pr_create sends must stay within GitHub's limit: {title:?}"
        );
    }

    #[test]
    fn manual_merge_command_matches_the_configured_style() {
        let repo = Path::new("/repo");
        let message = "Merge magi run 0832 (candidate A)\n\nadd retries";

        let merge = manual_merge_command(MergeStyle::Merge, repo, "magi/0832/A", message);
        assert_eq!(merge, "git -C /repo merge --no-ff magi/0832/A");

        let squash = manual_merge_command(MergeStyle::Squash, repo, "magi/0832/A", message);
        assert_eq!(
            squash,
            "git -C /repo merge --squash magi/0832/A && git -C /repo commit -m \
             \"Merge magi run 0832 (candidate A)\""
        );

        let rebase = manual_merge_command(MergeStyle::Rebase, repo, "magi/0832/A", message);
        assert_eq!(rebase, "git -C /repo merge --ff-only magi/0832/A");
    }

    #[test]
    fn a_nudge_gets_a_quarter_of_the_budget() {
        // The judge and implement budgets magi ships with.
        assert_eq!(retry_budget(secs(1200), true), secs(300));
        assert_eq!(retry_budget(secs(3600), true), secs(900));
    }

    #[test]
    fn a_resent_prompt_keeps_the_whole_budget() {
        // The seat kept no context, so the retry is the original job again and
        // shortening it would only guarantee a second failure.
        assert_eq!(retry_budget(secs(1200), false), secs(1200));
        assert_eq!(retry_budget(secs(60), false), secs(60));
    }

    #[test]
    fn the_floor_never_exceeds_the_original_budget() {
        // A short configured timeout must not be *raised* by the floor: the
        // operator asked for a bound, and a retry may not outlast the attempt
        // it is retrying.
        assert_eq!(retry_budget(secs(60), true), secs(60));
        assert_eq!(retry_budget(secs(480), true), secs(120));
        assert_eq!(retry_budget(secs(0), true), secs(0));
    }

    fn evidence(exit_code: Option<i32>) -> agent::CommandEvidence {
        agent::CommandEvidence {
            id: "item1".to_owned(),
            description: "cargo test".to_owned(),
            exit_code,
            result_summary: String::new(),
            source: "codex".to_owned(),
        }
    }

    #[test]
    fn a_reply_with_no_commands_at_all_is_not_unconfirmed() {
        // No evidence is not the same fact as unconfirmed evidence: a
        // backend with no adapter, or a reply that ran no commands at all,
        // must not be misread as carrying a dangling job.
        assert!(!has_unconfirmed_command(&[]));
    }

    #[test]
    fn a_command_with_a_real_exit_code_is_confirmed_whatever_its_value() {
        // Deliberately not a check on the exit code's *value*: a fixer
        // legitimately runs something that fails mid-iteration before it
        // succeeds, and that must never by itself reopen a valid report.
        assert!(!has_unconfirmed_command(&[evidence(Some(0))]));
        assert!(!has_unconfirmed_command(&[evidence(Some(1))]));
        assert!(!has_unconfirmed_command(&[
            evidence(Some(0)),
            evidence(Some(101))
        ]));
    }

    #[test]
    fn one_command_with_no_readable_exit_code_is_enough_to_flag_the_reply() {
        assert!(has_unconfirmed_command(&[
            evidence(Some(0)),
            evidence(None)
        ]));
    }

    #[test]
    fn a_clean_usable_reply_with_the_marker_is_a_verified_claim() {
        let text = "NO CHANGE NEEDED: already fixed by b32cfc4, on main.";
        assert_eq!(
            verified_noop_claim(true, &[], text).as_deref(),
            Some("already fixed by b32cfc4, on main.")
        );
    }

    #[test]
    fn an_unusable_reply_never_earns_the_benefit_of_the_doubt() {
        // A timeout or a bad exit code reads as the ordinary loss it is,
        // whatever the reply's own prose claims.
        let text = "NO CHANGE NEEDED: already fixed by b32cfc4, on main.";
        assert!(verified_noop_claim(false, &[], text).is_none());
    }

    #[test]
    fn an_unconfirmed_command_disqualifies_the_claim_even_on_a_usable_reply() {
        let text = "NO CHANGE NEEDED: already fixed by b32cfc4, on main.";
        assert!(verified_noop_claim(true, &[evidence(None)], text).is_none());
        // A confirmed command alongside the marker is fine.
        assert!(verified_noop_claim(true, &[evidence(Some(0))], text).is_some());
    }

    #[test]
    fn an_ordinary_reply_with_no_marker_is_never_a_claim() {
        assert!(verified_noop_claim(true, &[], "- did the thing\n- tested it").is_none());
    }

    /// Sets `runner.state.candidates` to one candidate per `(empty, verified)`
    /// pair, in order, labelled A, B, C, ...
    fn set_candidates(runner: &mut Runner, shape: &[(bool, Option<&str>)]) {
        runner.state.candidates = shape
            .iter()
            .enumerate()
            .map(|(i, &(empty, verified))| Candidate {
                index: i,
                label: (b'A' + i as u8) as char,
                agent: "sonnet".to_owned(),
                branch: format!("magi/x/{}", (b'A' + i as u8) as char),
                worktree: PathBuf::from(format!("/wt/{i}")),
                summary: String::new(),
                stat: String::new(),
                files: 0,
                commits: 0,
                empty,
                failed: None,
                verified_noop: verified.map(str::to_owned),
                duration_ms: 0,
                folded: false,
            })
            .collect();
    }

    #[test]
    fn after_implement_reads_all_candidates_verified_as_a_noop_not_a_failure() {
        ask_test_home();
        let mut runner = runner_at(RunStatus::Implementing);
        set_candidates(
            &mut runner,
            &[
                (true, Some("already on main at b32cfc4")),
                (true, Some("same fix, see the existing test")),
            ],
        );

        runner
            .after_implement()
            .expect("a verified no-op is not an error");

        assert_eq!(runner.state.status, RunStatus::VerifiedNoop);
    }

    #[test]
    fn after_implement_does_not_accept_one_candidates_claim_next_to_an_ordinary_loss() {
        ask_test_home();
        let mut runner = runner_at(RunStatus::Implementing);
        // Candidate A declares a verified no-op; candidate B simply wrote
        // nothing and said nothing about why. One candidate's claim is not
        // the whole run's agreement.
        set_candidates(
            &mut runner,
            &[(true, Some("already on main at b32cfc4")), (true, None)],
        );

        let err = runner
            .after_implement()
            .expect_err("an unverified empty candidate must still fail the run");

        assert!(
            err.to_string().contains("no candidate produced a change"),
            "{err}"
        );
        assert_eq!(runner.state.status, RunStatus::Failed);
    }

    #[test]
    fn after_implement_still_fails_an_ordinary_all_empty_run() {
        ask_test_home();
        let mut runner = runner_at(RunStatus::Implementing);
        set_candidates(&mut runner, &[(true, None), (true, None)]);

        let err = runner
            .after_implement()
            .expect_err("no candidate declared anything; this is an ordinary failure");

        assert!(
            err.to_string().contains("no candidate produced a change"),
            "{err}"
        );
        assert_eq!(runner.state.status, RunStatus::Failed);
    }
}
