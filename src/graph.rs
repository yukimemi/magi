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
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use jiff::Timestamp;
use tokio::sync::Semaphore;

use crate::agent::{self, AgentOutput, Invocation, SeatState};
use crate::blind;
use crate::config::{AgentSpec, Config, LeakPolicy, MergeMode, ResolvedRoles};
use crate::git;
use crate::prompt::{self, CandidateView, Turn};
use crate::run::{
    Candidate, CommandOutcome, DeliberationRound, DeliberationTurn, FixRecord, Judgement,
    MergeOutcome, QuotaLoss, ReviewRecord, ReviewRound, RunState, RunStatus, Tally, VoteRecord,
    tail, write_artifact,
};
use crate::verdict::{self, FinalVote, FixReport, Position, Ranking, Review, Severity};

/// How much verification output is kept and fed back to the fixer.
const OUTPUT_TAIL: usize = 8_000;

/// One queued agent invocation.
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
/// attempt on it.
enum AgentOutcome {
    /// A usable output.
    Ok(AgentOutput),
    /// The CLI ran out of quota / rate limit. Retrying now is pointless.
    Quota(AgentOutput),
    /// Any other failure: a timeout, a bad exit code, an empty reply.
    Failed(String),
}

/// Drives one run.
pub struct Runner {
    /// Run state; public so the CLI can report on it.
    pub state: RunState,
    roles: ResolvedRoles,
    sem: Arc<Semaphore>,
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
        if !git::is_clean(&repo).await? {
            let dirty = git::status_porcelain(&repo).await?;
            bail!(
                "{} has uncommitted changes; candidates branch off HEAD and \
                 would silently exclude them:\n{dirty}",
                repo.display()
            );
        }
        let base_branch = match config.merge.base.clone() {
            Some(b) => b,
            None => git::current_branch(&repo)
                .await?
                .context("HEAD is detached; set [merge] base in magi.toml")?,
        };
        let base_commit = git::rev_parse(&repo, "HEAD").await?;
        let roles = config.resolve_roles()?;
        let max_parallel = config.graph.max_parallel.max(1);
        let mut state = RunState::new(repo, base_branch, base_commit, instruction, config);
        state.event("start", format!("run {} created", state.id));
        state.save()?;
        Ok(Self {
            state,
            roles,
            sem: Arc::new(Semaphore::new(max_parallel)),
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
        let base_commit = git::rev_parse(&repo, &base_branch).await?;

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
            tie_break: Some("review-only run: nothing competed".to_owned()),
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
        })
    }

    /// Walk the graph to a terminal state, skipping nodes already recorded.
    pub async fn execute(&mut self) -> Result<()> {
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
        self.prep().await?;
        self.implement().await?;
        self.judge().await?;
        self.deliberate().await?;
        self.vote().await?;
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

    /// The tail of the graph after a trustworthy tally: fold losers, review,
    /// gate, merge, and persist.
    async fn finish_after_tally(&mut self) -> Result<()> {
        self.fold_losers().await?;
        self.review_loop().await?;
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
            if git::enable_worktree_config(&repo).await? {
                self.state.enabled_worktree_config = true;
            }
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
        let results = wave(jobs, Arc::clone(&self.sem)).await;

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
            bail!("no candidate produced a change; nothing to judge");
        }
        self.state.status = RunStatus::Judging;
        self.state.save()?;
        Ok(())
    }

    // --------------------------------------------------------------- judge

    async fn judge(&mut self) -> Result<()> {
        if !self.state.judgements.is_empty() {
            return Ok(());
        }
        self.state.status = RunStatus::Judging;
        let viable: Vec<Candidate> = self.state.viable().into_iter().cloned().collect();
        if viable.len() == 1 {
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
        let results = ask_json_wave::<Ranking>(
            jobs,
            Arc::clone(&self.sem),
            self.state.config.graph.retries,
            "judge",
            &mut quota_losses,
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
                let (updated, out) = run_one(job, Arc::clone(&self.sem)).await;
                seat = updated;
                let agent_id = seat.agent.clone();
                let seat_key = seat.key.clone();
                self.state.seats.insert(seat.key.clone(), seat);
                let body = match out {
                    AgentOutcome::Ok(o) => verdict::section(&o.text, "position").unwrap_or(o.text),
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
        let results = ask_json_wave::<FinalVote>(
            jobs,
            Arc::clone(&self.sem),
            self.state.config.graph.retries,
            "vote",
            &mut quota_losses,
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
        // for a healthy one. A one-candidate run needs no panel at all.
        let judges_total = self.roles.judges.len();
        let needs_quorum = viable.len() > 1;
        let quorum = if needs_quorum {
            judges_total / 2 + 1
        } else {
            0
        };
        let met_quorum = !needs_quorum || present >= quorum;

        self.state.event(
            "tally",
            format!(
                "winner {winner} — votes {} | initial {} | {} changed | {present}/{judges_total} judges{}",
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
        // Absent seats = quota-lost at any node, or failed outright. Mirroring
        // `tally`'s presence test (rather than the old quota-judge/vote filter)
        // is what keeps a non-quota collapse — or a quota loss recorded at the
        // deliberate node — from being a permanent dead-end on `--resume`.
        let quota_seats: BTreeSet<&str> = self
            .state
            .quota
            .iter()
            .map(|q| q.seat.as_str())
            .collect();
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
        let results = ask_json_wave::<Ranking>(
            judge_jobs,
            Arc::clone(&self.sem),
            self.state.config.graph.retries,
            "judge",
            &mut judge_losses,
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
        let votes = ask_json_wave::<FinalVote>(
            vote_jobs,
            Arc::clone(&self.sem),
            self.state.config.graph.retries,
            "vote",
            &mut vote_losses,
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

    // --------------------------------------------------------------- review

    async fn review_loop(&mut self) -> Result<()> {
        let Some(winner) = self.state.winner().cloned() else {
            return Ok(());
        };
        let max_rounds = self.state.config.graph.review_rounds;
        if max_rounds == 0 || self.state.reviews.iter().any(|r| r.clean) {
            self.state.status = RunStatus::Gating;
            self.state.save()?;
            return Ok(());
        }
        self.state.status = RunStatus::Reviewing;

        let repo = self.state.repo.clone();
        let root = self.state.worktree_root();
        let language = self.state.config.graph.language.clone();
        let sessions = self.state.config.graph.sessions;
        let artifacts = agent::artifacts_dir(&self.state.dir());
        let base_short = short(&self.state.base_commit);
        let reviewers = self.roles.reviewers.clone();
        let base = self.state.base_commit.clone();
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
            let results = ask_json_wave::<Review>(
                jobs,
                Arc::clone(&self.sem),
                self.state.config.graph.retries,
                "review",
                &mut quota_losses,
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
                    failed: None,
                    duration_ms: 0,
                };
                match res {
                    Ok((review, out)) => {
                        record.summary = review.summary;
                        record.duration_ms = out.duration_ms;
                        for (n, mut f) in review.findings.into_iter().enumerate() {
                            // ids are magi's, never the agent's: the fixer's
                            // adoption report is keyed by them.
                            f.id = format!("R{round}-{}-{}", r + 1, n + 1);
                            all_findings.push(f.clone());
                            record.findings.push(f);
                        }
                        self.state.event(
                            "review",
                            format!(
                                "round {round}: reviewer {} raised {} finding(s)",
                                r + 1,
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

            let e2e = run_commands(
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
                        "round {round}: `{}` -> {}",
                        o.command,
                        if o.ok() {
                            "pass".to_owned()
                        } else {
                            format!("FAIL ({:?})", o.code)
                        }
                    ),
                );
            }
            let e2e_failures: String = e2e
                .iter()
                .filter(|o| !o.ok())
                .map(|o| format!("$ {}\n{}\n", o.command, o.output_tail))
                .collect();

            let blocking = all_findings.iter().filter(|f| f.severity.blocks()).count();
            let clean = blocking == 0 && e2e.iter().all(CommandOutcome::ok);

            let mut round_record = ReviewRound {
                round,
                head: head.clone(),
                reviews: records,
                e2e,
                fix: None,
                blocking,
                clean,
            };

            if clean {
                self.state.event(
                    "review",
                    format!("round {round}: clean — no blocking findings, verification green"),
                );
                self.state.reviews.push(round_record);
                self.state.status = RunStatus::Gating;
                self.state.save()?;
                return Ok(());
            }

            if round == max_rounds {
                self.state.reviews.push(round_record);
                self.state.status = RunStatus::Blocked;
                self.state.event(
                    "review",
                    format!("{blocking} blocking finding(s) still open after {max_rounds} rounds"),
                );
                self.state.save()?;
                return Ok(());
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
            let (seat, out) = run_one(job, Arc::clone(&self.sem)).await;
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
            self.state.event(
                "fix",
                format!(
                    "round {round}: {} addressed, {} rejected, {}",
                    fix.addressed.len(),
                    fix.rejected.len(),
                    if fix.committed {
                        "committed"
                    } else {
                        "NO new commit"
                    }
                ),
            );
            let stalled = !fix.committed;
            round_record.fix = Some(fix);
            self.state.reviews.push(round_record);
            self.state.save()?;

            prev_e2e = (!e2e_failures.is_empty()).then_some(e2e_failures);

            if stalled {
                self.state.status = RunStatus::Blocked;
                self.state.event(
                    "review",
                    "the fixer produced no commit; stopping instead of looping on an unchanged tree"
                        .to_owned(),
                );
                self.state.save()?;
                return Ok(());
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------------- gate

    async fn gate(&mut self) -> Result<()> {
        if self.state.status == RunStatus::Blocked || self.state.status == RunStatus::Failed {
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
                        format!("FAIL ({:?})", o.code)
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
        if self.state.status.done() && self.state.status != RunStatus::Ready {
            return Ok(());
        }
        let Some(winner) = self.state.winner().cloned() else {
            return Ok(());
        };
        let repo = self.state.repo.clone();
        let base = self.state.base_branch.clone();
        let mode = self.state.config.merge.mode;
        let message = format!(
            "Merge magi run {} (candidate {})\n\n{}",
            self.state.id, winner.label, self.state.instruction
        );

        let outcome = match mode {
            MergeMode::None => MergeOutcome {
                mode,
                ok: true,
                detail: format!("git -C {} merge --no-ff {}", repo.display(), winner.branch),
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
                    let out = git::merge_no_ff(&repo, &winner.branch, &message).await?;
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

/// Run one job, honouring the parallelism budget.
async fn run_one(job: SeatJob, sem: Arc<Semaphore>) -> (SeatState, AgentOutcome) {
    let (_, seat, out) = wave(vec![job], sem)
        .await
        .pop()
        .expect("one job in, one result out");
    (seat, out)
}

/// Run every job concurrently, capped by the semaphore, preserving order.
async fn wave(jobs: Vec<SeatJob>, sem: Arc<Semaphore>) -> Vec<(usize, SeatState, AgentOutcome)> {
    let mut set = tokio::task::JoinSet::new();
    for (i, job) in jobs.into_iter().enumerate() {
        let sem = Arc::clone(&sem);
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
                },
            )
            .await;
            let out = match out {
                Ok(o) if o.usable() => AgentOutcome::Ok(o),
                Ok(o) if o.quota_exhausted() => AgentOutcome::Quota(o),
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
            Err(e) => {
                tracing::error!("agent task panicked: {e}");
                continue;
            }
        };
        if collected.len() <= i {
            collected.resize_with(i + 1, || None);
        }
        collected[i] = Some((i, seat, out));
    }
    collected.into_iter().flatten().collect()
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
    node: &str,
    losses: &mut Vec<QuotaLoss>,
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
            let prompt = if attempt == 0 {
                src.prompt.clone()
            } else {
                let why = done[i]
                    .as_ref()
                    .and_then(|r| r.as_ref().err().map(ToString::to_string))
                    .unwrap_or_else(|| "no parsable answer".to_owned());
                let nudge = prompt::nudge(&why);
                if has_context(&src.spec, &seats[i], src.sessions) {
                    nudge
                } else {
                    format!("{}\n\n---\n\n{}", src.prompt, nudge)
                }
            };
            batch.push(SeatJob {
                spec: src.spec.clone(),
                seat: seats[i].clone(),
                cwd: src.cwd.clone(),
                prompt,
                timeout: src.timeout,
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

        let results = wave(batch, Arc::clone(&sem)).await;
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
                        node: node.to_owned(),
                        at: Timestamp::now(),
                        reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                    });
                    (
                        Err(anyhow::anyhow!("rate limited (quota); not retrying now")),
                        true,
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
        git::disable_worktree_config(&repo).await.ok();
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
