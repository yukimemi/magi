//! The task queue: what magi should do next, and who asked for it.
//!
//! The queue is what lets magi run unattended. `magi serve` takes the next
//! task, runs the graph on it, records the outcome, and takes the next one.
//!
//! It is also the reason an agent can ask for work. `magi task add` is the
//! whole interface, and it is the same command whether a human types it at a
//! prompt, a phone posts it through the web UI, or an implementer inside a run
//! shells out to it because it noticed something worth doing but out of scope.
//! magi's CLI is the operating surface for both kinds of user; the queue is
//! where their intentions meet.
//!
//! One task is one JSON file under [`Queue`]'s root. Files rather than a
//! database because the operator has to be able to read, edit, and delete the
//! backlog with the tools already on the machine, and because a crashed daemon
//! must leave a queue the next one can pick up without recovery ceremony.
//!
//! # Shape
//!
//! [`Task`] is data plus *pure* state transitions - [`Task::fail`] decides
//! whether an attempt was the last one, and touches no disk. [`Queue`] owns all
//! I/O and is constructed with its root, so a test drives a real queue in a
//! temp directory without setting a process-global home. Splitting them this
//! way is why the retry policy below can be asserted directly.
//!
//! # Bounded by construction
//!
//! An autonomous loop that retries forever is a way to spend money on a task
//! that cannot succeed. Every claim increments [`Task::attempts`]; a task that
//! has burned its attempts becomes [`TaskStatus::Held`] and waits for a human
//! rather than for another agent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// On-disk format for a queued task. Bumped when a field's meaning changes.
///
/// 3: added [`HoldSource`] so conductor recovery cannot release a hold an
/// operator deliberately placed. Old records default to `None` and are
/// protected as operator-held until an explicit release; the safe direction
/// when their author was never recorded.
///
/// 2: added [`TaskStatus::Blocked`], [`Task::blocked_by`] and
/// [`Task::block_reason`] (`crate::conduct`'s decisions) and
/// [`Task::answers`] (operator answers carried forward to the next
/// conductor prompt and the next run's instruction). All three are
/// `#[serde(default)]`, so [`read_path`] accepts anything up to and
/// including this schema rather than only an exact match — a task written
/// by a build that only knew about schema 1 has nothing to say about
/// blocking or answers, and defaulting those fields is exactly as good a
/// reading as a value that build never had a chance to write.
pub const SCHEMA: u32 = 3;

/// Who placed the current hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HoldSource {
    /// An operator used the CLI or web UI.
    Manual,
    /// The daemon or conductor placed the hold as part of its own recovery.
    Machine,
}

impl HoldSource {
    /// Short human-facing label for reports and the CLI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Machine => "machine",
        }
    }
}

/// Where a task came from. Recorded because "who asked for this" is the first
/// question about an autonomous run, and the answer is not recoverable later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// A person, at a terminal or through the web UI.
    Human,
    /// An agent inside a run, via `magi task add`. Both ids are recorded so a
    /// task can be traced back to the exact seat that asked for it.
    Agent {
        /// Run the asking agent belonged to.
        run: String,
        /// Node it was working in, e.g. `implement` or `review`.
        node: String,
    },
    /// A GitHub issue, imported by number.
    Issue {
        /// Issue number.
        number: u64,
        /// `owner/repo`, as `gh` reports it.
        repo: String,
    },
}

impl Source {
    /// Short human-facing label, for lists and the web UI.
    pub fn label(&self) -> String {
        match self {
            Self::Human => "human".to_owned(),
            Self::Agent { run, node } => format!("{node}@{}", short(run)),
            Self::Issue { number, .. } => format!("issue #{number}"),
        }
    }
}

/// Where a task is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Waiting to be claimed.
    Queued,
    /// Claimed by a daemon; a run is in flight.
    Running,
    /// A run finished and its gate passed.
    Done,
    /// A run finished without passing, and attempts remain.
    Failed,
    /// Out of attempts, or held by hand. The loop will not pick it up.
    Held,
    /// Waiting on another task or an unanswered question. See
    /// [`Task::blocked_by`]. Set and cleared by `crate::conduct` and
    /// `crate::daemon`'s deterministic resolver, never by hand.
    Blocked,
}

impl TaskStatus {
    /// Is this task eligible for a daemon to claim?
    pub fn runnable(self) -> bool {
        matches!(self, Self::Queued | Self::Failed)
    }

    /// Lowercase name, as it appears on disk and in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Held => "held",
            Self::Blocked => "blocked",
        }
    }
}

