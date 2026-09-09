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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use jiff::Timestamp;
use tokio::sync::Semaphore;

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
use crate::run::{
    BaseSync, Candidate, CommandOutcome, DeliberationRound, DeliberationTurn, FixRecord, Judgement,
    MergeOutcome, QuotaLoss, ReviewRecord, ReviewRevoteRecord, ReviewRound, RunState, RunStatus,
    Tally, VoteRecord, tail, write_artifact,
};
use crate::verdict::{
    self, FinalVote, FixReport, Position, Ranking, Review, ReviewRevote, ReviewVote, Severity,
};

/// How much verification output is kept and fed back to the fixer.
const OUTPUT_TAIL: usize = 8_000;

/// Bytes of a failing command's output kept in an event, so the reason a run
/// stopped is readable from the report without opening `run.json`.
const EVENT_OUTPUT_TAIL: usize = 2_000;

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
#[derive(Debug, Clone, Default)]
pub struct Pause(Arc<AtomicBool>);

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

    /// Has a park been asked for?
    #[must_use]
    pub fn parked(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Drives one run.
pub struct Runner {
    /// Run state; public so the CLI can report on it.
    pub state: RunState,
    roles: ResolvedRoles,
    sem: Arc<Semaphore>,
    /// Set when someone wants the run parked at its next node boundary.
    pause: Pause,
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
        self.implement().await?;
        if self.park_here()? {
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
        if !self.pause.parked() {
            return Ok(false);
        }
        self.state.event(
            "park",
            format!(
                "parked after `{}` — resume to carry on from here",
                self.state.status.as_str()
            ),
        );
        self.state.parked = true;
        self.state.save()?;
        Ok(true)
    }

    /// Hand the runner a pause to watch.
    pub fn on_pause(&mut self, pause: Pause) {
        self.pause = pause;
    }

    /// Abandon this run's own open questions, once `status` has actually
    /// settled rather than merely paused.
    ///
    /// `Blocked` and `Stalled` are `RunStatus::resumable` — a human can pick
    /// either back up with the candidates, the review round and the seat
    /// sessions already on disk, so a question an implementer asked mid-round
    /// may still get a real answer read by a real resume. Only the three
    /// statuses `resumable` excludes are actually final: the run merged, or
    /// it reached `Ready` with nothing left to do, or it failed outright with
    /// no established point to continue from. In every one of those the seat
    /// that asked is gone for good, exactly like the run being deleted under
    /// `magi run rm` - so the same cleanup applies, worded for what actually
    /// happened instead of "the run was deleted".
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
                prompt: prompt::implement(&instruction, &worktree.to_string_lossy(), &language),
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
        // job: `wave` consumes what it is given.
        let sent = jobs.clone();
        let cache = self.state.config.cache_dir();
        let ctx = WaveCtx {
            run: &run_id,
            node: "implement",
            prompts: &prompts,
            cache: cache.as_deref(),
        };
        let mut results = wave(jobs, Arc::clone(&self.sem), &ctx, &mut self.state, 0).await;
        self.resume_undelivered(&mut results, &sent, &prompts, &run_id)
            .await;

        for (&i, (_wi, seat, out)) in todo.iter().zip(results) {
            let seat_key = seat.key.clone();
            self.state.seats.insert(seat.key.clone(), seat);
            let label = self.state.candidates[i].label;
            let worktree = self.state.candidates[i].worktree.clone();
            let base = self.state.base_commit.clone();

            let (summary, duration, failed) = match out {
                AgentOutcome::Ok(o) => {
                    let text = verdict::section(&o.text, "summary").unwrap_or(o.text.clone());
                    let failed = (!o.usable()).then(|| {
                        if o.timed_out {
                            "agent timed out".to_owned()
                        } else {
                            format!("agent exited with {:?}", o.exit_code)
                        }
                    });
                    (text, o.duration_ms, failed)
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
                    )
                }
                AgentOutcome::Failed(e) => (String::new(), 0, Some(e)),
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
            let note = match (&c.failed, c.empty, rescued) {
                (Some(e), _, _) => format!("candidate {label}: {e}"),
                (None, true, _) => format!("candidate {label}: no change produced"),
                (None, false, true) => {
                    format!(
                        "candidate {label}: {files} files, {commits} commits (rescued an uncommitted tree)"
                    )
                }
                (None, false, false) => {
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
            };
            let (resumed_seat, resumed) =
                run_one(retry, Arc::clone(&self.sem), &ctx, &mut self.state, 1).await;
            *seat = resumed_seat;
            *out = resumed;
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

        let repo = self.state.repo.clone();
        let root = self.state.worktree_root();
        let language = self.state.config.graph.language.clone();
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let base = self.landing_base();
        let base_short = short(&base);
        let reviewers = self.roles.reviewers.clone();
        let shell = self.state.config.shell();

        let mut prev_e2e: Option<String> = None;
        for round in (self.state.reviews.len() + 1)..=max_rounds {
            let head = git::rev_parse(&winner.worktree, "HEAD").await?;
            let patch = git::diff(&winner.worktree, &base, "HEAD").await?;
            let stat = git::diff_stat(&winner.worktree, &base, "HEAD").await?;

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
                        e2e: prev_e2e.as_deref(),
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

            let mut e2e = run_commands(
                &shell,
                &self.state.config.verify.e2e,
                &winner.worktree,
                Duration::from_secs(self.state.config.graph.timeout_review),
            )
            .await;
            for o in &e2e {
                self.state.event(
                    "verify",
                    format!("round {round}: `{}` -> {}", o.command, e2e_outcome_label(o)),
                );
            }

            // A build/link failure is not a verdict on the patch — it is
            // frequently a race against a shared `CARGO_TARGET_DIR` (see
            // AGENTS.md). Give verify one retry before letting a red like
            // that decide the round.
            let verify_retried = e2e.iter().any(CommandOutcome::build_failed);
            if verify_retried {
                self.state.event(
                    "verify",
                    format!(
                        "round {round}: verify could not build/link, not a test result — \
                         retrying once before concluding"
                    ),
                );
                e2e = run_commands(
                    &shell,
                    &self.state.config.verify.e2e,
                    &winner.worktree,
                    Duration::from_secs(self.state.config.graph.timeout_review),
                )
                .await;
                for o in &e2e {
                    self.state.event(
                        "verify",
                        format!(
                            "round {round}: retry `{}` -> {}",
                            o.command,
                            e2e_outcome_label(o)
                        ),
                    );
                }
            }

            let e2e_failures: String = e2e
                .iter()
                .filter(|o| !o.ok())
                .map(|o| format!("$ {}\n{}\n", o.command, o.output_tail))
                .collect();

            let expected = records.len();
            let answered = records.iter().filter(|r| r.failed.is_none()).count();
            let incomplete = answered < expected;
            let blocking = all_findings.iter().filter(|f| f.severity.blocks()).count();
            let e2e_ok = e2e.iter().all(CommandOutcome::ok);
            let policy = self.state.config.graph.incomplete_review;
            let clean = round_is_clean(blocking, e2e_ok, answered, expected, policy);

            let mut round_record = ReviewRound {
                round,
                head: head.clone(),
                reviews: records,
                e2e,
                verify_retried,
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
                    if incomplete {
                        format!(
                            "round {round}: clean (warn policy, incomplete panel) — no \
                             blocking findings from the seats that answered, verification green"
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
            // answered and the policy refuses to call that clean: re-review
            // rather than send the fixer after a round with nothing to fix.
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
                prev_e2e = None;
                continue;
            }

            if round == max_rounds {
                self.state.reviews.push(round_record);
                return self.stop_reviewing(&format!(
                    "{blocking} blocking finding(s) still open after {max_rounds} round(s)"
                ));
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
                    (!e2e_failures.is_empty()).then_some(e2e_failures.as_str()),
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
            };
            let (seat, out) = run_one(job, Arc::clone(&self.sem), &ctx, &mut self.state, 0).await;
            let agent_id = seat.agent.clone();
            let seat_key = seat.key.clone();
            self.state.seats.insert(seat.key.clone(), seat);

            let mut fix = FixRecord {
                agent: agent_id,
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: false,
                failed: None,
                duration_ms: 0,
            };
            match out {
                AgentOutcome::Ok(o) => {
                    fix.duration_ms = o.duration_ms;
                    match verdict::extract_json::<FixReport>(&o.text) {
                        Ok(report) => {
                            fix.addressed = report.addressed;
                            fix.rejected = report.rejected;
                            fix.notes =
                                blind::sanitize_prose(&report.notes, &self.state.config.blind);
                        }
                        Err(e) => fix.failed = Some(format!("unparsable fix report: {e}")),
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
                        seat: seat_key,
                        node: "fix".to_owned(),
                        at: Timestamp::now(),
                        reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                    });
                    fix.failed = Some("rate limited (quota); fixer could not run".to_owned());
                }
                AgentOutcome::Failed(e) => fix.failed = Some(e),
            }
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
                        "round {round}: {} addressed, {} rejected, {commit_note}, tree {tree_note}",
                        fix.addressed.len(),
                        fix.rejected.len(),
                    ),
                },
            );
            round_record.fix = Some(fix);
            round_record.progressed = progressed;
            self.state.reviews.push(round_record);
            self.state.save()?;

            prev_e2e = (!e2e_failures.is_empty()).then_some(e2e_failures);

            let streak = self
                .state
                .reviews
                .iter()
                .rev()
                .take_while(|r| !r.progressed)
                .count();
            if streak >= STAGNANT_LIMIT {
                return self.stop_reviewing(&format!(
                    "the tree has not moved against base for {streak} round(s) in a row"
                ));
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
    fn stop_reviewing(&mut self, why: &str) -> Result<()> {
        let last = self
            .state
            .reviews
            .last()
            .expect("a round was just recorded before this is called");
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
        let open: usize = last.reviews.iter().map(|r| r.findings.len()).sum();

        if red.is_empty() {
            self.state.event(
                "review",
                format!("{why}; e2e is green — handing off with {open} finding(s) still open"),
            );
            self.state.status = RunStatus::Gating;
        } else {
            self.state
                .event("review", format!("{why}; e2e failed:\n{}", red.join("\n")));
            self.state.status = RunStatus::Blocked;
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
        if !self.state.gate.is_empty() {
            return Ok(());
        }
        let Some(winner) = self.state.winner().cloned() else {
            return Ok(());
        };
        self.state.status = RunStatus::Gating;
        let shell = self.state.config.shell();
        let outcomes = run_commands(
            &shell,
            &self.state.config.verify.gate,
            &winner.worktree,
            Duration::from_secs(self.state.config.graph.timeout_review),
        )
        .await;
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
        let passed = outcomes.iter().all(CommandOutcome::ok);
        self.state.gate = outcomes;
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
            || self.state.gate.iter().any(|o| !o.ok())
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
    let mut set = tokio::task::JoinSet::new();
    let overlay = prompts.overlay(node);
    for (i, mut job) in jobs.into_iter().enumerate() {
        job.prompt = prompt::with_overlay(job.prompt, overlay.clone());
        if cache.is_some() {
            job.prompt.push('\n');
            job.prompt.push_str(prompt::build_cache_note());
        }
        let sem = Arc::clone(&sem);
        let run = run.to_owned();
        let node = node.to_owned();
        let cache = cache.map(Path::to_path_buf);
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
    collected.into_iter().flatten().collect()
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
fn round_is_clean(
    blocking: usize,
    e2e_ok: bool,
    answered: usize,
    expected: usize,
    policy: IncompleteReviewPolicy,
) -> bool {
    blocking == 0 && e2e_ok && (answered == expected || policy == IncompleteReviewPolicy::Warn)
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
fn review_conclusion(reviews: &[ReviewRound], max_rounds: usize) -> Option<RunStatus> {
    if max_rounds == 0 || reviews.iter().any(|r| r.clean) {
        return Some(RunStatus::Gating);
    }
    let last = reviews.last()?;
    let stagnant = reviews.iter().rev().take_while(|r| !r.progressed).count() >= STAGNANT_LIMIT;
    if reviews.len() < max_rounds && !stagnant {
        return None;
    }
    Some(if last.incomplete() && last.blocking == 0 {
        RunStatus::Blocked
    } else if last.e2e.iter().all(CommandOutcome::ok) {
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

/// Run configured shell commands in `cwd`, in order.
async fn run_commands(
    shell: &[String],
    commands: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Vec<CommandOutcome> {
    let mut out = Vec::new();
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
            Ok(child) => match tokio::time::timeout(timeout, child.wait_with_output()).await {
                Ok(Ok(o)) => {
                    let mut body = String::from_utf8_lossy(&o.stdout).into_owned();
                    body.push_str(&String::from_utf8_lossy(&o.stderr));
                    (o.status.code(), body)
                }
                Ok(Err(e)) => (None, format!("failed to run: {e}")),
                Err(_) => (None, format!("timed out after {}s", timeout.as_secs())),
            },
            Err(e) => (None, format!("failed to spawn `{}`: {e}", shell[0])),
        };
        out.push(CommandOutcome {
            command: command.clone(),
            code,
            output_tail: tail(&body, OUTPUT_TAIL),
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }
    out
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
fn pr_body(state: &RunState, winner: char) -> String {
    let mut message = format!(
        "Merge magi run {} (candidate {winner})\n\n{}",
        state.id, state.instruction
    );

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

    message
}

/// `gh pr create`, returning the PR url.
async fn gh_pr_create(cwd: &Path, base: &str, head: &str, body: &str) -> Result<String> {
    let title = body.lines().next().unwrap_or("magi run").to_owned();
    let out = tokio::process::Command::new("gh")
        .args([
            "pr", "create", "--base", base, "--head", head, "--title", &title, "--body", body,
        ])
        .current_dir(cwd)
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
pub async fn fold_run(state: &mut RunState, drop_winner: bool) -> Result<Vec<String>> {
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

    if state.enabled_worktree_config && drop_winner {
        // A release, not a raw disable: some sibling run in this repository
        // may still hold its own reference (see `git::acquire_worktree_config`),
        // and only the last release actually turns the setting back off.
        git::release_worktree_config(&repo).await.ok();
        state.enabled_worktree_config = false;
    }
    state.save()?;
    Ok(removed)
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
    use std::time::Duration;

    // `round_is_clean` is the exact decision this task fixed: a round with a
    // seat that never answered must not read the same as a round every seat
    // actually reviewed. These are deterministic and process-free by design —
    // the equivalent end-to-end check (a real reviewer timing out under a
    // live graph run) is a genuine race against wall-clock contention, and a
    // spawn slow enough to blow even a generous budget under a loaded test
    // run must not turn this specific regression check flaky.

    #[test]
    fn a_full_panel_that_found_nothing_is_clean() {
        assert!(round_is_clean(0, true, 2, 2, IncompleteReviewPolicy::Block));
    }

    #[test]
    fn a_missing_seat_is_never_clean_under_the_default_policy() {
        assert!(!round_is_clean(
            0,
            true,
            1,
            2,
            IncompleteReviewPolicy::Block
        ));
    }

    #[test]
    fn warn_policy_still_refuses_a_missing_seat_with_open_findings() {
        assert!(!round_is_clean(1, true, 1, 2, IncompleteReviewPolicy::Warn));
    }

    #[test]
    fn warn_policy_gates_a_missing_seat_once_what_answered_is_clean() {
        assert!(round_is_clean(0, true, 1, 2, IncompleteReviewPolicy::Warn));
    }

    #[test]
    fn a_full_panel_with_an_open_finding_is_not_clean() {
        assert!(!round_is_clean(
            1,
            true,
            2,
            2,
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
            IncompleteReviewPolicy::Block
        ));
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
            reviews: Vec::new(),
            e2e: vec![CommandOutcome {
                command: "test".to_owned(),
                code: Some(if e2e_ok { 0 } else { 1 }),
                output_tail: String::new(),
                duration_ms: 0,
            }],
            verify_retried: false,
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
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
        }
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
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: true,
            verify_retried: false,
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
        }];
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
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
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
            reviews: Vec::new(),
            e2e: Vec::new(),
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: true,
            verify_retried: false,
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
        }];
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
            },
            sem: Arc::new(Semaphore::new(1)),
            pause: Pause::new(),
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
            }],
            verify_retried: false,
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
}
