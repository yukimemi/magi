//! The conductor: a single agent seat that arranges the queue.
//!
//! [`crate::queue`] orders runnable tasks by `priority` alone; nothing in it
//! can express that one task should wait for another, or that a task whose
//! last run stopped short deserves a second look before the loop blindly
//! retries it. Once per polling cycle, [`Conductor::maybe_run`] shows one
//! agent seat three things - the runnable tasks, the tasks stuck `running`
//! with no live daemon behind them, and the `failed`/`held` tasks nobody has
//! decided about yet - and asks it to decide `blocked_by` for the first and a
//! recovery for the other two. Everything else about the loop - which
//! unblocked task runs next, one at a time, in `priority` order - is
//! unchanged; see `crate::daemon`.
//!
//! # What the conductor may not do
//!
//! [`Decision`] has no field for `priority`, for deleting a task, or for
//! touching git, a worktree, or a branch directly. [`Recovery::Review`] only
//! ever reopens a branch `crate::daemon` itself resolved from the task's own
//! run record ([`surviving_branch`]) - never a name the model wrote - through
//! `crate::graph::Runner::review`, which reviews and verifies but never
//! rewrites history.
//!
//! # Non-blocking by construction
//!
//! [`crate::ask::ask_and_wait`] is never called from here, and the prompt
//! tells the model the same: that CLI command blocks until a human answers,
//! and calling it from inside the conductor's own invocation would park the
//! whole polling loop behind one task's question. Instead a decision that
//! wants the operator's judgement carries a `question` field, and [`apply`]
//! files it with [`Question::new`] and [`Questions::put`] and moves on in the
//! same call.
//!
//! # Fails soft, always
//!
//! [`Conductor::maybe_run`] never returns an error: an unusable roster, a
//! timed-out invocation, or a reply [`verdict::extract_json`] cannot parse are
//! all logged and treated as "this cycle changes nothing." `crate::daemon`'s
//! loop always falls through to its own `Queue::next_runnable` regardless of
//! what happened here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::agent::{self, Invocation, SeatState};
use crate::ask::{Question, Questions};
use crate::config::Config;
use crate::prompt;
use crate::queue::{Queue, Task, TaskStatus};
use crate::run::RunState;
use crate::verdict;

/// Seat name for the conductor's own CLI-side conversation, scoped away from
/// every other seat magi ever opens - the same rule every other seat follows.
const SEAT: &str = "conduct";

/// Node name reported to the invoked agent (`MAGI_NODE`) and recorded on any
/// question it files, so an operator reading the questions list can tell a
/// conductor's question from one a run's own agent asked.
pub const NODE: &str = "conduct";

/// Wall-clock limit for one conductor turn. The conductor reads a queue
/// listing and replies with json; it does not implement anything or run a
/// build, so this is short - the same order of magnitude as
/// `crate::chat`'s own single-turn, no-write invocations.
const TURN_TIMEOUT: Duration = Duration::from_secs(300);

/// What the conductor may choose for a `running`-but-stalled or a
/// `failed`/`held` task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Recovery {
    /// Put it back in line, attempts reset - the same effect as
    /// `magi task release`.
    Requeue,
    /// Leave it for a human, unchanged otherwise - the same effect as
    /// `magi task hold`.
    Hold,
    /// Reopen the task's surviving branch as a review-only pass
    /// (`crate::graph::Runner::review`) instead of competing from scratch.
    /// Only takes effect when [`surviving_branch`] can actually name one;
    /// otherwise `crate::daemon` falls back to [`Recovery::Requeue`].
    Review,
}

/// The conductor's decision for one task. Deliberately has no `priority`
/// field: see this module's doc.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Decision {
    /// Task id, expected to be copied verbatim from what it was shown.
    pub id: String,
    /// Ids this task should be blocked on. Meaningful only for a runnable
    /// (`queued`) task; ignored otherwise.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// One line explaining the block or the recovery.
    #[serde(default)]
    pub reason: Option<String>,
    /// Recovery for a stalled or finished task. Ignored for a runnable one.
    #[serde(default)]
    pub recovery: Option<Recovery>,
    /// A question for the operator. When present, [`apply`] files it (unless
    /// one is already open for this task) and blocks the task on its id
    /// instead of acting on `blocked_by` or `recovery`.
    #[serde(default)]
    pub question: Option<String>,
    /// Fixed answers for `question`, if it has any. Empty means free text.
    #[serde(default)]
    pub choices: Vec<String>,
}

/// The conductor's whole reply for one cycle.
///
/// `decisions` is deliberately **not** `#[serde(default)]`, unlike every
/// other field in this module. [`verdict::extract_json`] disambiguates
/// between several balanced `{...}` spans in one reply by trying the type the
/// caller wants against each of them, last first, and keeping the first that
/// fits - which only works when a span that is not really the answer can
/// fail to fit. A `Verdict` with no required field at all would make every
/// span fit, including a `{}` left by stray trailing prose, and the reply's
/// real `decisions` - earlier in the text - would never be reached. Requiring
/// the key costs nothing: the prompt already asks for it on every reply, `[]`
/// included.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Verdict {
    /// One entry per task the conductor chose to say something about. A task
    /// left out of this list is left exactly as it was.
    pub decisions: Vec<Decision>,
}

