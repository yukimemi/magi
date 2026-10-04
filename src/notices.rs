//! Notices: what needs the operator's attention but is not a question.
//!
//! A merged run whose release bump failed, a task held after three attempts, a
//! disk gate that refused to start work - each used to end in a log line or in
//! a question-shaped record nobody could answer. A [`Notice`] is the missing
//! shape: something to *know*, with a severity, a timestamp and an optional
//! link, that the operator marks read or dismisses. It is deliberately not a
//! [`crate::ask::Question`]: nothing waits on it and there is nothing to answer.
//!
//! # Shape
//!
//! The same split as [`crate::queue`] and [`crate::ask`]. [`Notice`] is data
//! plus *pure* transitions; [`Notices`] owns every filesystem call and is
//! constructed with its root, so a test drives a real store in a temp
//! directory and nothing here is process-global.
//!
//! One notice is one JSON file, written atomically, because `magi serve`,
//! `magi web` and the CLI all write here from different processes and a rename
//! is the only cross-process write that needs no coordination. The file name is
//! derived from the notice's *key* by a stable function, which is what makes
//! deduplication lock-free: two processes raising the same problem write the
//! same file. A raise racing another may lose one increment of `count`; that is
//! accepted.
//!
//! # Deduplication, and what dismissing means
//!
//! A key names a kind and a subject (`release-bump:<run>`, `task:<id>`), never
//! the prose. Raising a key that exists bumps `count` and `last_at`. It returns
//! the notice to *unread* only when the message changed or the severity rose:
//! a retry loop re-raising the identical problem must not relight the bell.
//! Dismissal is a tombstone (`dismissed_at`) rather than a delete, so the same
//! message re-raised does not resurrect what the operator already waved away.
//! The cost is that a genuine recurrence with identical wording stays hidden
//! until the cap prunes the tombstone; producers should therefore keep
//! messages stable and put anything that varies elsewhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// On-disk format. A file is refused only when its own schema is *greater*
/// than this one; every field added later must carry `#[serde(default)]`.
pub const SCHEMA: u32 = 1;

/// The most notices kept. Older ones are pruned on every write: dismissed
/// first, then read, then unread, oldest first within each.
pub const CAP: usize = 200;

/// How serious a notice is. Ordered so a rise is a plain comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing, nothing to do.
    Info,
    /// Something needs a look soon.
    Warn,
    /// Something failed and stays failed until someone acts.
    Error,
}

/// Where a notice points, when it points anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Link {
    /// A run, by id.
    Run {
        /// The run id.
        id: String,
    },
    /// A queued task, by id.
    Task {
        /// The task id.
        id: String,
    },
    /// An external page, such as a pull request.
    Url {
        /// The address.
        url: String,
    },
}

/// One thing the operator should know.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    /// Stable file-name-safe id derived from [`Notice::key`].
    pub id: String,
    /// Deduplication key: kind and subject, never prose.
    pub key: String,
    /// How serious.
    pub severity: Severity,
    /// What happened.
    pub message: String,
    /// Where it points, when it points anywhere.
    #[serde(default)]
    pub link: Option<Link>,
    /// First raised.
    pub first_at: Timestamp,
    /// Last raised.
    pub last_at: Timestamp,
    /// How many times this key was raised.
    #[serde(default = "one")]
    pub count: u32,
    /// When it was marked read.
    #[serde(default)]
    pub read_at: Option<Timestamp>,
    /// When it was dismissed; the tombstone.
    #[serde(default)]
    pub dismissed_at: Option<Timestamp>,
    /// The task and run ids this notice is about. A question whose `run`
    /// names one of them already pages the operator for the same cause.
    #[serde(default)]
    pub subjects: Vec<String>,
    /// The question that carries this notice as context, when it was filed
    /// already read for that reason. Never a tombstone: a recurrence with a
    /// changed message or higher severity is unread again.
    #[serde(default)]
    pub covered_by: Option<String>,
    /// When the cause this notice reports began (a task's `held_at`). A
    /// question filed before it is about an older cause and does not cover
    /// it; `None` keeps the broad coverage of notices without one.
    #[serde(default)]
    pub since: Option<Timestamp>,
    /// On-disk format version.
    #[serde(default = "schema")]
    pub schema: u32,
}

fn one() -> u32 {
    1
}

fn schema() -> u32 {
    SCHEMA
}