/// One unit of work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// On-disk format version.
    pub schema: u32,
    /// Task id, e.g. `20260902-140501-a1b2`.
    pub id: String,
    /// One line, for lists and notifications.
    pub title: String,
    /// The task itself, handed to the graph verbatim.
    pub instruction: String,
    /// Repository to work in.
    pub repo: PathBuf,
    /// Who asked.
    pub source: Source,
    /// Higher runs first; ties break oldest-first so nothing starves.
    #[serde(default)]
    pub priority: i32,
    /// Run this task alone: one implementer, no panel of judges to convince.
    ///
    /// `#[serde(default)]` so a queue file written before this field existed
    /// still reads, as `false` - the ordinary multi-candidate competition,
    /// unchanged. A task set to `solo` still runs the whole graph; only the
    /// candidate count the daemon builds it with changes, and
    /// [`crate::graph::Runner`] already collapses a single-candidate run to
    /// implement → review → gate → merge on its own (see
    /// [`crate::graph::Runner::review`]'s doc), so nothing about judging,
    /// deliberation or voting had to change to support this.
    #[serde(default)]
    pub solo: bool,
    /// Current state.
    pub status: TaskStatus,
    /// How many times this task has been claimed.
    #[serde(default)]
    pub attempts: usize,
    /// Runs this task has produced, oldest first.
    #[serde(default)]
    pub runs: Vec<String>,
    /// Why the last attempt did not land.
    #[serde(default)]
    pub last_error: Option<String>,
    /// What a human hold is waiting on.
    ///
    /// `None` covers both the ordinary cases: a hold the loop makes itself
    /// (out of attempts, or the disk gate closed) explains itself through
    /// [`Task::last_error`] instead, and a human hold nobody bothered to
    /// explain is still a valid hold. The queue has no way to express a
    /// dependency between two tasks, so on the occasions a hold really is
    /// "wait for that other task first", this is the only place that reason
    /// survives - see [`Task::hold_manual`] and [`Task::release`].
    ///
    /// `#[serde(default)]` so a queue file written before this field existed
    /// still reads, with no reason recorded rather than a parse error.
    #[serde(default)]
    pub hold_reason: Option<String>,
    /// Who placed [`Task::hold_reason`].  `None` is a compatible old record;
    /// see [`Task::operator_held`] for its deliberately conservative meaning.
    #[serde(default)]
    pub hold_source: Option<HoldSource>,
    /// Diagnostic detail excerpted from the run that led to a hold - what a
    /// human would have found opening `artifacts/` by hand, not the one-line
    /// reason in [`Task::last_error`]. Set only when a run's own attempts are
    /// exhausted and the task becomes [`TaskStatus::Held`]; `daemon` computes
    /// it from the run's own record, since this module has no notion of a
    /// run's internals. Bounded in length by the writer - see
    /// `daemon::diagnostic` - so a verbose run cannot make this file grow
    /// without limit.
    ///
    /// `#[serde(default)]` so a queue file written before this field existed
    /// still reads, with no diagnostic recorded rather than a parse error.
    #[serde(default)]
    pub diagnostic: Option<String>,
    /// What this task is waiting on: other task ids, unanswered
    /// `crate::ask::Question` ids, or both. Non-empty exactly when
    /// [`TaskStatus::Blocked`]; emptying it — see [`Task::unblock`] — is what
    /// puts the task back at [`TaskStatus::Queued`].
    ///
    /// Set by `crate::conduct`'s decisions and cleared deterministically by
    /// `crate::daemon` as each dependency resolves, never by a person. Never
    /// `#[serde(default)]` is skipped: a queue file from before this field
    /// existed has nothing to report here, and an empty list is exactly that.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// One line explaining the current [`Task::blocked_by`], written by
    /// `crate::conduct`. Cleared whenever `blocked_by` empties.
    #[serde(default)]
    pub block_reason: Option<String>,
    /// Questions `crate::conduct` asked about this task that the operator has
    /// since answered, oldest first — what was asked, and what they said.
    ///
    /// A blocking question's id leaves [`Task::blocked_by`] the moment
    /// [`crate::ask::QuestionStatus::Answered`] is observed, but the id alone
    /// tells nobody what was decided. This is what carries the answer's
    /// *content* forward: into the next conductor prompt for this task, and
    /// into the instruction handed to the next run — see `crate::daemon`'s
    /// deterministic blocker resolution. Kept for the task's whole life, the
    /// same as [`Task::runs`]: a release resets attempts, not evidence.
    #[serde(default)]
    pub answers: Vec<AnsweredQuestion>,
    /// Set by `crate::conduct` when it chooses `Review` recovery for a task
    /// whose branch survived a blocked run: the branch to reopen with
    /// `crate::graph::Runner::review` instead of competing from scratch.
    ///
    /// Requeues the task the same way [`Task::release`] does, so it is
    /// picked up by the ordinary loop; `crate::daemon` reads this field once,
    /// when it actually starts the run, and clears it either way — consumed
    /// on success, dropped if the branch no longer exists by then. Never set
    /// from the conductor's own words: `crate::daemon` derives the branch
    /// name itself from the task's last run, so a hallucinated branch can
    /// never reach here.
    #[serde(default)]
    pub review_branch: Option<String>,
    /// A release deliberately starts a new competition instead of resuming
    /// the prior run. History remains as evidence in `runs`.
    #[serde(default)]
    pub fresh_start: bool,
    /// When the task was filed.
    pub created_at: Timestamp,
    /// Last change to this file.
    pub updated_at: Timestamp,
}

/// One question `crate::conduct` asked about a task, and what the operator
/// said back. See [`Task::answers`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsweredQuestion {
    /// The question as asked, e.g. [`crate::ask::Question::summary`].
    pub question: String,
    /// What the operator answered.
    pub answer: String,
}