/// A view of a task built for [`prompt::conduct`], shared by the runnable and
/// stalled sections.
fn view(t: &Task, max_attempts: usize) -> prompt::ConductTask {
    prompt::ConductTask {
        id: t.id.clone(),
        title: t.title.clone(),
        instruction: t.instruction.clone(),
        repo: t.repo.display().to_string(),
        priority: t.priority,
        status: t.status.as_str().to_owned(),
        attempts: t.attempts,
        max_attempts,
        last_error: t.last_error.clone(),
        blocked_by: t.blocked_by.clone(),
        answers: t
            .answers
            .iter()
            .map(|a| prompt::ConductAnswer {
                question: a.question.clone(),
                answer: a.answer.clone(),
            })
            .collect(),
    }
}

/// Severity as a lowercase word, matching how `crate::verdict::Severity` is
/// spelled everywhere else an operator or a model reads it.
fn severity_str(s: crate::verdict::Severity) -> &'static str {
    match s {
        crate::verdict::Severity::Nit => "nit",
        crate::verdict::Severity::Minor => "minor",
        crate::verdict::Severity::Major => "major",
        crate::verdict::Severity::Blocker => "blocker",
    }
}

/// The branch a task's last run left behind, if the tally ever ran on it -
/// what [`Recovery::Review`] reopens. Derived from `crate::run::RunState`
/// alone, never from anything the conductor wrote, so a hallucinated branch
/// name can never reach `crate::graph::Runner::review`.
fn surviving_branch(task: &Task) -> Option<String> {
    let last = task.runs.last()?;
    let state = RunState::load(last).ok()?;
    state.winner().map(|c| c.branch.clone())
}