impl Notice {
    /// A notice of `severity` for `key`.
    pub fn new(severity: Severity, key: &str, message: impl Into<String>) -> Self {
        let now = Timestamp::now();
        Self {
            id: id_of(key),
            key: key.to_owned(),
            severity,
            message: message.into(),
            link: None,
            first_at: now,
            last_at: now,
            count: 1,
            read_at: None,
            dismissed_at: None,
            subjects: Vec::new(),
            covered_by: None,
            since: None,
            schema: SCHEMA,
        }
    }

    /// Name the task / run ids this notice is about.
    pub fn about<I, S>(mut self, subjects: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.subjects = subjects.into_iter().map(Into::into).collect();
        self
    }

    /// An [`Severity::Info`] notice.
    pub fn info(key: &str, message: impl Into<String>) -> Self {
        Self::new(Severity::Info, key, message)
    }

    /// A [`Severity::Warn`] notice.
    pub fn warn(key: &str, message: impl Into<String>) -> Self {
        Self::new(Severity::Warn, key, message)
    }

    /// An [`Severity::Error`] notice.
    pub fn error(key: &str, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, key, message)
    }

    /// Record when the reported cause began.
    pub fn since(mut self, at: Option<Timestamp>) -> Self {
        self.since = at;
        self
    }

    /// Attach a link.
    pub fn link(mut self, link: Link) -> Self {
        self.link = Some(link);
        self
    }

    /// Neither read nor dismissed.
    pub fn unread(&self) -> bool {
        self.read_at.is_none() && self.dismissed_at.is_none()
    }

    /// Fold a repeat of this notice into it. Always counts; only a changed
    /// message or a higher severity makes it unread and visible again.
    pub fn raise_again(&mut self, again: &Notice, now: Timestamp) {
        self.count = self.count.saturating_add(1);
        self.last_at = now;
        if again.link.is_some() {
            self.link = again.link.clone();
        }
        if !again.subjects.is_empty() {
            self.subjects = again.subjects.clone();
        }
        let escalated = again.severity > self.severity;
        // A newer cause is news even when it reads the same; an old record
        // with no `since` just adopts one without lighting up again.
        let newer_cause = matches!((self.since, again.since), (Some(old), Some(new)) if new > old);
        if again.since.is_some() {
            self.since = again.since;
        }
        if again.message != self.message || escalated || newer_cause {
            self.message = again.message.clone();
            self.severity = self.severity.max(again.severity);
            self.read_at = None;
            self.dismissed_at = None;
            self.covered_by = None;
        }
        // A question already pages for this cause: keep the record, skip the
        // second page. A higher severity is news the question did not carry.
        if let Some(q) = &again.covered_by
            && !escalated
            && self.unread()
        {
            self.read_at = Some(now);
            self.covered_by = Some(q.clone());
        }
    }

    /// Mark read, keeping the first read time.
    pub fn mark_read(&mut self, now: Timestamp) {
        if self.read_at.is_none() {
            self.read_at = Some(now);
        }
    }

    /// Tombstone: read and hidden from the list.
    pub fn dismiss(&mut self, now: Timestamp) {
        self.mark_read(now);
        if self.dismissed_at.is_none() {
            self.dismissed_at = Some(now);
        }
    }

    /// Pruning order: dismissed go first, then read, then unread.
    fn keep_rank(&self) -> u8 {
        if self.dismissed_at.is_some() {
            0
        } else if self.read_at.is_some() {
            1
        } else {
            2
        }
    }
}

/// A stable, file-name-safe id for a key: a readable slug plus FNV-1a of the
/// whole key. Not `DefaultHasher`, whose output may change between builds and
/// would strand every existing file's dedupe.
pub fn id_of(key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let slug: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    format!("{slug}-{hash:016x}")
}

/// Whether `id` could have come from [`id_of`]. Checked before any path is
/// built from a request.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// How long a caller waits for a notice's lock before proceeding without it.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Age after which a lock file is taken to belong to a dead process.
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(10);

/// Distinguishes temp files written by threads of one process.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A notice store on disk.
#[derive(Debug, Clone)]
pub struct Notices {
    root: PathBuf,
}

impl Notices {
    /// The operator's notices, `<home>/notifications`.
    pub fn open() -> Self {
        Self::at(crate::run::home().join("notifications"))
    }