impl Task {
    /// File a new task. Persist it with [`Queue::put`].
    pub fn new(title: String, instruction: String, repo: PathBuf, source: Source) -> Self {
        let now = Timestamp::now();
        Self {
            schema: SCHEMA,
            id: new_id(),
            title,
            instruction,
            repo,
            source,
            priority: 0,
            solo: false,
            status: TaskStatus::Queued,
            attempts: 0,
            runs: Vec::new(),
            last_error: None,
            hold_reason: None,
            hold_source: None,
            diagnostic: None,
            blocked_by: Vec::new(),
            block_reason: None,
            answers: Vec::new(),
            review_branch: None,
            fresh_start: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// Short form used in reports, matching a run's short id.
    pub fn short(&self) -> &str {
        short(&self.id)
    }

    /// Record that a run has started for this task.
    pub fn start(&mut self, run: String) {
        self.status = TaskStatus::Running;
        self.attempts += 1;
        self.runs.push(run);
        self.last_error = None;
        self.fresh_start = false;
    }

    /// Record a successful run.
    ///
    /// Both `magi task done` and `POST /api/queue/{id}/done` can close a held
    /// task directly, with no release in between, so this clears
    /// `hold_reason` the same way [`Task::release`] does. Otherwise a task
    /// held for "waiting on 3ed9" and then closed as done without ever being
    /// released would still read as waiting on something in `magi task show`
    /// and on its card, after it no longer is.
    pub fn succeed(&mut self) {
        self.status = TaskStatus::Done;
        self.last_error = None;
        self.hold_reason = None;
        self.hold_source = None;
        self.diagnostic = None;
    }

    /// Record a failed attempt. Out of attempts means held for a human, rather
    /// than retried until the money runs out.
    ///
    /// Clears [`Task::diagnostic`] unconditionally: it belongs to whatever run
    /// produced it, and a caller that has one for *this* attempt sets it
    /// itself right after calling this, once it knows the task actually ended
    /// up [`TaskStatus::Held`] - see `daemon::diagnostic`. Without the clear, a
    /// task released after a diagnosed hold and then failed again for an
    /// unrelated, undiagnosed reason (a config error, say) would go on
    /// showing the previous run's diagnostic as if it explained the new one.
    pub fn fail(&mut self, why: impl Into<String>, max_attempts: usize) {
        self.last_error = Some(why.into());
        self.diagnostic = None;
        self.status = if self.attempts >= max_attempts {
            self.hold_source = Some(HoldSource::Machine);
            TaskStatus::Held
        } else {
            TaskStatus::Failed
        };
    }

    /// Record an attempt that failed for a reason the task is not responsible
    /// for - the agent CLIs ran out of quota and the judging panel collapsed.
    ///
    /// This refunds the attempt on purpose. A quota window closing at 4am must
    /// not spend the backlog's retry budget: the operator would come back to a
    /// queue of held tasks that were never actually judged, and would have to
    /// release every one by hand to find out which had a real problem. The task
    /// goes back to `Failed`, which the loop retries, so a reset quota picks the
    /// work up where it stopped.
    pub fn stall(&mut self, why: impl Into<String>) {
        self.last_error = Some(why.into());
        self.diagnostic = None;
        self.attempts = self.attempts.saturating_sub(1);
        self.status = TaskStatus::Failed;
    }

    /// Whether this held task may only be released by an operator.
    ///
    /// Old files did not record a source. Preserve every such hold rather
    /// than guessing that it was automatic and risking duplicate work. New
    /// automatic holds record [`HoldSource::Machine`] and remain recoverable.
    pub fn operator_held(&self) -> bool {
        self.status == TaskStatus::Held && !matches!(self.hold_source, Some(HoldSource::Machine))
    }

    /// Take this task out of the loop's reach by an operator action.
    pub fn hold_manual(&mut self, reason: Option<String>) {
        self.status = TaskStatus::Held;
        if reason.is_some() {
            self.hold_reason = reason;
        }
        self.hold_source = Some(HoldSource::Manual);
    }

    /// Take this task out of the loop's reach during automatic recovery.
    pub fn hold_machine(&mut self, reason: Option<String>) {
        self.status = TaskStatus::Held;
        if reason.is_some() {
            self.hold_reason = reason;
        }
        self.hold_source = Some(HoldSource::Machine);
    }

    /// Block this task on other task ids and/or open question ids, chosen by
    /// `crate::conduct`. Pure: the caller still owns writing it back with
    /// [`Queue::put`].
    pub fn block(&mut self, blocked_by: Vec<String>, reason: Option<String>) {
        self.status = TaskStatus::Blocked;
        self.blocked_by = blocked_by;
        self.block_reason = reason;
    }

    /// Remove one resolved dependency (a task id that became [`TaskStatus::Done`],
    /// or a question id that became [`crate::ask::QuestionStatus::Answered`]).
    /// Once nothing is left in [`Task::blocked_by`], the task returns to
    /// [`TaskStatus::Queued`] on its own - deciding *why* a task was blocked
    /// was `crate::conduct`'s job, but noticing a dependency resolved needs no
    /// model at all.
    ///
    /// A no-op, on purpose, for a task that is not [`TaskStatus::Blocked`]:
    /// `crate::daemon`'s deterministic resolver runs over every task on every
    /// poll, and a task that moved on for some other reason must not be
    /// dragged back to `Queued` by a stale id it still happens to carry.
    pub fn unblock(&mut self, resolved_id: &str) {
        if self.status != TaskStatus::Blocked {
            return;
        }
        self.blocked_by.retain(|id| id != resolved_id);
        if self.blocked_by.is_empty() {
            self.status = TaskStatus::Queued;
            self.block_reason = None;
        }
    }

    /// Record that a question `crate::conduct` asked about this task has been
    /// answered, so the answer's content — not just the fact that the
    /// question is gone — reaches the next conductor prompt and the next
    /// run's instruction. See [`Task::answers`].
    pub fn record_answer(&mut self, question: String, answer: String) {
        self.answers.push(AnsweredQuestion { question, answer });
    }

    /// Requeue this task to reopen its last run as a review-only pass against
    /// `branch` (`crate::graph::Runner::review`) rather than competing from
    /// scratch. See [`Task::review_branch`].
    pub fn request_review(&mut self, branch: String) {
        self.release();
        self.review_branch = Some(branch);
    }

    /// Requeue after a conductor chose a new competition. Unlike an ordinary
    /// operator release, this deliberately does not resume the old run.
    pub fn requeue(&mut self) {
        self.release();
        self.fresh_start = true;
    }

    /// Change how urgently this task should run next.
    ///
    /// Refused once the task is `running`: priority only feeds the sort
    /// [`Queue::next_runnable`] does over tasks waiting to be claimed, and a
    /// running task has already left that pool. Accepting the write anyway
    /// would look like it worked while changing nothing until - and unless -
    /// this attempt fails and the task becomes runnable again, which is a
    /// surprise the phone should not hand back as a success.
    pub fn set_priority(&mut self, priority: i32) -> Result<()> {
        if self.status == TaskStatus::Running {
            bail!(
                "task {} is running; its priority cannot be changed until \
                 this attempt finishes",
                self.short()
            );
        }
        self.priority = priority;
        Ok(())
    }

    /// Replace this task's title and instruction wholesale.
    ///
    /// Restricted to `queued` and `held`. A `running` task's instruction has
    /// already been handed to the graph, so a run in flight and the file on
    /// disk must not be allowed to disagree about what was asked; a `done` or
    /// `failed` task is a record of what actually happened and editing it
    /// after the fact would falsify that record. `id`, `created_at`,
    /// `source`, and `runs` are left untouched on purpose - an edit stands in
    /// for "delete and refile", and keeping the id, the timestamp, the
    /// attribution, and the run history is the entire reason it exists
    /// instead.
    pub fn edit(&mut self, title: String, instruction: String) -> Result<()> {
        if !matches!(self.status, TaskStatus::Queued | TaskStatus::Held) {
            bail!(
                "task {} is {}; only a queued or held task's instruction can \
                 be edited",
                self.short(),
                self.status.as_str()
            );
        }
        self.title = title;
        self.instruction = instruction;
        Ok(())
    }

    /// Record a run that produced a pull request without merging it.
    ///
    /// The task is held rather than retried, and it costs no further attempt
    /// either way. The work the task asked for exists: it is sitting on a
    /// branch, in a pull request, waiting for CI or for a person. Retrying
    /// would spend the whole competition budget a second time and then race a
    /// second branch against the pull request the first one opened - which is
    /// exactly what happened to run 01c2, whose finished and green pull request
    /// was re-competed from scratch four seconds after it opened.
    ///
    /// A pull request nobody merged is a request for a person, not a failure.
    pub fn handed_off(&mut self, why: impl Into<String>) {
        self.last_error = Some(why.into());
        self.diagnostic = None;
        self.status = TaskStatus::Held;
        self.hold_source = Some(HoldSource::Machine);
    }

    /// Put a held or finished task back in line, with its attempt count reset
    /// so a release is a real second chance rather than an instant re-hold.
    /// The run history is kept: attempts reset, evidence does not.
    pub fn release(&mut self) {
        self.status = TaskStatus::Queued;
        self.attempts = 0;
        self.last_error = None;
        // Otherwise the next person who holds this task reads a reason that
        // belonged to whatever it was waiting on last time.
        self.hold_reason = None;
        self.hold_source = None;
        self.diagnostic = None;
        // A release also un-blocks: the dependency or question `blocked_by`
        // named may still be unresolved, but a human (or `crate::conduct`)
        // choosing to release the task overrides that wait outright, the same
        // as it overrides an ordinary hold.
        self.blocked_by.clear();
        self.block_reason = None;
        self.review_branch = None;
        self.fresh_start = false;
    }
}

/// A queue on disk.
#[derive(Debug, Clone)]
pub struct Queue {
    root: PathBuf,
}

impl Queue {
    /// The operator's queue, `<home>/queue`.
    pub fn open() -> Self {
        Self::at(crate::run::home().join("queue"))
    }

    /// A queue at an explicit root. Tests use this; so could an operator who
    /// wants a queue per project.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Directory holding the task files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path for one task id.
    pub fn path_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// Write a task, atomically, so a daemon killed mid-write leaves the
    /// previous state readable rather than a truncated file.
    pub fn put(&self, task: &mut Task) -> Result<()> {
        task.updated_at = Timestamp::now();
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let body = serde_json::to_string_pretty(task).context("serialize task")?;
        let path = self.path_of(&task.id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// Load a task by id or unambiguous id prefix.
    pub fn get(&self, id: &str) -> Result<Task> {
        let resolved = self.resolve_id(id)?;
        read_path(&self.path_of(&resolved))
    }

    /// Remove a task, and the claim lock that belongs to it.
    ///
    /// `in_flight` comes from the caller — a live daemon's heartbeat naming
    /// this task — because the task's own `running` status cannot answer the
    /// question. A daemon killed mid-competition leaves the status at
    /// `running` and an orphaned `.lock` behind, and a guard that trusted
    /// either would make the task undeletable for good: the phone showed
    /// exactly that, refusing a task whose daemon had been gone for an hour.
    ///
    /// So the lock is removed with the task rather than respected. Any lock
    /// still there once no live daemon claims the task is by definition stale,
    /// and leaving it would make a deleted task look claimed to
    /// [`Queue::claim`] and to whoever reads the directory.
    pub fn remove(&self, id: &str, in_flight: bool) -> Result<String> {
        let resolved = self.resolve_id(id)?;
        if in_flight {
            bail!("task {resolved} is being run by a live daemon right now");
        }
        let path = self.path_of(&resolved);
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        let lock = self.lock_path(&resolved);
        if let Err(e) = std::fs::remove_file(&lock) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e).with_context(|| format!("remove {}", lock.display()));
            }
        }
        Ok(resolved)
    }

    /// Path of the claim lock for a task. One definition, so `claim` and
    /// `remove` cannot end up naming different files.
    fn lock_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.lock"))
    }

