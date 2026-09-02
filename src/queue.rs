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
pub const SCHEMA: u32 = 1;

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
    /// When the task was filed.
    pub created_at: Timestamp,
    /// Last change to this file.
    pub updated_at: Timestamp,
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
            status: TaskStatus::Queued,
            attempts: 0,
            runs: Vec::new(),
            last_error: None,
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
    }

    /// Record a successful run.
    pub fn succeed(&mut self) {
        self.status = TaskStatus::Done;
        self.last_error = None;
    }

    /// Record a failed attempt. Out of attempts means held for a human, rather
    /// than retried until the money runs out.
    pub fn fail(&mut self, why: impl Into<String>, max_attempts: usize) {
        self.last_error = Some(why.into());
        self.status = if self.attempts >= max_attempts {
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
        self.attempts = self.attempts.saturating_sub(1);
        self.status = TaskStatus::Failed;
    }

    /// Take this task out of the loop's reach without deleting it.
    pub fn hold(&mut self) {
        self.status = TaskStatus::Held;
    }

    /// Put a held or finished task back in line, with its attempt count reset
    /// so a release is a real second chance rather than an instant re-hold.
    /// The run history is kept: attempts reset, evidence does not.
    pub fn release(&mut self) {
        self.status = TaskStatus::Queued;
        self.attempts = 0;
        self.last_error = None;
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

    /// Delete a task. The only destructive operation in here, and the web UI
    /// does not expose it.
    pub fn remove(&self, id: &str) -> Result<String> {
        let resolved = self.resolve_id(id)?;
        let path = self.path_of(&resolved);
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        Ok(resolved)
    }

    /// Every task on disk, newest first. Unreadable files are skipped rather
    /// than fatal: one corrupt task must not take the queue - or the web UI,
    /// or an unattended daemon - down with it.
    pub fn list(&self) -> Vec<Task> {
        let mut tasks: Vec<Task> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| read_path(&p).ok())
            .collect();
        tasks.sort_unstable_by(|a, b| b.id.cmp(&a.id));
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
        let path = self.root.join(format!("{id}.lock"));
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

    /// Newest modification time in the queue, in milliseconds, for change
    /// detection. The web UI compares this instead of re-reading every task,
    /// so an idle phone on a slow link costs one `stat` per file rather than
    /// the whole backlog.
    pub fn revision(&self) -> u64 {
        std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter_map(|m| m.modified().ok())
            .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .max()
            .unwrap_or(0)
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
    if task.schema != SCHEMA {
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
        c.hold();
        q.put(&mut c).unwrap();
        // ...then oldest, so a burst of new work cannot starve older work.
        assert_eq!(q.next_runnable().unwrap().id, a.id);
        assert_eq!(q.list().len(), 3, "b is still waiting its turn");
    }

    #[test]
    fn a_held_task_is_never_offered_to_the_loop() {
        let (_dir, q) = queue();
        let mut t = task("held");
        q.put(&mut t).unwrap();
        assert!(q.next_runnable().is_some());

        t.hold();
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
    fn a_task_from_a_future_schema_is_refused_rather_than_guessed_at() {
        let (_dir, q) = queue();
        let mut t = task("from the future");
        q.put(&mut t).unwrap();
        let path = q.path_of(&t.id);
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"schema\": 1", "\"schema\": 99");
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
    fn removing_a_task_takes_it_out_of_the_listing() {
        let (_dir, q) = queue();
        let mut t = task("delete me");
        q.put(&mut t).unwrap();
        let removed = q.remove(t.short()).unwrap();
        assert_eq!(removed, t.id, "a prefix resolves before deleting");
        assert!(q.list().is_empty());
        assert!(q.remove(&t.id).is_err(), "removing twice is an error");
    }
}