    /// A store at an explicit root; tests use this.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Directory holding the notice files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn put(&self, n: &Notice) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let body = serde_json::to_string_pretty(n).context("serialize notice")?;
        let path = self.path_of(&n.id);
        // Unique per write: two processes raising or marking the same notice
        // must not share a temp file, or the second rename finds it gone.
        let tmp = path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("replace {}", path.display()));
        }
        Ok(())
    }

    /// Load one notice by id.
    pub fn get(&self, id: &str) -> Result<Notice> {
        if !valid_id(id) {
            bail!("`{id}` is not a notification id");
        }
        read_path(&self.path_of(id))
    }

    /// Run a read-modify-write of one notice under its lock file.
    ///
    /// `magi serve`, `magi web` and the CLI all update the same files, and a
    /// unique temp file only makes each write atomic - it does not stop one
    /// process saving a stale read over another's newer raise. The lock is an
    /// exclusive-create file beside the notice; a holder that died is broken
    /// after [`LOCK_STALE`], and a caller that cannot get it in
    /// [`LOCK_WAIT`] proceeds anyway rather than lose the notice.
    fn locked<T>(&self, id: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let lock = self.root.join(format!("{id}.lock"));
        let start = std::time::Instant::now();
        let mut held = false;
        while start.elapsed() < LOCK_WAIT {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => {
                    held = true;
                    break;
                }
                Err(_) => {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        }
        let out = f();
        if held {
            let _ = std::fs::remove_file(&lock);
        }
        out
    }

    /// File a notice, folding it into an existing one with the same key.
    pub fn raise(&self, incoming: Notice) -> Result<Notice> {
        let stored = self.locked(&incoming.id.clone(), || {
            let now = Timestamp::now();
            let stored = match read_path(&self.path_of(&incoming.id)) {
                Ok(mut existing) => {
                    existing.raise_again(&incoming, now);
                    existing
                }
                Err(_) => {
                    let mut fresh = incoming;
                    if fresh.covered_by.is_some() {
                        fresh.read_at = Some(now);
                    }
                    fresh
                }
            };
            self.put(&stored)?;
            Ok(stored)
        })?;
        self.prune();
        Ok(stored)
    }

    /// Mark every unread notice about `subject` read, recording `question` as
    /// the card that carries it. Best-effort per notice.
    pub fn cover(&self, question: &crate::ask::Question) {
        for n in self.all() {
            if n.unread() && covers(question, &n) {
                let _ = self.update(&n.id, |n| {
                    // Re-checked under the lock: a newer cause may have
                    // landed since `all()` read it.
                    if n.unread() && covers(question, n) {
                        n.mark_read(Timestamp::now());
                        n.covered_by = Some(question.id.clone());
                    }
                });
            }
        }
    }

    /// Load, change and save one notice under its lock.
    fn update(&self, id: &str, change: impl FnOnce(&mut Notice)) -> Result<Notice> {
        if !valid_id(id) {
            bail!("`{id}` is not a notification id");
        }
        self.locked(id, || {
            let mut n = read_path(&self.path_of(id))?;
            change(&mut n);
            self.put(&n)?;
            Ok(n)
        })
    }

    /// Every notice on disk, unreadable files skipped, newest first.
    fn all(&self) -> Vec<Notice> {
        let mut all: Vec<Notice> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| read_path(&p).ok())
            .collect();
        all.sort_by(|a, b| b.last_at.cmp(&a.last_at).then_with(|| a.id.cmp(&b.id)));
        all
    }

    /// What the bell lists: everything not dismissed, newest first.
    pub fn list(&self) -> Vec<Notice> {
        self.all()
            .into_iter()
            .filter(|n| n.dismissed_at.is_none())
            .collect()
    }

    /// How many notices are unread.
    pub fn count_unread(&self) -> usize {
        self.all().iter().filter(|n| n.unread()).count()
    }

    /// Mark read, keeping the first read time.
    pub fn mark_read(&self, id: &str) -> Result<Notice> {
        let now = Timestamp::now();
        self.update(id, |n| n.mark_read(now))
    }

    /// Mark every unread notice read; returns how many changed.
    pub fn mark_all_read(&self) -> Result<usize> {
        let now = Timestamp::now();
        let mut changed = 0;
        for n in self.all().into_iter().filter(Notice::unread) {
            // Re-read under the lock: a raise since the scan may have made it
            // a different, still-unread notice, which stays as it is.
            let done = self.update(&n.id, |n| n.mark_read(now));
            if done.is_ok() {
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Tombstone: read and hidden from the list.
    pub fn dismiss(&self, id: &str) -> Result<Notice> {
        let now = Timestamp::now();
        self.update(id, |n| n.dismiss(now))
    }

    /// Keep at most [`CAP`] files. Best-effort.
    fn prune(&self) {
        let mut all = self.all();
        if all.len() <= CAP {
            return;
        }
        // Most worth keeping first; drop the tail.
        all.sort_by(|a, b| {
            b.keep_rank()
                .cmp(&a.keep_rank())
                .then_with(|| b.last_at.cmp(&a.last_at))
        });
        for n in all.split_off(CAP) {
            let _ = std::fs::remove_file(self.path_of(&n.id));
        }
    }

    /// Change token for the stream. Hashes every file's name and mtime, so a
    /// prune or delete moves it as surely as a write does (a max-mtime would
    /// not move when the newest file is not the one removed).
    pub fn revision(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut entries: Vec<(std::ffi::OsString, u128)> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| {
                let at = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                (e.file_name(), at)
            })
            .collect();
        if entries.is_empty() {
            return 0;
        }
        entries.sort();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        entries.hash(&mut h);
        h.finish()
    }
}

fn read_path(path: &Path) -> Result<Notice> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let n: Notice =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    if n.schema > SCHEMA {
        bail!(
            "notice {} was written by a newer magi (schema {}, this build speaks up to {SCHEMA})",
            n.id,
            n.schema
        );
    }
    Ok(n)
}