    /// Every task on disk, highest priority first and newest first within a
    /// priority. This is what `magi task list` and `GET /api/queue` print, so
    /// a raised priority has to move a task here the moment it is saved, not
    /// only in [`Queue::next_runnable`]'s own ordering - the operator reading
    /// the backlog and the loop about to drain it must agree on what "first"
    /// means. Every existing task defaults to priority 0, so this is a no-op
    /// change from the old newest-first order for a queue nobody has
    /// reprioritised.
    ///
    /// Unreadable files are skipped rather than fatal: one corrupt task must
    /// not take the queue - or the web UI, or an unattended daemon - down
    /// with it.
    pub fn list(&self) -> Vec<Task> {
        let mut tasks: Vec<Task> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| read_path(&p).ok())
            .collect();
        tasks.sort_unstable_by(|a, b| b.priority.cmp(&a.priority).then_with(|| b.id.cmp(&a.id)));
        tasks
    }

    /// The task a daemon should run next, or `None` when the queue is idle.
    ///
    /// Highest priority first, oldest first within a priority, so a burst of
    /// agent-filed work cannot starve the task a human filed this morning.
    pub fn next_runnable(&self) -> Option<Task> {
        let mut runnable: Vec<Task> = self
            .list()
            .into_iter()
            .filter(|t| t.status.runnable())
            .collect();
        runnable.sort_unstable_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
        runnable.into_iter().next()
    }

    /// Take exclusive ownership of a task.
    ///
    /// The lock is a `create_new` file next to the task, which is atomic on
    /// every platform magi targets. It exists so two daemons - or a daemon and
    /// a human running `magi run` - cannot drive one task into two competing
    /// runs. The returned guard releases on drop, including on panic.
    pub fn claim(&self, id: &str) -> Result<Claim> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let path = self.lock_path(id);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                // Best effort: the pid is for the human looking at a stale lock.
                let _ = writeln!(f, "{}", std::process::id());
                Ok(Claim { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("task {id} is already claimed ({} exists)", path.display())
            }
            Err(e) => Err(e).with_context(|| format!("lock {}", path.display())),
        }
    }

    /// Expand an id prefix to exactly one task id.
    pub fn resolve_id(&self, prefix: &str) -> Result<String> {
        if self.path_of(prefix).is_file() {
            return Ok(prefix.to_owned());
        }
        let hits: Vec<String> = self
            .list()
            .into_iter()
            .map(|t| t.id)
            .filter(|id| id.starts_with(prefix) || id.ends_with(prefix))
            .collect();
        match hits.len() {
            1 => Ok(hits.into_iter().next().expect("exactly one hit")),
            0 => bail!("no task matches `{prefix}`"),
            _ => bail!(
                "`{prefix}` matches {} tasks: {}",
                hits.len(),
                hits.join(", ")
            ),
        }
    }

    /// Change detection token for the queue.
    ///
    /// Combines file names and modification times of all task files in the
    /// queue, so adding, modifying, or deleting any task — even an older one —
    /// moves the revision and notifies connected clients via the change stream.
    /// Returns 0 when the queue is completely empty.
    pub fn revision(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};

        let mut entries: Vec<(String, u64)> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let mtime = e
                    .metadata()
                    .ok()?
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as u64;
                Some((name, mtime))
            })
            .collect();

        if entries.is_empty() {
            return 0;
        }

        entries.sort_unstable();
        let mut hasher = std::hash::DefaultHasher::new();
        for (name, mtime) in &entries {
            name.hash(&mut hasher);
            mtime.hash(&mut hasher);
        }
        let h = hasher.finish();
        if h == 0 { 1 } else { h }
    }
}