/// Everything the conductor is shown about a `failed`/`held` task's last run.
async fn outcome_for(task: &Task, repo: &Path) -> prompt::ConductOutcome {
    let Some(run_id) = task.runs.last().cloned() else {
        return prompt::ConductOutcome {
            run_id: "(none)".to_owned(),
            unreadable: Some("this task has not produced a run yet".to_owned()),
            run_status: None,
            open_findings: Vec::new(),
            rounds_used: 0,
            rounds_max: 0,
            rounds: Vec::new(),
            branch: None,
            branch_head: None,
        };
    };
    let state = match RunState::load(&run_id) {
        Ok(s) => s,
        Err(e) => {
            // The exact failure this feature exists to stop hiding: a schema
            // mismatch (or any other unreadable state) must never be treated
            // as "nothing to recover" - it is surfaced here, verbatim, rather
            // than swallowed into a quiet re-competition.
            tracing::warn!(
                "conductor: could not read run {run_id} for task {}: {e:#}",
                task.short()
            );
            return prompt::ConductOutcome {
                run_id,
                unreadable: Some(format!("{e:#}")),
                run_status: None,
                open_findings: Vec::new(),
                rounds_used: 0,
                rounds_max: 0,
                rounds: Vec::new(),
                branch: None,
                branch_head: None,
            };
        }
    };

    let finding_view = |f: &crate::verdict::Finding| prompt::ConductFinding {
        id: f.id.clone(),
        title: f.title.clone(),
        severity: severity_str(f.severity).to_owned(),
    };
    let open_findings = state
        .open_findings()
        .into_iter()
        .map(finding_view)
        .collect();
    let rounds = state
        .reviews
        .iter()
        .map(|r| prompt::ConductRound {
            round: r.round,
            findings: r
                .reviews
                .iter()
                .flat_map(|rec| rec.findings.iter())
                .map(finding_view)
                .collect(),
            addressed: r
                .fix
                .as_ref()
                .map(|fx| fx.addressed.clone())
                .unwrap_or_default(),
            rejected: r
                .fix
                .as_ref()
                .map(|fx| {
                    fx.rejected
                        .iter()
                        .map(|rej| prompt::ConductRejection {
                            id: rej.id.clone(),
                            why: rej.why.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    let branch = state.winner().map(|c| c.branch.clone());
    let branch_head = match &branch {
        Some(b) => crate::git::rev_parse(repo, b)
            .await
            .ok()
            .map(|h| h.chars().take(8).collect()),
        None => None,
    };

    prompt::ConductOutcome {
        run_id,
        unreadable: None,
        run_status: Some(state.status.as_str().to_owned()),
        open_findings,
        rounds_used: state.reviews.len(),
        rounds_max: state.config.graph.review_rounds,
        rounds,
        branch,
        branch_head,
    }
}

/// A `failed`/`held` task together with how its last run ended.
async fn finished_view(t: &Task, repo: &Path, max_attempts: usize) -> prompt::ConductFinished {
    prompt::ConductFinished {
        task: view(t, max_attempts),
        outcome: outcome_for(t, &repo_for(t, repo)).await,
    }
}

/// The repository containing a task's branch. A task filed without a
/// repository uses the daemon's repository, exactly as its later attempt does.
fn repo_for(task: &Task, fallback: &Path) -> PathBuf {
    if task.repo.as_os_str().is_empty() || task.repo == Path::new(".") {
        fallback.to_path_buf()
    } else {
        task.repo.clone()
    }
}

/// Apply one decision to the queue and the question store.
///
/// Takes the task's own claim before touching it: a model call spans a whole
/// agent turn, and the queue can have moved on by the time its answer comes
/// back. A claim that cannot be taken means something else owns this task
/// right now - most often a live daemon mid-competition on it - so the
/// conductor's now-stale view of it is dropped rather than raced against; see
/// `crate::queue::Queue::claim`'s own doc on why a claim is proof, not a
/// guess.
///
/// A decision is matched against the task's *current* status, re-read under
/// the claim, not against whichever section of the prompt it came from: a
/// `blocked_by` only takes effect on a `queued` task, and `recovery` only on
/// one `running` (stalled) or `failed`/`held`, so a decision that no longer
/// matches what the task actually is - it moved on between the read that
/// built the prompt and this write - changes nothing.
fn apply_one(queue: &Queue, questions: &Questions, d: &Decision) -> Result<()> {
    let _claim = queue
        .claim(&d.id)
        .with_context(|| format!("task {} is claimed elsewhere right now", d.id))?;
    let mut task = queue.get(&d.id).context("no such task")?;

    if let Some(text) = &d.question {
        if task.status == TaskStatus::Done {
            return Ok(());
        }
        let question_id = match questions.open_for(&task.id).into_iter().next() {
            Some(existing) => existing.id,
            None => {
                let mut q = Question::new(
                    task.id.clone(),
                    NODE.to_owned(),
                    SEAT.to_owned(),
                    text.clone(),
                    d.reason.clone().unwrap_or_default(),
                    d.choices.clone(),
                );
                questions.put(&mut q)?;
                q.id
            }
        };
        task.block(vec![question_id], d.reason.clone());
        return queue.put(&mut task);
    }

    match task.status {
        TaskStatus::Queued if !d.blocked_by.is_empty() => {
            task.block(d.blocked_by.clone(), d.reason.clone());
            queue.put(&mut task)?;
        }
        TaskStatus::Running => match d.recovery {
            Some(Recovery::Requeue) => {
                task.release();
                queue.put(&mut task)?;
            }
            Some(Recovery::Hold) => {
                task.hold(d.reason.clone());
                queue.put(&mut task)?;
            }
            // `Review` reopens a branch, which only makes sense once a run
            // has actually stopped; a task still `running` has nothing to
            // reopen yet.
            _ => {}
        },
        TaskStatus::Failed | TaskStatus::Held => match d.recovery {
            Some(Recovery::Requeue) => {
                task.release();
                queue.put(&mut task)?;
            }
            Some(Recovery::Hold) => {
                task.hold(d.reason.clone());
                queue.put(&mut task)?;
            }
            Some(Recovery::Review) => {
                if let Some(branch) = surviving_branch(&task) {
                    task.request_review(branch);
                    queue.put(&mut task)?;
                }
                // No survivable branch: a decision naming `review` here is
                // simply not actionable, and is dropped rather than guessed
                // at - `crate::daemon` applies the same "no branch, no
                // review" rule again, from its own read, right before it
                // would actually start the run.
            }
            None => {}
        },
        // `queued` with nothing to block on, `done`, or already `blocked`:
        // nothing for this decision to do.
        _ => {}
    }
    Ok(())
}

/// Apply every decision in `verdict`. A single bad decision - a task id that
/// no longer exists, one already claimed elsewhere - is logged and skipped
/// rather than losing every other decision in the same reply.
pub fn apply(queue: &Queue, questions: &Questions, verdict: &Verdict) -> Result<()> {
    for d in &verdict.decisions {
        if let Err(e) = apply_one(queue, questions, d) {
            tracing::warn!("conductor decision for task {}: {e:#}", d.id);
        }
    }
    Ok(())
}

/// The conductor's state across polling cycles: its own CLI-side conversation
/// and the last (revision, stalled ∪ finished ids) pair it actually acted on.
#[derive(Debug, Default)]
pub struct Conductor {
    seat: Option<SeatState>,
    last_seen: Option<(u64, BTreeSet<String>)>,
}

impl Conductor {
    /// A conductor that has never run.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn snapshot(queue: &Queue, stalled: &[Task], finished: &[Task]) -> (u64, BTreeSet<String>) {
        let ids = stalled
            .iter()
            .chain(finished)
            .map(|t| t.id.clone())
            .collect();
        (queue.revision(), ids)
    }

    /// Whether calling the conductor could possibly do anything different
    /// from last time: [`Queue::revision`] moved, or the set of stalled and
    /// finished task ids changed.
    ///
    /// Deliberately **not** "stalled or finished is non-empty" - a task
    /// sitting stalled or finished with nobody changing anything about it
    /// must not be re-shown to the model every single poll forever; only a
    /// change in *which* tasks are stalled or finished, or a queue write
    /// changing something about a runnable one, is worth another look.
    ///
    /// Cheap and config-free on purpose, so `crate::daemon`'s poll loop can
    /// skip `Config::discover`'s synchronous I/O entirely on a cycle where
    /// this says no - which [`Conductor::maybe_run`] would otherwise only
    /// discover after paying for that load. Both ask the identical question,
    /// from the same [`Conductor::last_seen`], so they can never disagree
    /// about whether there is anything to look at.
    #[must_use]
    pub fn worth_a_look(&self, queue: &Queue, stalled: &[Task], finished: &[Task]) -> bool {
        self.last_seen.as_ref() != Some(&Self::snapshot(queue, stalled, finished))
    }

    /// Call the conductor once, unless nothing has changed since the last
    /// time it was worth calling - see [`Conductor::worth_a_look`], the exact
    /// same test. Never fatal - see this module's doc.
    #[allow(clippy::too_many_arguments)]
    pub async fn maybe_run(
        &mut self,
        cfg: &Config,
        repo: &Path,
        queue: &Queue,
        questions: &Questions,
        home: &Path,
        queued: &[Task],
        stalled: &[Task],
        finished: &[Task],
        max_attempts: usize,
    ) {
        let snapshot = Self::snapshot(queue, stalled, finished);
        if self.last_seen.as_ref() == Some(&snapshot) {
            return;
        }
        self.last_seen = Some(snapshot);
        if let Err(e) = self
            .run_once(
                cfg,
                repo,
                queue,
                questions,
                home,
                queued,
                stalled,
                finished,
                max_attempts,
            )
            .await
        {
            tracing::warn!("conductor: {e:#}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_once(
        &mut self,
        cfg: &Config,
        repo: &Path,
        queue: &Queue,
        questions: &Questions,
        home: &Path,
        queued: &[Task],
        stalled: &[Task],
        finished: &[Task],
        max_attempts: usize,
    ) -> Result<()> {
        if queued.is_empty() && stalled.is_empty() && finished.is_empty() {
            return Ok(());
        }

        let spec = cfg
            .resolve_roles()
            .context("resolving the conductor seat")?
            .conductor;
        let needs_new_seat = !matches!(&self.seat, Some(s) if s.agent == spec.id);
        if needs_new_seat {
            self.seat = Some(SeatState::new(SEAT, &spec.id, crate::rng::entropy()));
        }
        let seat = self.seat.as_mut().expect("just ensured a seat exists");

        let runnable_views: Vec<prompt::ConductTask> =
            queued.iter().map(|t| view(t, max_attempts)).collect();
        let stalled_views: Vec<prompt::ConductTask> =
            stalled.iter().map(|t| view(t, max_attempts)).collect();
        let mut finished_views = Vec::with_capacity(finished.len());
        for t in finished {
            finished_views.push(finished_view(t, repo, max_attempts).await);
        }

        let body = prompt::with_overlay(
            prompt::conduct(
                &runnable_views,
                &stalled_views,
                &finished_views,
                &cfg.graph.language,
            ),
            cfg.prompts.overlay(NODE),
        );

        let artifacts = home.join("conduct").join("artifacts");
        let stem = format!("turn-{}", seat.turns + 1);
        // Bound to a local: `Invocation` only borrows the cache path, and the
        // `Option<PathBuf>` `cache_dir()` returns has to outlive that borrow.
        let cache_dir = cfg.cache_dir();
        let inv = Invocation {
            cwd: repo,
            prompt: &body,
            timeout: TURN_TIMEOUT,
            // The conductor never edits anything - it only decides what
            // blocks a task and what to do about one stuck or finished.
            allow_write: false,
            sessions: cfg.graph.sessions,
            artifacts: &artifacts,
            stem: &stem,
            run: NODE,
            node: NODE,
            cache_dir: cache_dir.as_deref(),
            attachments: &[],
        };

        let out = agent::invoke(&spec, seat, &inv)
            .await
            .context("invoking the conductor")?;
        if !out.usable() {
            bail!(
                "no usable reply (exit {:?}, timed out {})",
                out.exit_code,
                out.timed_out
            );
        }
        let verdict: Verdict = verdict::extract_json(&out.text)
            .context("the conductor's reply could not be parsed")?;
        apply(queue, questions, &verdict)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;
    use crate::ask::{Answer, QuestionStatus};
    use crate::config::{AgentKind, AgentSpec, Graph};
    use crate::queue::Source;

    fn mock_agent(dir: &Path, script: &str, env: BTreeMap<String, String>) -> AgentSpec {
        let path = dir.join("mock-conduct-agent.sh");
        std::fs::write(&path, script).expect("write mock");
        AgentSpec {
            id: "mock".to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["sh".to_owned(), path.to_string_lossy().into_owned()],
            extra_args: Vec::new(),
            env,
            prompt_delivery: None,
        }
    }

    fn config(spec: AgentSpec) -> Config {
        Config {
            agents: vec![spec],
            graph: Graph {
                language: "en".to_owned(),
                ..Graph::default()
            },
            ..Config::default()
        }
    }

    fn task(title: &str) -> Task {
        Task::new(
            title.to_owned(),
            format!("do {title}"),
            std::path::PathBuf::from("."),
            Source::Human,
        )
    }

    const BROKEN: &str = "#!/bin/sh\ncat >/dev/null\nexit 3\n";
    const GARBAGE: &str = "#!/bin/sh\ncat >/dev/null\nprintf 'not json at all\\n'\n";

    fn env(reply: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("MOCK_REPLY".to_owned(), reply.to_owned())])
    }

    const REPLY: &str = "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' \"$MOCK_REPLY\"\n";

    /// A throwaway repo with one commit on `main` and a second branch ahead
    /// of it, so `outcome_for`'s own `git::rev_parse` call has a real head to
    /// resolve.
    fn init_repo_with_branch(dir: &Path, branch: &str) {
        use crate::proc::Quiet as _;
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
        run(&["checkout", "-b", branch]);
        std::fs::write(dir.join("change.txt"), "x\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "candidate work"]);
    }

    fn review_round_with_finding(
        round: usize,
        finding_id: &str,
        title: &str,
        addressed: &[&str],
        rejected: &[(&str, &str)],
    ) -> crate::run::ReviewRound {
        crate::run::ReviewRound {
            round,
            head: "deadbeef".to_owned(),
            reviews: vec![crate::run::ReviewRecord {
                reviewer: 1,
                agent: "mock".to_owned(),
                summary: String::new(),
                findings: vec![crate::verdict::Finding {
                    id: finding_id.to_owned(),
                    severity: crate::verdict::Severity::Major,
                    file: None,
                    line: None,
                    title: title.to_owned(),
                    detail: String::new(),
                }],
                vote: None,
                failed: None,
                duration_ms: 0,
            }],
            e2e: Vec::new(),
            verify_retried: false,
            fix: Some(crate::run::FixRecord {
                agent: "mock".to_owned(),
                addressed: addressed.iter().map(|s| (*s).to_owned()).collect(),
                rejected: rejected
                    .iter()
                    .map(|(id, why)| crate::verdict::Rejection {
                        id: (*id).to_owned(),
                        why: (*why).to_owned(),
                    })
                    .collect(),
                notes: String::new(),
                committed: false,
                failed: None,
                duration_ms: 0,
            }),
            blocking: 1,
            answered: 1,
            expected: 1,
            clean: false,
            progressed: true,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }
    }

    #[test]
    fn outcome_for_carries_every_rounds_findings_and_the_branch_head() {
        crate::run::set_home(std::env::temp_dir().join("magi-conduct-tests-home"));
        let dir = tempdir().unwrap();
        let default_repo = dir.path().join("default");
        let task_repo = dir.path().join("task");
        std::fs::create_dir_all(&default_repo).unwrap();
        std::fs::create_dir_all(&task_repo).unwrap();
        init_repo_with_branch(&default_repo, "other-branch");
        init_repo_with_branch(&task_repo, "magi/f00d/A");

        let mut config = Config::default();
        config.graph.review_rounds = 6;
        let mut state = crate::run::RunState::new(
            task_repo.clone(),
            "main".to_owned(),
            "deadbeef".to_owned(),
            "task".to_owned(),
            config,
        );
        state.status = crate::run::RunStatus::Blocked;
        state.candidates.push(crate::run::Candidate {
            index: 0,
            label: 'A',
            agent: "mock".to_owned(),
            branch: "magi/f00d/A".to_owned(),
            worktree: task_repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            duration_ms: 0,
            folded: false,
        });
        state.tally = Some(crate::run::Tally {
            first_choice: std::collections::BTreeMap::new(),
            borda: std::collections::BTreeMap::new(),
            winner: 'A',
            rankings: 0,
            unanimous_initial: false,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: false,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("solo".to_owned()),
        });
        state.reviews = vec![
            review_round_with_finding(
                1,
                "R1-1-2",
                "answer content is dropped",
                &[],
                &[("R1-1-2", "the id leaving blocked_by is enough")],
            ),
            review_round_with_finding(2, "R2-1-3", "answer content is still dropped", &[], &[]),
        ];
        state.save().unwrap();

        let mut t = task("outcome test");
        t.repo = task_repo;
        t.runs.push(state.id.clone());

        let finished = tokio_test_block_on(finished_view(&t, &default_repo, 2));
        let outcome = finished.outcome;

        assert!(outcome.unreadable.is_none());
        assert_eq!(outcome.run_status.as_deref(), Some("blocked"));
        assert_eq!(outcome.rounds_used, 2);
        assert_eq!(outcome.rounds_max, 6);
        assert_eq!(outcome.rounds.len(), 2);
        assert_eq!(outcome.rounds[0].findings[0].id, "R1-1-2");
        assert_eq!(outcome.rounds[0].rejected[0].id, "R1-1-2");
        assert!(outcome.rounds[1].addressed.is_empty());
        assert!(outcome.rounds[1].rejected.is_empty());
        assert_eq!(outcome.branch.as_deref(), Some("magi/f00d/A"));
        assert!(
            outcome.branch_head.is_some(),
            "a real branch must resolve a head commit: {outcome:?}"
        );
    }

    /// A tiny single-threaded block-on, so an `async fn` can be exercised
    /// from a plain `#[test]` without pulling `tokio::test`'s multi-thread
    /// runtime into a test that does no other async work.
    fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn view_carries_a_tasks_recorded_answers_into_the_conductor_prompt_input() {
        let mut t = task("answered");
        t.record_answer("Which backend?".to_owned(), "SQLite".to_owned());
        let v = view(&t, 2);
        assert_eq!(v.answers.len(), 1);
        assert_eq!(v.answers[0].question, "Which backend?");
        assert_eq!(v.answers[0].answer, "SQLite");
    }

    #[test]
    fn a_dependency_decision_blocks_the_task_and_leaves_priority_alone() {
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut a = task("a");
        a.priority = 9;
        queue.put(&mut a).unwrap();

        let verdict = Verdict {
            decisions: vec![Decision {
                id: a.id.clone(),
                blocked_by: vec!["20260101-000000-dead".to_owned()],
                reason: Some("waits on the other task".to_owned()),
                recovery: None,
                question: None,
                choices: Vec::new(),
            }],
        };
        apply(&queue, &questions, &verdict).unwrap();

        let back = queue.get(&a.id).unwrap();
        assert_eq!(back.status, TaskStatus::Blocked);
        assert_eq!(back.blocked_by, ["20260101-000000-dead"]);
        assert_eq!(
            back.priority, 9,
            "the conductor's reply cannot carry priority"
        );
    }

    #[test]
    fn a_question_decision_files_one_and_blocks_on_its_id() {
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("ambiguous");
        queue.put(&mut t).unwrap();

        let verdict = Verdict {
            decisions: vec![Decision {
                id: t.id.clone(),
                blocked_by: Vec::new(),
                reason: Some("which backend?".to_owned()),
                recovery: None,
                question: Some("Which storage backend?".to_owned()),
                choices: vec!["SQLite".to_owned(), "Redis".to_owned()],
            }],
        };
        apply(&queue, &questions, &verdict).unwrap();

        let back = queue.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Blocked);
        assert_eq!(back.blocked_by.len(), 1);
        let q = questions.get(&back.blocked_by[0]).unwrap();
        assert_eq!(q.summary, "Which storage backend?");
        assert_eq!(q.node, NODE);
        assert!(q.status.open());
    }

    #[test]
    fn a_task_with_an_open_question_already_reuses_it_rather_than_filing_a_second_one() {
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("asked once");
        queue.put(&mut t).unwrap();

        let decision = Decision {
            id: t.id.clone(),
            reason: Some("still deciding".to_owned()),
            question: Some("Which backend?".to_owned()),
            ..Decision::default()
        };
        apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![decision.clone()],
            },
        )
        .unwrap();
        assert_eq!(questions.list().len(), 1);
        let first_question_id = queue.get(&t.id).unwrap().blocked_by[0].clone();

        // An operator releasing the blocked task by hand, without answering,
        // puts it back at `Queued` while the question stays open - exactly
        // the case the guard in `apply_one` exists for: a later cycle
        // proposing the very same question must reuse it, not file a second.
        let mut released = queue.get(&t.id).unwrap();
        released.release();
        queue.put(&mut released).unwrap();

        apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![decision],
            },
        )
        .unwrap();
        assert_eq!(questions.list().len(), 1, "no duplicate question was filed");
        let after = queue.get(&t.id).unwrap();
        assert_eq!(
            after.blocked_by,
            [first_question_id],
            "the existing open question is reused, not replaced"
        );
    }

    #[test]
    fn answering_the_question_lets_the_resolver_clear_the_block_with_the_answer_kept() {
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("waits on an answer");
        queue.put(&mut t).unwrap();

        apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![Decision {
                    id: t.id.clone(),
                    blocked_by: Vec::new(),
                    reason: None,
                    recovery: None,
                    question: Some("Which backend?".to_owned()),
                    choices: Vec::new(),
                }],
            },
        )
        .unwrap();
        let blocked = queue.get(&t.id).unwrap();
        let question_id = blocked.blocked_by[0].clone();

        let mut q = questions.get(&question_id).unwrap();
        q.answer(Answer::Text("SQLite".to_owned())).unwrap();
        questions.put(&mut q).unwrap();
        assert_eq!(q.status, QuestionStatus::Answered);

        // `crate::daemon::resolve_blockers` is the deterministic resolver
        // that actually does this on the real queue; here it is enough to
        // prove the pure steps it is built from behave together.
        let mut task_after = queue.get(&t.id).unwrap();
        task_after.record_answer(q.summary.clone(), "SQLite".to_owned());
        task_after.unblock(&question_id);
        assert_eq!(task_after.status, TaskStatus::Queued);
        assert_eq!(task_after.answers[0].answer, "SQLite");
    }

    #[test]
    fn a_stalled_task_can_be_requeued_or_held() {
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));

        let mut requeue_me = task("stuck a");
        requeue_me.start("run-1".to_owned());
        queue.put(&mut requeue_me).unwrap();

        let mut hold_me = task("stuck b");
        hold_me.start("run-2".to_owned());
        queue.put(&mut hold_me).unwrap();

        apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![
                    Decision {
                        id: requeue_me.id.clone(),
                        recovery: Some(Recovery::Requeue),
                        ..Decision::default()
                    },
                    Decision {
                        id: hold_me.id.clone(),
                        recovery: Some(Recovery::Hold),
                        reason: Some("looks broken".to_owned()),
                        ..Decision::default()
                    },
                ],
            },
        )
        .unwrap();

        let requeued = queue.get(&requeue_me.id).unwrap();
        assert_eq!(requeued.status, TaskStatus::Queued);
        assert_eq!(requeued.attempts, 0);

        let held = queue.get(&hold_me.id).unwrap();
        assert_eq!(held.status, TaskStatus::Held);
        assert_eq!(held.hold_reason.as_deref(), Some("looks broken"));
    }

    #[test]
    fn review_recovery_is_a_no_op_without_a_survivable_branch() {
        // `surviving_branch` reaches `RunState::load`, which reaches the
        // process-global `run::home()` - a `OnceLock`, so this only wins the
        // race the first time it runs in the binary; every other test still
        // reaches the same directory whichever call won, and this test's own
        // run id never collides with another test's.
        crate::run::set_home(std::env::temp_dir().join("magi-conduct-tests-home"));
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("blocked with no readable run");
        t.start("20260101-000000-dead".to_owned()); // no such run on disk
        t.fail("blocked", 5);
        queue.put(&mut t).unwrap();

        apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![Decision {
                    id: t.id.clone(),
                    recovery: Some(Recovery::Review),
                    ..Decision::default()
                }],
            },
        )
        .unwrap();

        let after = queue.get(&t.id).unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Failed,
            "with nothing to reopen, the decision is dropped rather than guessed at"
        );
        assert!(after.review_branch.is_none());
    }

    #[test]
    fn recovery_is_ignored_for_a_task_that_is_not_actually_stalled_or_finished() {
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("ordinary");
        queue.put(&mut t).unwrap();

        apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![Decision {
                    id: t.id.clone(),
                    recovery: Some(Recovery::Hold),
                    ..Decision::default()
                }],
            },
        )
        .unwrap();

        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Queued);
    }

    #[tokio::test]
    async fn a_broken_agent_leaves_the_queue_untouched_and_does_not_error() {
        let dir = tempdir().unwrap();
        let cfg = config(mock_agent(dir.path(), BROKEN, BTreeMap::new()));
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("normal");
        queue.put(&mut t).unwrap();

        let mut conductor = Conductor::new();
        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[t.clone()],
                &[],
                &[],
                2,
            )
            .await;

        assert_eq!(
            queue.get(&t.id).unwrap().status,
            TaskStatus::Queued,
            "a failed invocation must change nothing"
        );
        assert!(
            queue.next_runnable().is_some(),
            "the loop must still be able to take the next task"
        );
    }

    #[tokio::test]
    async fn a_reply_with_no_json_leaves_the_queue_untouched() {
        let dir = tempdir().unwrap();
        let cfg = config(mock_agent(dir.path(), GARBAGE, BTreeMap::new()));
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("normal");
        queue.put(&mut t).unwrap();

        let mut conductor = Conductor::new();
        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[t.clone()],
                &[],
                &[],
                2,
            )
            .await;

        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Queued);
    }

    #[tokio::test]
    async fn json_survives_code_fences_and_a_preamble() {
        let dir = tempdir().unwrap();
        let mut t = task("fenced");
        let reply = format!(
            "Sure, here is my decision.\n\n```json\n{{\"decisions\":[{{\"id\":\"{}\",\
             \"blocked_by\":[\"x\"],\"reason\":\"why\"}}]}}\n```\n",
            t.id
        );
        let cfg = config(mock_agent(dir.path(), REPLY, env(&reply)));
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        queue.put(&mut t).unwrap();

        let mut conductor = Conductor::new();
        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[t.clone()],
                &[],
                &[],
                2,
            )
            .await;

        let back = queue.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Blocked);
        assert_eq!(back.blocked_by, ["x"]);
    }

    #[tokio::test]
    async fn the_conductor_is_not_called_again_when_nothing_worth_looking_at_has_changed() {
        // Each real invocation writes its own artifact stem, `turn-<n>`, so
        // whether a second one happened is read off the artifacts directory.
        let dir = tempdir().unwrap();
        let cfg = config(mock_agent(dir.path(), REPLY, env("{\"decisions\":[]}")));
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("stable");
        queue.put(&mut t).unwrap();
        let artifacts = dir.path().join("conduct").join("artifacts");
        let turn = |n: usize| artifacts.join(format!("turn-{n}.out"));

        let mut conductor = Conductor::new();
        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[t.clone()],
                &[],
                &[],
                2,
            )
            .await;
        assert!(turn(1).is_file(), "the first cycle must call the conductor");

        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[t.clone()],
                &[],
                &[],
                2,
            )
            .await;
        assert!(
            !turn(2).is_file(),
            "an unchanged revision and an unchanged stalled/finished set must not call the \
             conductor twice"
        );

        // Once the queue actually changes, the next `maybe_run` calls again.
        t.priority = 1;
        queue.put(&mut t).unwrap();
        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[t.clone()],
                &[],
                &[],
                2,
            )
            .await;
        assert!(turn(2).is_file(), "a moved revision calls it again");
    }

    #[tokio::test]
    async fn a_task_turning_stalled_calls_the_conductor_again_despite_an_unchanged_revision() {
        // The queue's own revision has not moved - nothing wrote to it - but
        // a task now looks stalled, purely because time passed. Calling
        // again here, and never again once this exact set has been shown
        // once, is the whole point of keying `worth_a_look` on the id set
        // rather than on "is it non-empty".
        let dir = tempdir().unwrap();
        let cfg = config(mock_agent(dir.path(), REPLY, env("{\"decisions\":[]}")));
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("quiet");
        queue.put(&mut t).unwrap();
        let artifacts = dir.path().join("conduct").join("artifacts");
        let turn = |n: usize| artifacts.join(format!("turn-{n}.out"));

        let mut conductor = Conductor::new();
        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[t.clone()],
                &[],
                &[],
                2,
            )
            .await;
        assert!(turn(1).is_file());

        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[],
                &[t.clone()],
                &[],
                2,
            )
            .await;
        assert!(
            turn(2).is_file(),
            "a task turning stalled must call the conductor again"
        );

        // But once shown at this exact revision, showing the *same* stalled
        // set again must not call a third time.
        conductor
            .maybe_run(
                &cfg,
                dir.path(),
                &queue,
                &questions,
                dir.path(),
                &[],
                &[t.clone()],
                &[],
                2,
            )
            .await;
        assert!(
            !turn(3).is_file(),
            "the same stalled task lingering must not call the conductor every cycle"
        );
    }

    #[test]
    fn worth_a_look_is_config_free_and_matches_maybe_runs_own_gate() {
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let mut t = task("t");
        queue.put(&mut t).unwrap();

        let mut conductor = Conductor::new();
        assert!(
            conductor.worth_a_look(&queue, &[], &[]),
            "a conductor that has never run has something to look at"
        );

        conductor.last_seen = Some(Conductor::snapshot(&queue, &[], &[]));
        assert!(
            !conductor.worth_a_look(&queue, &[], &[]),
            "nothing changed and nothing is stalled or finished"
        );
        assert!(
            conductor.worth_a_look(&queue, &[t.clone()], &[]),
            "a stalled task is worth a look even at the same revision"
        );
        assert!(
            conductor.worth_a_look(&queue, &[], &[t.clone()]),
            "a finished task is worth a look even at the same revision"
        );
    }

    #[tokio::test]
    async fn the_conduct_path_never_calls_ask_and_wait() {
        // Structural: grepping this module and `daemon.rs` for
        // `ask_and_wait` is the actual assertion this module's own doc
        // promises; this test exists so the promise has a name in the test
        // output too. `apply_one`'s question path uses `Questions::put`
        // exclusively.
        let dir = tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let questions = Questions::at(dir.path().join("questions"));
        let mut t = task("asks without blocking");
        queue.put(&mut t).unwrap();

        apply(
            &queue,
            &questions,
            &Verdict {
                decisions: vec![Decision {
                    id: t.id.clone(),
                    question: Some("ok?".to_owned()),
                    ..Decision::default()
                }],
            },
        )
        .unwrap();
        // Reaching here at all (no hang) is the assertion.
        assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Blocked);
    }
}