/// The notice for a run that ended Blocked, Stalled or Failed, if it did.
///
/// A parked run was only asked to stop, so it is not news. A run that left a
/// pull request behind still is: the PR is waiting on someone. Keyed on the
/// run with a message free of anything that varies between attempts.
pub fn run_ended(state: &crate::run::RunState) -> Option<Notice> {
    use crate::run::RunStatus;
    let failed = matches!(
        state.status,
        RunStatus::Blocked | RunStatus::Stalled | RunStatus::Failed
    );
    (failed && !state.parked).then(|| {
        Notice::error(
            &format!("run:{}", state.id),
            format!("Run {} ended {}.", state.short(), state.status.as_str()),
        )
        .link(Link::Run {
            id: state.id.clone(),
        })
        .about([state.id.clone()])
    })
}

/// The notice for a pull request that merged while checks were red.
///
/// Keyed on the run. `summary` names the repository, pull request and failing
/// checks: with no notify command configured this is the only place the
/// operator learns them. A run merges once, so the wording cannot churn.
pub fn merged_red(run_id: &str, summary: &str) -> Notice {
    Notice::warn(&format!("merged-red:{run_id}"), summary).link(Link::Run {
        id: run_id.to_owned(),
    })
}

/// The notice for a task the machine or its attempt budget has held, if it is.
///
/// Called from [`crate::queue::Queue::put`], which every task transition goes
/// through, so a hold made anywhere - the loop, the conductor, triage, a
/// dependency removed from under a blocked task - is announced. A hold the
/// operator placed by hand is their own action and is not news. Keyed on the
/// task, with wording free of anything that varies between retries.
///
/// Falls back to [`crate::queue::Task::last_error`] when `hold_reason` is
/// empty, the same fallback `crate::triage`'s question detail uses: a record
/// written before `Task::fail`/`Task::handed_off` started copying `why` into
/// `hold_reason` too would otherwise still read "no reason recorded" even
/// though the run said exactly why it stopped.
pub fn task_held(task: &crate::queue::Task) -> Option<Notice> {
    use crate::queue::{HoldSource, TaskStatus};
    if task.status != TaskStatus::Held || task.hold_source != Some(HoldSource::Machine) {
        return None;
    }
    let why = task
        .hold_reason
        .as_deref()
        .or(task.last_error.as_deref())
        .unwrap_or("no reason recorded");
    Some(
        Notice::warn(
            &format!("task:{}", task.id),
            format!("Task {} is held: {why}.", task.short()),
        )
        .link(Link::Task {
            id: task.id.clone(),
        })
        .about([task.id.clone()])
        .since(task.held_at),
    )
}

/// The notice for a run whose graph returned an error before it settled.
pub fn run_stopped(id: &str, state: &crate::run::RunState) -> Notice {
    Notice::error(
        &format!("run:{id}"),
        format!("Run {} stopped with an error.", state.short()),
    )
    .link(Link::Run { id: id.to_owned() })
    .about([id.to_owned()])
}

/// The one function producers call. Best-effort by construction: a notice that
/// cannot be filed is a `tracing::warn`, never a reason to fail the run, the
/// loop or the request that wanted to mention it.
pub fn raise(notice: Notice) {
    if let Some(home) = crate::run::try_home() {
        raise_in(&home, notice);
    }
}