/// Exclusive ownership of a task, released on drop.
#[derive(Debug)]
pub struct Claim {
    path: PathBuf,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The first line of a task, trimmed to a title. Used when the caller gives a
/// body but no title, which is the normal case for an agent piping a file in.
pub fn title_from(instruction: &str, max: usize) -> String {
    // The first non-blank line, whatever it is. A markdown heading is the
    // task's own summary - agents pipe in `# Rework the config loader` and mean
    // exactly that - so it is preferred over the prose beneath it rather than
    // skipped as decoration. Leading list and heading markers are stripped
    // because they are syntax, not words.
    let line = instruction
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(empty task)")
        .trim_start_matches(['#', '-', '*', '>', ' '])
        .trim();
    if line.is_empty() {
        return "(empty task)".to_owned();
    }
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let head: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn read_path(path: &Path) -> Result<Task> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let task: Task =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    // Greater-than, not not-equal: every field added since schema 1 carries
    // `#[serde(default)]`, so an older task has nothing to say about it and
    // defaulting is exactly as good a reading as a value that build never had
    // a chance to write. Only a schema *ahead* of this build - a meaning it
    // cannot possibly know - is refused rather than guessed at.
    if task.schema > SCHEMA {
        bail!(
            "task {} was written by a different magi (schema {}, this build \
             speaks {SCHEMA})",
            task.id,
            task.schema
        );
    }
    Ok(task)
}

fn short(id: &str) -> &str {
    id.split('-').next_back().unwrap_or(id)
}

fn new_id() -> String {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
    let seed = crate::rng::entropy();
    format!("{stamp}-{:04x}", (seed ^ (seed >> 32)) & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue of its own, with no process-global state - which is the point of
    /// `Queue::at`, and why these can run in parallel.
    fn queue() -> (tempfile::TempDir, Queue) {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        (dir, q)
    }

    fn task(title: &str) -> Task {
        Task::new(
            title.to_owned(),
            format!("do {title}"),
            PathBuf::from("."),
            Source::Human,
        )
    }

    #[test]
    fn a_markdown_heading_is_the_title_not_decoration() {
        // A task file's heading is the summary its author already wrote, so it
        // beats the prose underneath. Getting this backwards was visible in the
        // first smoke test: a task titled "# Rework the config loader" listed
        // as "It re-reads the file on every lookup".
        assert_eq!(
            title_from("# Rework the config loader\n\nIt re-reads it.\n", 40),
            "Rework the config loader"
        );
        assert_eq!(title_from("- fix the thing", 40), "fix the thing");
        assert_eq!(title_from("> quoted task", 40), "quoted task");
        // Nothing usable at all still has to produce something printable.
        assert_eq!(title_from("   \n\n", 40), "(empty task)");
        assert_eq!(title_from("###\n", 40), "(empty task)");
    }

    #[test]
    fn a_long_title_is_elided_by_characters_not_bytes() {
        // Byte truncation would split a multi-byte character and panic.
        let long = "課題".repeat(30);
        let title = title_from(&long, 10);
        assert_eq!(title.chars().count(), 10);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn priority_wins_and_ties_break_oldest_first() {
        let (_dir, q) = queue();
        let mut a = task("first");
        let mut b = task("second");
        let mut c = task("urgent");
        // Ids carry a timestamp, so force a known order.
        a.id = "20260101-000001-aaaa".to_owned();
        b.id = "20260101-000002-bbbb".to_owned();
        c.id = "20260101-000003-cccc".to_owned();
        c.priority = 5;
        for t in [&mut a, &mut b, &mut c] {
            q.put(t).unwrap();
        }

        // Priority first...
        assert_eq!(q.next_runnable().unwrap().id, c.id);
        c.hold_machine(None);
        q.put(&mut c).unwrap();
        // ...then oldest, so a burst of new work cannot starve older work.
        assert_eq!(q.next_runnable().unwrap().id, a.id);
        assert_eq!(q.list().len(), 3, "b is still waiting its turn");
    }

    #[test]
    fn a_blocked_task_never_starves_another_runnable_one() {
        let (_dir, q) = queue();
        let mut blocked = task("blocked");
        blocked.block(vec!["something".to_owned()], None);
        q.put(&mut blocked).unwrap();

        let mut runnable = task("free to go");
        q.put(&mut runnable).unwrap();

        let next = q.next_runnable().expect("a runnable task is still offered");
        assert_eq!(next.id, runnable.id);
    }

    #[test]
    fn a_held_task_is_never_offered_to_the_loop() {
        let (_dir, q) = queue();
        let mut t = task("held");
        q.put(&mut t).unwrap();
        assert!(q.next_runnable().is_some());

        t.hold_machine(None);
        q.put(&mut t).unwrap();
        assert!(
            q.next_runnable().is_none(),
            "a held task must wait for a human"
        );

        // A failed task, by contrast, is exactly what the loop should retry.
        t.status = TaskStatus::Failed;
        q.put(&mut t).unwrap();
        assert!(q.next_runnable().is_some());
    }

    #[test]
    fn attempts_are_capped_and_then_the_task_is_held() {
        let mut t = task("doomed");

        t.start("run-1".to_owned());
        t.fail("gate red", 2);
        assert_eq!(t.status, TaskStatus::Failed, "one attempt of two: retry");

        t.start("run-2".to_owned());
        t.fail("gate red", 2);
        assert_eq!(
            t.status,
            TaskStatus::Held,
            "out of attempts: stop spending money on it"
        );
        assert_eq!(t.runs, ["run-1", "run-2"]);
        assert_eq!(t.last_error.as_deref(), Some("gate red"));
    }

    #[test]
    fn a_quota_stall_is_refunded_so_the_backlog_survives_the_night() {
        let mut t = task("stalled by quota");

        t.start("run-1".to_owned());
        assert_eq!(t.attempts, 1);
        t.stall("judge-1, judge-2 out of quota");
        assert_eq!(
            t.attempts, 0,
            "a closed quota window must not spend the task's retry budget"
        );
        assert_eq!(t.status, TaskStatus::Failed, "the loop should retry it");
        assert_eq!(
            t.last_error.as_deref(),
            Some("judge-1, judge-2 out of quota")
        );

        // A task can therefore stall all night and still get its real attempts
        // once the quota resets - which is the whole point.
        for _ in 0..20 {
            t.start("run-n".to_owned());
            t.stall("still out of quota");
        }
        t.start("run-real".to_owned());
        t.fail("gate red", 2);
        assert_eq!(
            t.status,
            TaskStatus::Failed,
            "the first attempt that was really judged is attempt one"
        );
    }

    #[test]
    fn releasing_a_held_task_gives_it_a_real_second_chance() {
        let mut t = task("retry me");
        t.start("run-1".to_owned());
        t.fail("gate red", 1);
        assert_eq!(t.status, TaskStatus::Held);

        t.release();
        assert_eq!(t.status, TaskStatus::Queued);
        // Without resetting attempts the next failure would re-hold at once,
        // and a release would be a no-op the operator cannot see.
        assert_eq!(t.attempts, 0);
        assert!(t.last_error.is_none());
        assert_eq!(
            t.runs.len(),
            1,
            "history is kept: attempts reset, evidence does not"
        );
    }

    #[test]
    fn a_hold_reason_survives_and_a_release_clears_it() {
        let mut t = task("waiting on something else");
        t.hold_manual(Some(
            "waiting for 20260101-000000-aaaa to land first".to_owned(),
        ));
        assert_eq!(t.status, TaskStatus::Held);
        assert_eq!(
            t.hold_reason.as_deref(),
            Some("waiting for 20260101-000000-aaaa to land first")
        );

        // Holding again with no reason must not erase the one already there.
        t.hold_manual(None);
        assert_eq!(
            t.hold_reason.as_deref(),
            Some("waiting for 20260101-000000-aaaa to land first"),
            "a bare re-hold keeps whatever a human already wrote down"
        );

        // A hold with no reason at all is still an ordinary, allowed hold.
        let mut plain = task("no reason given");
        plain.hold_manual(None);
        assert_eq!(plain.status, TaskStatus::Held);
        assert!(plain.hold_reason.is_none());

        t.release();
        assert_eq!(t.status, TaskStatus::Queued);
        assert!(
            t.hold_reason.is_none(),
            "a stale reason must not greet the next person who holds this task"
        );
    }

    #[test]
    fn closing_a_held_task_as_done_clears_its_hold_reason_too() {
        // `done` can close a held task directly - neither `magi task done`
        // nor `POST /api/queue/{id}/done` requires a release first - so a
        // task held for "waiting on 3ed9" and then closed without ever being
        // released must not still read as waiting on it afterwards.
        let mut t = task("landed by hand while held");
        t.hold_manual(Some("waiting on 3ed9".to_owned()));
        assert_eq!(t.hold_reason.as_deref(), Some("waiting on 3ed9"));

        t.succeed();
        assert_eq!(t.status, TaskStatus::Done);
        assert!(
            t.hold_reason.is_none(),
            "a done task cannot still be waiting on something"
        );
    }

    #[test]
    fn a_blocked_task_is_never_offered_to_the_loop() {
        let mut t = task("blocked");
        assert!(t.status.runnable());
        t.block(
            vec!["dep-id".to_owned()],
            Some("waits on dep-id".to_owned()),
        );
        assert_eq!(t.status, TaskStatus::Blocked);
        assert!(!t.status.runnable());
        assert_eq!(TaskStatus::Blocked.as_str(), "blocked");
    }

    #[test]
    fn unblocking_the_last_dependency_returns_the_task_to_queued() {
        let mut t = task("blocked on two");
        t.block(
            vec!["a".to_owned(), "b".to_owned()],
            Some("waits on a and b".to_owned()),
        );

        t.unblock("a");
        assert_eq!(t.status, TaskStatus::Blocked, "b is still outstanding");
        assert_eq!(t.blocked_by, ["b"]);

        t.unblock("b");
        assert_eq!(t.status, TaskStatus::Queued);
        assert!(t.blocked_by.is_empty());
        assert!(t.block_reason.is_none());
    }

    #[test]
    fn unblocking_an_id_on_a_task_that_is_not_blocked_is_a_no_op() {
        let mut t = task("never blocked");
        t.unblock("whatever");
        assert_eq!(t.status, TaskStatus::Queued);
    }

    #[test]
    fn answering_a_question_is_recorded_and_survives_a_release() {
        let mut t = task("asked something");
        t.block(vec!["q1".to_owned()], Some("which backend?".to_owned()));
        t.record_answer("Which backend?".to_owned(), "SQLite".to_owned());
        t.unblock("q1");
        assert_eq!(t.status, TaskStatus::Queued);
        assert_eq!(t.answers.len(), 1);
        assert_eq!(t.answers[0].answer, "SQLite");

        // A release resets attempts, not evidence - the same rule
        // `releasing_a_held_task_gives_it_a_real_second_chance` asserts for
        // `runs`.
        t.release();
        assert_eq!(t.answers.len(), 1, "the answer is not lost on release");
    }

    #[test]
    fn requesting_review_requeues_the_task_and_remembers_the_branch() {
        let mut t = task("blocked run with a surviving branch");
        t.start("run-1".to_owned());
        t.fail("blocked with major findings", 5);
        assert_eq!(t.status, TaskStatus::Failed);

        t.request_review("magi/eba2/A".to_owned());
        assert_eq!(t.status, TaskStatus::Queued);
        assert_eq!(t.attempts, 0);
        assert_eq!(t.review_branch.as_deref(), Some("magi/eba2/A"));

        // An ordinary release (a human overriding the choice) drops it again.
        t.release();
        assert!(t.review_branch.is_none());
    }

    #[test]
    fn conductor_requeue_but_not_an_ordinary_release_forces_a_fresh_start() {
        let mut t = task("retry");
        t.start("run-1".to_owned());
        t.requeue();
        assert!(t.fresh_start);

        t.release();
        assert!(!t.fresh_start);
    }

    #[test]
    fn priority_can_be_changed_while_queued_but_not_while_running() {
        let mut t = task("reprioritise me");
        t.set_priority(5).unwrap();
        assert_eq!(t.priority, 5);

        t.start("run-1".to_owned());
        let err = t.set_priority(9).unwrap_err().to_string();
        assert!(err.contains("running"), "{err}");
        assert_eq!(t.priority, 5, "the rejected write must not partially apply");
    }

    #[test]
    fn changing_priority_moves_a_task_ahead_in_the_real_queue_order() {
        let (_dir, q) = queue();
        let mut a = task("first filed");
        let mut b = task("second filed");
        a.id = "20260101-000001-aaaa".to_owned();
        b.id = "20260101-000002-bbbb".to_owned();
        q.put(&mut a).unwrap();
        q.put(&mut b).unwrap();

        assert_eq!(
            q.next_runnable().unwrap().id,
            a.id,
            "with equal priority the older task goes first, so a burst of \
             new work cannot starve it"
        );
        assert_eq!(
            q.list()[0].id,
            b.id,
            "but the list an operator reads is newest first, the same as \
             before priority existed - a's turn to run does not make it the \
             newest task"
        );

        let mut a = q.get(&a.id).unwrap();
        a.set_priority(10).unwrap();
        q.put(&mut a).unwrap();

        assert_eq!(
            q.next_runnable().unwrap().id,
            a.id,
            "a raised priority must be reflected the moment it is saved"
        );
        // `magi task list` and `GET /api/queue` both print `Queue::list()`
        // directly, so the raised task has to lead there too - not only in
        // what the loop would claim next.
        assert_eq!(
            q.list()[0].id,
            a.id,
            "the raised task must sort first in the list an operator reads, \
             not only in next_runnable's own ordering"
        );
    }

    #[test]
    fn editing_replaces_title_and_instruction_but_keeps_identity_and_history() {
        let mut t = Task::new(
            "old title".to_owned(),
            "old instruction".to_owned(),
            PathBuf::from("/repo"),
            Source::Agent {
                run: "20260101-000000-beef".to_owned(),
                node: "implement".to_owned(),
            },
        );
        let id = t.id.clone();
        let created_at = t.created_at;
        t.runs.push("20260101-000000-beef".to_owned());

        t.edit("new title".to_owned(), "new instruction".to_owned())
            .unwrap();

        assert_eq!(t.title, "new title");
        assert_eq!(t.instruction, "new instruction");
        assert_eq!(t.id, id, "editing must not mint a new id");
        assert_eq!(t.created_at, created_at);
        assert_eq!(
            t.source,
            Source::Agent {
                run: "20260101-000000-beef".to_owned(),
                node: "implement".to_owned(),
            },
            "editing must not turn agent attribution into human"
        );
        assert_eq!(t.runs, ["20260101-000000-beef"]);
    }

    #[test]
    fn editing_is_refused_once_a_task_is_running_or_finished() {
        let mut running = task("in flight");
        running.start("run-1".to_owned());
        let err = running
            .edit("x".to_owned(), "y".to_owned())
            .unwrap_err()
            .to_string();
        assert!(err.contains("running"), "{err}");

        let mut done = task("finished");
        done.succeed();
        let err = done
            .edit("x".to_owned(), "y".to_owned())
            .unwrap_err()
            .to_string();
        assert!(err.contains("done"), "{err}");

        // Both queued and held are the point of the feature and must work.
        let mut queued = task("waiting");
        queued.edit("x".to_owned(), "y".to_owned()).unwrap();
        let mut held = task("parked");
        held.hold_machine(None);
        held.edit("x".to_owned(), "y".to_owned()).unwrap();
    }

    #[test]
    fn a_task_recorded_without_a_hold_reason_still_reads_as_none() {
        let (_dir, q) = queue();
        let path = q.path_of("20260101-000000-aaaa");
        std::fs::create_dir_all(q.root()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": SCHEMA,
                "id": "20260101-000000-aaaa",
                "title": "from before hold reasons existed",
                "instruction": "from before hold reasons existed",
                "repo": ".",
                "source": { "kind": "human" },
                "status": "held",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string(),
            })
            .to_string(),
        )
        .unwrap();

        let task = q.get("20260101-000000-aaaa").expect("must still read");
        assert!(task.hold_reason.is_none());
        assert!(task.operator_held());
    }

    #[test]
    fn a_legacy_reasoned_hold_defaults_to_operator_protection() {
        let (_dir, q) = queue();
        let path = q.path_of("20260101-000000-bbbb");
        std::fs::create_dir_all(q.root()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": 2,
                "id": "20260101-000000-bbbb",
                "title": "old manual recovery",
                "instruction": "old manual recovery",
                "repo": ".",
                "source": { "kind": "human" },
                "status": "held",
                "hold_reason": "active manual recovery run20260912-224242-daf5",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string(),
            })
            .to_string(),
        )
        .unwrap();