/// Does open question `q` already page the operator for what `n` reports?
///
/// The cause is decided by what each side is about, not by when it happened:
/// a conductor / triage question exists *because* its task is held, so it
/// covers that task's hold and handover notices whenever it was filed; a
/// question raised from inside a run (any other seat node) covers that run's
/// ended / stopped notices and never a task hold. Exact match on
/// `Question::run` (a task id for the former, a run id for the latter). The
/// land approval and release questions are to-dos, not duplicates, and cover
/// nothing.
///
/// A notice with a `since` (a task hold's start) is covered only by a question
/// filed at or after it: one filed earlier is about an older cause, and
/// letting it silence a new hold would hide a fresh failure behind a stale
/// card. Missing a duplicate costs one extra page; hiding a failure costs the
/// failure. A notice without `since` keeps the broad rule.
pub fn covers(q: &crate::ask::Question, n: &Notice) -> bool {
    if !q.status.open() || !n.subjects.contains(&q.run) {
        return false;
    }
    if n.since.is_some_and(|since| q.asked_at < since) {
        return false;
    }
    let about_task = n.key.starts_with("task:") || n.key.starts_with("handover:");
    let about_run = n.key.starts_with("run:");
    match q.node.as_str() {
        crate::bump::NOTICE_NODE | crate::land::APPROVAL_NODE => false,
        crate::conduct::NODE | crate::triage::NODE | crate::triage::DEPS_NODE => about_task,
        _ => about_run,
    }
}

fn covering_question(home: &Path, n: &Notice) -> Option<String> {
    if n.subjects.is_empty() {
        return None;
    }
    crate::ask::Questions::at(home.join("questions"))
        .list()
        .into_iter()
        .find(|q| covers(q, n))
        .map(|q| q.id)
}

/// A question was just filed for `run`: quiet the unread notices about it.
pub fn quiet_for(home: &Path, question: &crate::ask::Question) {
    Notices::at(home.join("notifications")).cover(question);
}

/// [`raise`] into an explicit magi home, for callers that already carry one
/// (the janitor) and so must not reach for the process-global.
pub fn raise_in(home: &Path, mut notice: Notice) {
    if let Some(q) = covering_question(home, &notice) {
        notice.covered_by = Some(q);
    }
    if let Err(e) = Notices::at(home.join("notifications")).raise(notice) {
        tracing::warn!("could not file a notification: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Notices) {
        let dir = tempfile::tempdir().unwrap();
        let s = Notices::at(dir.path().join("notifications"));
        (dir, s)
    }

    #[test]
    fn a_red_merge_notice_is_keyed_on_the_run_and_raised_once() {
        let (_d, s) = store();
        let n = merged_red("run-1", "Merged a/b PR #1 with red checks: x (u)");
        assert_eq!(n.key, "merged-red:run-1");
        assert_eq!(
            n.link,
            Some(Link::Run {
                id: "run-1".to_owned()
            })
        );
        s.raise(merged_red(
            "run-1",
            "Merged a/b PR #1 with red checks: x (u)",
        ))
        .unwrap();
        s.raise(merged_red(
            "run-1",
            "Merged a/b PR #1 with red checks: x (u)",
        ))
        .unwrap();
        assert_eq!(s.list().len(), 1);
    }

    #[test]
    fn a_repeat_counts_but_stays_read_and_a_change_relights_it() {
        let now = Timestamp::now();
        let mut n = Notice::warn("task:1", "held");
        n.mark_read(now);
        n.raise_again(&Notice::warn("task:1", "held"), now);
        assert_eq!(n.count, 2);
        assert!(n.read_at.is_some(), "identical repeat stays read");
        n.raise_again(&Notice::warn("task:1", "held differently"), now);
        assert!(n.unread());
        assert_eq!(n.message, "held differently");
    }

    #[test]
    fn a_rise_in_severity_resurrects_a_dismissed_notice_but_a_repeat_does_not() {
        let now = Timestamp::now();
        let mut n = Notice::warn("task:x", "m");
        n.dismiss(now);
        n.raise_again(&Notice::warn("task:x", "m"), now);
        assert!(n.dismissed_at.is_some(), "tombstone holds");
        n.raise_again(&Notice::error("k", "m"), now);
        assert!(n.unread());
        assert_eq!(n.severity, Severity::Error);
        // Severity never falls on a repeat.
        n.raise_again(&Notice::info("k", "other"), now);
        assert_eq!(n.severity, Severity::Error);
    }

    #[test]
    fn identical_raises_share_one_file() {
        let (_d, s) = store();
        for _ in 0..5 {
            s.raise(Notice::error("run:abc", "blocked")).unwrap();
        }
        let all = s.list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].count, 5);
        assert_eq!(s.count_unread(), 1);
    }

    #[test]
    fn transitions_persist_and_dismissed_leave_the_list() {
        let (_d, s) = store();
        let a = s.raise(Notice::info("a", "one")).unwrap();
        let b = s.raise(Notice::warn("b", "two")).unwrap();
        assert_eq!(s.count_unread(), 2);
        s.mark_read(&a.id).unwrap();
        assert_eq!(s.count_unread(), 1);
        s.dismiss(&b.id).unwrap();
        assert_eq!(s.count_unread(), 0);
        assert_eq!(s.list().len(), 1);
        s.raise(Notice::warn("b", "two")).unwrap();
        assert_eq!(
            s.list().len(),
            1,
            "a tombstone survives a same-message raise"
        );
        s.raise(Notice::info("c", "three")).unwrap();
        assert_eq!(s.mark_all_read().unwrap(), 1);
        assert_eq!(s.count_unread(), 0);
    }

    #[test]
    fn the_list_is_newest_first() {
        let (_d, s) = store();
        let mut old = Notice::info("old", "old");
        old.last_at = "2020-01-01T00:00:00Z".parse().unwrap();
        s.put(&old).unwrap();
        s.raise(Notice::info("new", "new")).unwrap();
        let keys: Vec<_> = s.list().into_iter().map(|n| n.key).collect();
        assert_eq!(keys, ["new", "old"]);
    }

    #[test]
    fn the_cap_prunes_dismissed_then_read_then_oldest() {
        let (_d, s) = store();
        let keep = s.raise(Notice::error("keep", "unread")).unwrap();
        let gone = s.raise(Notice::info("gone", "dismissed")).unwrap();
        s.dismiss(&gone.id).unwrap();
        for i in 0..CAP - 1 {
            s.raise(Notice::info(&format!("k{i}"), "x")).unwrap();
        }
        let files = std::fs::read_dir(s.root()).unwrap().count();
        assert_eq!(files, CAP);
        assert!(
            s.get(&keep.id).is_ok(),
            "an unread notice outlives a dismissed one"
        );
        assert!(s.get(&gone.id).is_err(), "the dismissed one went first");
    }

    #[test]
    fn revision_moves_on_write_and_on_removal() {
        let (_d, s) = store();
        assert_eq!(s.revision(), 0);
        let a = s.raise(Notice::info("a", "m")).unwrap();
        let r1 = s.revision();
        assert_ne!(r1, 0);
        s.raise(Notice::info("b", "m")).unwrap();
        let r2 = s.revision();
        assert_ne!(r1, r2);
        std::fs::remove_file(s.path_of(&a.id)).unwrap();
        assert_ne!(s.revision(), r2);
    }

    #[test]
    fn ids_are_stable_and_untrusted_ids_are_refused() {
        assert_eq!(id_of("run:1"), id_of("run:1"));
        assert_ne!(id_of("run:1"), id_of("run-1"));
        assert!(valid_id(&id_of("release-bump:20260101-abc")));
        let (_d, s) = store();
        for bad in ["", "../x", "a/b", "A", "x.json"] {
            assert!(s.get(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_newer_schema_is_refused_and_an_older_reads() {
        let (_d, s) = store();
        let mut n = Notice::info("k", "m");
        n.schema = SCHEMA + 1;
        s.put(&n).unwrap();
        assert!(s.get(&n.id).is_err());
        let old = r#"{"id":"x-1","key":"x","severity":"warn","message":"m",
            "first_at":"2020-01-01T00:00:00Z","last_at":"2020-01-01T00:00:00Z"}"#;
        std::fs::write(s.path_of("x-1"), old).unwrap();
        assert_eq!(s.get("x-1").unwrap().count, 1);
    }

    fn state(status: crate::run::RunStatus) -> crate::run::RunState {
        let mut st = crate::run::RunState::new(
            std::path::PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "task".to_owned(),
            crate::config::Config::default(),
        );
        st.status = status;
        st
    }

    #[test]
    fn a_run_that_ended_badly_is_news_unless_it_only_parked() {
        use crate::run::RunStatus;
        for bad in [RunStatus::Blocked, RunStatus::Stalled, RunStatus::Failed] {
            let st = state(bad);
            let n = run_ended(&st).expect("news");
            assert_eq!(n.key, format!("run:{}", st.id));
            assert_eq!(n.severity, Severity::Error);
        }
        assert!(run_ended(&state(RunStatus::Merged)).is_none());
        let mut parked = state(RunStatus::Stalled);
        parked.parked = true;
        assert!(run_ended(&parked).is_none());
    }

    #[test]
    fn concurrent_writers_of_one_key_all_succeed() {
        let (_d, s) = store();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        s.raise(Notice::warn("same", "m")).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.list().len(), 1);
        let stray = std::fs::read_dir(s.root())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(stray, 0);
    }

    #[test]
    fn a_dead_holders_lock_is_broken_and_no_lock_is_left_behind() {
        let (_d, s) = store();
        let n = s.raise(Notice::info("k", "m")).unwrap();
        let lock = s.root().join(format!("{}.lock", n.id));
        std::fs::write(&lock, "").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        s.mark_read(&n.id).unwrap();
        assert!(!lock.exists());
        assert!(s.get(&n.id).unwrap().read_at.is_some());
    }

    #[test]
    fn only_a_machine_hold_is_a_task_notice() {
        let mut t = crate::queue::Task::new(
            "t".to_owned(),
            "do it".to_owned(),
            std::path::PathBuf::from("/repo"),
            crate::queue::Source::Human,
        );
        assert!(task_held(&t).is_none());
        t.hold_manual(None);
        assert!(task_held(&t).is_none(), "the operator's own hold");
        t.hold_machine(Some("missing blocker".to_owned()));
        let n = task_held(&t).expect("machine hold");
        assert_eq!(n.key, format!("task:{}", t.id));
        assert!(n.message.contains("missing blocker"));
    }

    fn held_task() -> crate::queue::Task {
        let mut t = crate::queue::Task::new(
            "t".to_owned(),
            "do it".to_owned(),
            std::path::PathBuf::from("/repo"),
            crate::queue::Source::Human,
        );
        t.hold_machine(Some("branch b is checked out".to_owned()));
        t
    }

    fn ask(home: &Path, run: &str, node: &str) -> crate::ask::Question {
        let mut q = crate::ask::Question::new(
            run.to_owned(),
            node.to_owned(),
            "conductor".to_owned(),
            "cannot resume".to_owned(),
            String::new(),
            Vec::new(),
        );
        crate::ask::Questions::at(home.join("questions"))
            .put(&mut q)
            .unwrap();
        q
    }

    fn unread(home: &Path) -> usize {
        Notices::at(home.join("notifications")).count_unread()
    }

    #[test]
    fn a_hold_with_an_open_question_for_the_task_pages_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = held_task();
        let q = ask(dir.path(), &t.id, "conduct");
        crate::queue::Queue::at(dir.path().join("queue"))
            .put(&mut t)
            .unwrap();
        let s = Notices::at(dir.path().join("notifications"));
        assert_eq!(s.count_unread(), 0);
        let n = s.list().pop().expect("the record is kept");
        assert_eq!(n.covered_by.as_deref(), Some(q.id.as_str()));
        assert!(n.message.contains("checked out"));
    }

    #[test]
    fn a_refused_handover_without_a_question_pages_once() {
        let dir = tempfile::tempdir().unwrap();
        let q = crate::queue::Queue::at(dir.path().join("queue"));
        let mut t = held_task();
        t.hold_for_handover(Some("magi/x/A".to_owned()), "checked out".to_owned());
        q.put(&mut t).unwrap();
        q.put(&mut t).unwrap();
        let s = Notices::at(dir.path().join("notifications"));
        assert_eq!(s.list().len(), 1);
        assert_eq!(s.list()[0].key, format!("task:{}", t.id));
        assert_eq!(s.count_unread(), 1);
    }

    #[test]
    fn an_older_question_does_not_silence_a_new_hold() {
        for node in ["deps", "conduct", "triage"] {
            let dir = tempfile::tempdir().unwrap();
            let mut t = crate::queue::Task::new(
                "t".to_owned(),
                "do it".to_owned(),
                std::path::PathBuf::from("/repo"),
                crate::queue::Source::Human,
            );
            let old = ask(dir.path(), &t.id, node);
            std::thread::sleep(std::time::Duration::from_millis(5));
            t.hold_machine(Some("new failure".to_owned()));
            crate::queue::Queue::at(dir.path().join("queue"))
                .put(&mut t)
                .unwrap();
            let s = Notices::at(dir.path().join("notifications"));
            let n = s.list().pop().unwrap();
            assert_eq!(n.covered_by, None, "{node} question {}", old.id);
            assert_eq!(s.count_unread(), 1);
        }
    }

    #[test]
    fn a_newer_cause_relights_a_dismissed_notice() {
        let (_d, s) = store();
        let t0 = Timestamp::now();
        let first = Notice::warn("task:x", "held").since(Some(t0));
        let id = first.id.clone();
        s.raise(first).unwrap();
        s.dismiss(&id).unwrap();
        s.raise(Notice::warn("task:x", "held").since(Some(t0)))
            .unwrap();
        assert!(!s.get(&id).unwrap().unread(), "same cause stays dismissed");
        let later = t0 + std::time::Duration::from_secs(1);
        s.raise(Notice::warn("task:x", "held").since(Some(later)))
            .unwrap();
        assert!(s.get(&id).unwrap().unread());
    }

    #[test]
    fn a_hold_with_no_question_still_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = held_task();
        crate::queue::Queue::at(dir.path().join("queue"))
            .put(&mut t)
            .unwrap();
        assert_eq!(unread(dir.path()), 1);
    }

    #[test]
    fn a_question_for_another_task_does_not_suppress() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = held_task();
        ask(dir.path(), "some-other-task", "conduct");
        crate::queue::Queue::at(dir.path().join("queue"))
            .put(&mut t)
            .unwrap();
        assert_eq!(unread(dir.path()), 1);
    }

    #[test]
    fn a_question_filed_after_the_hold_quiets_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = held_task();
        crate::queue::Queue::at(dir.path().join("queue"))
            .put(&mut t)
            .unwrap();
        assert_eq!(unread(dir.path()), 1);
        let q = ask(dir.path(), &t.id, "conduct");
        assert_eq!(unread(dir.path()), 0);
        // A later update of the same question does not run the hook again.
        let s = Notices::at(dir.path().join("notifications"));
        let id = s.list()[0].id.clone();
        s.update(&id, |n| n.read_at = None).unwrap();
        let mut again = crate::ask::Questions::at(dir.path().join("questions"))
            .get(&q.id)
            .unwrap();
        crate::ask::Questions::at(dir.path().join("questions"))
            .put(&mut again)
            .unwrap();
        assert_eq!(unread(dir.path()), 1);
    }

    #[test]
    fn a_blocked_run_with_an_open_question_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        ask(dir.path(), "run-1", "implement");
        raise_in(
            dir.path(),
            Notice::error("run:run-1", "Run r ended blocked.").about(["run-1"]),
        );
        assert_eq!(unread(dir.path()), 0);
        raise_in(
            dir.path(),
            Notice::error("run:run-2", "Run r ended blocked.").about(["run-2"]),
        );
        assert_eq!(unread(dir.path()), 1);
    }

    #[test]
    fn escalation_makes_a_covered_notice_unread_again() {
        let dir = tempfile::tempdir().unwrap();
        ask(dir.path(), "t1", "conduct");
        raise_in(dir.path(), Notice::warn("task:t1", "held").about(["t1"]));
        assert_eq!(unread(dir.path()), 0);
        raise_in(dir.path(), Notice::error("task:t1", "held").about(["t1"]));
        assert_eq!(unread(dir.path()), 1);
    }

    #[test]
    fn covers_matches_run_exactly_and_ignores_release_questions() {
        let dir = tempfile::tempdir().unwrap();
        let n = Notice::warn("task:x", "m").about(["t1"]);
        let q = ask(dir.path(), "t1", "conduct");
        assert!(covers(&q, &n));
        assert!(!covers(&q, &Notice::warn("task:x", "m")));
        assert!(!covers(&q, &Notice::warn("task:x", "m").about(["t"])));
        let release = ask(dir.path(), "t1", crate::bump::NOTICE_NODE);
        assert!(!covers(&release, &n));
    }

    #[test]
    fn a_run_question_does_not_swallow_a_task_hold_and_vice_versa() {
        let dir = tempfile::tempdir().unwrap();
        ask(dir.path(), "t1", "implement");
        raise_in(dir.path(), Notice::warn("task:t1", "held").about(["t1"]));
        assert_eq!(unread(dir.path()), 1);
        ask(dir.path(), "r1", "conduct");
        raise_in(dir.path(), Notice::error("run:r1", "ended").about(["r1"]));
        assert_eq!(unread(dir.path()), 2);
    }

    #[test]
    fn a_conduct_question_covers_the_hold_however_late_it_was_filed() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = crate::ask::Question::new(
            "t1".to_owned(),
            "conduct".to_owned(),
            "conductor".to_owned(),
            "s".to_owned(),
            String::new(),
            Vec::new(),
        );
        q.asked_at = Timestamp::from_second(Timestamp::now().as_second() - 3600).unwrap();
        crate::ask::Questions::at(dir.path().join("questions"))
            .put(&mut q)
            .unwrap();
        raise_in(dir.path(), Notice::warn("task:t1", "held").about(["t1"]));
        assert_eq!(unread(dir.path()), 0);
    }
}