        let task = q.get("20260101-000000-bbbb").expect("must still read");
        assert_eq!(task.hold_source, None);
        assert!(task.operator_held());
    }

    #[test]
    fn a_task_recorded_without_a_diagnostic_still_reads_as_none() {
        let (_dir, q) = queue();
        let path = q.path_of("20260101-000000-aaaa");
        std::fs::create_dir_all(q.root()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": SCHEMA,
                "id": "20260101-000000-aaaa",
                "title": "from before diagnostics existed",
                "instruction": "from before diagnostics existed",
                "repo": ".",
                "source": { "kind": "human" },
                "status": "held",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string(),
            })
            .to_string(),
        )
        .unwrap();

        let task = q.get("20260101-000000-aaaa").expect("must still read");
        assert!(task.diagnostic.is_none());
    }

    #[test]
    fn a_schema_1_task_with_no_blocking_fields_still_reads() {
        // Written by a build that predates `blocked_by`, `block_reason`,
        // `answers` and `review_branch` entirely - literal `"schema": 1`,
        // not `SCHEMA`, since the whole point is a build older than this one.
        let (_dir, q) = queue();
        let path = q.path_of("20260101-000000-aaaa");
        std::fs::create_dir_all(q.root()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": 1,
                "id": "20260101-000000-aaaa",
                "title": "from before blocking existed",
                "instruction": "from before blocking existed",
                "repo": ".",
                "source": { "kind": "human" },
                "status": "queued",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string(),
            })
            .to_string(),
        )
        .unwrap();

        let task = q.get("20260101-000000-aaaa").expect("must still read");
        assert!(task.blocked_by.is_empty());
        assert!(task.block_reason.is_none());
        assert!(task.answers.is_empty());
        assert!(task.review_branch.is_none());
    }

    #[test]
    fn releasing_or_finishing_a_task_clears_its_stale_diagnostic() {
        // A diagnostic belongs to the run that produced it. Left in place
        // across a release, an unrelated later failure - a config error, say -
        // would go on showing evidence for a problem that is no longer why the
        // task is stuck.
        let mut held = task("diagnosed");
        held.start("run-1".to_owned());
        held.fail("gate red", 1);
        held.diagnostic = Some("cargo test failed: ...".to_owned());
        assert_eq!(held.status, TaskStatus::Held);

        held.release();
        assert!(held.diagnostic.is_none());

        held.diagnostic = Some("cargo test failed: ...".to_owned());
        held.succeed();
        assert!(held.diagnostic.is_none());
    }

    #[test]
    fn failing_a_task_always_clears_whatever_diagnostic_it_carried() {
        let mut t = task("retried");
        t.start("run-1".to_owned());
        t.diagnostic = Some("stale evidence from a previous hold".to_owned());
        t.fail("unrelated config error", 5);
        assert_eq!(t.status, TaskStatus::Failed);
        assert!(
            t.diagnostic.is_none(),
            "fail() must not let an old diagnostic outlive the run that produced it"
        );
    }

    #[test]
    fn a_claim_is_exclusive_and_releases_on_drop() {
        let (_dir, q) = queue();
        let mut t = task("contended");
        q.put(&mut t).unwrap();

        let held = q.claim(&t.id).unwrap();
        assert!(
            q.claim(&t.id).is_err(),
            "two daemons must not drive one task into two runs"
        );
        drop(held);
        assert!(q.claim(&t.id).is_ok(), "a released claim is reclaimable");
    }

    #[test]
    fn a_round_trip_survives_disk() {
        let (_dir, q) = queue();
        let mut t = Task::new(
            "titled".to_owned(),
            "body".to_owned(),
            PathBuf::from("/repo"),
            Source::Agent {
                run: "20260101-000000-beef".to_owned(),
                node: "implement".to_owned(),
            },
        );
        t.priority = 3;
        q.put(&mut t).unwrap();

        let back = q.get(&t.id).unwrap();
        assert_eq!(back.id, t.id);
        assert_eq!(back.priority, 3);
        assert_eq!(back.source.label(), "implement@beef");
        // A prefix is enough, the way run ids work everywhere else.
        assert_eq!(q.get(t.short()).unwrap().id, t.id);
    }

    #[test]
    fn an_unreadable_task_does_not_take_the_queue_down() {
        let (_dir, q) = queue();
        let mut t = task("fine");
        q.put(&mut t).unwrap();
        std::fs::write(q.root().join("broken.json"), "{ not json").unwrap();

        let listed = q.list();
        assert_eq!(listed.len(), 1, "the readable task still lists");
        assert_eq!(listed[0].id, t.id);
    }

    #[test]
    fn a_task_recorded_without_a_solo_field_still_reads_as_not_solo() {
        let (_dir, q) = queue();
        let path = q.path_of("20260101-000000-aaaa");
        std::fs::create_dir_all(q.root()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": SCHEMA,
                "id": "20260101-000000-aaaa",
                "title": "from before solo existed",
                "instruction": "from before solo existed",
                "repo": ".",
                "source": { "kind": "human" },
                "status": "queued",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string(),
            })
            .to_string(),
        )
        .unwrap();

        let task = q.get("20260101-000000-aaaa").expect("must still read");
        assert!(!task.solo, "a queue file with no `solo` field means false");
    }

    #[test]
    fn a_task_from_a_future_schema_is_refused_rather_than_guessed_at() {
        let (_dir, q) = queue();
        let mut t = task("from the future");
        q.put(&mut t).unwrap();
        let path = q.path_of(&t.id);
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .replace(&format!("\"schema\": {SCHEMA}"), "\"schema\": 99");
        std::fs::write(&path, body).unwrap();

        let err = q.get(&t.id).unwrap_err().to_string();
        assert!(err.contains("schema 99"), "{err}");
    }

    #[test]
    fn revision_moves_when_the_queue_changes() {
        let (_dir, q) = queue();
        assert_eq!(q.revision(), 0, "an empty queue has no revision");
        let mut t = task("first");
        q.put(&mut t).unwrap();
        assert!(q.revision() > 0, "a written task moves the revision");
    }

    #[test]
    fn revision_moves_when_deleting_an_older_task() {
        let (_dir, q) = queue();
        let mut t1 = task("older");
        q.put(&mut t1).unwrap();
        // Ensure mtime ticks forward.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut t2 = task("newer");
        q.put(&mut t2).unwrap();

        let rev_before = q.revision();
        q.remove(&t1.id, false).unwrap();
        let rev_after = q.revision();

        assert_ne!(
            rev_before, rev_after,
            "deleting an older task must change the revision so other clients see the deletion"
        );
    }

    #[test]
    fn removing_a_task_takes_it_out_of_the_listing() {
        let (_dir, q) = queue();
        let mut t = task("delete me");
        q.put(&mut t).unwrap();
        let removed = q.remove(t.short(), false).unwrap();
        assert_eq!(removed, t.id, "a prefix resolves before deleting");
        assert!(q.list().is_empty());
        assert!(
            q.remove(&t.id, false).is_err(),
            "removing twice is an error"
        );
    }

    #[test]
    fn removing_a_task_takes_its_stale_lock_with_it() {
        let (_dir, q) = queue();
        let mut t = task("interrupted");
        q.put(&mut t).unwrap();

        // A daemon killed mid-run leaves this behind. Nothing holds it: the
        // process that would have dropped the guard is gone.
        let claim = q.claim(&t.id).unwrap();
        std::mem::forget(claim);
        assert!(
            q.claim(&t.id).is_err(),
            "the orphaned lock is what makes the task look claimed"
        );

        // A live daemon on this task is refused, whatever the lock says.
        let err = q.remove(&t.id, true).unwrap_err().to_string();
        assert!(err.contains("live daemon"), "{err}");
        assert!(q.get(&t.id).is_ok(), "a refused delete keeps the task");

        // With no daemon behind it, the lock is stale and goes with the task.
        q.remove(&t.id, false).unwrap();
        assert!(q.list().is_empty());
        let mut again = task("interrupted");
        again.id = t.id.clone();
        q.put(&mut again).unwrap();
        assert!(
            q.claim(&t.id).is_ok(),
            "a task that comes back must be claimable, which a left-behind lock would prevent"
        );
    }
}
