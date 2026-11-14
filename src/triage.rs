//! Held-task triage: walking `held` tasks so a hold left by an accident does
//! not sit unread forever next to one a human placed on purpose.
//!
//! [`crate::queue::HoldSource`] already distinguishes "the daemon or
//! conductor held this during its own recovery" ([`HoldSource::Machine`],
//! documented as recoverable) from "an operator held this on purpose"
//! ([`HoldSource::Manual`]). What was missing was anything that actually acts
//! on that distinction: nothing walked the held list and asked whether a
//! machine hold's cause was still true, and a record written before
//! `hold_source` existed (schema < 3, `None`) was silently protected forever
//! by [`Task::operator_held`]'s conservative default - never wrong, but also
//! never looked at again by anything.
//!
//! [`run_once`] is that walk. For every `held` task it finds:
//!
//! - [`HoldSource::Machine`]: if [`machine_cause_resolved`] can tell the
//!   disk cause is gone, the same task returns to `queued`, preserving attempts
//!   and retained execution evidence. Known disk waits remain monitored without
//!   questions while pressure or measurement failure persists. Unknown causes
//!   get one [`Question`] and remain held until answered.
//! - `None` (a legacy record, or a hold nobody explained): always a question,
//!   exactly once - the whole point being that "protected forever" must not
//!   mean "never shown to anyone" either.
//! - [`HoldSource::Manual`]: never touched automatically. Only once the hold
//!   has sat untouched past [`MANUAL_STALE_AFTER`] does it earn a question of
//!   its own, asking whether it is still wanted.
//!
//! Every question this module files carries [`NODE`] and uses the task id as
//! [`Question::run`] - the same convention `crate::conduct`'s own questions
//! use for a task rather than a run (see `conduct::apply_one`'s own comment
//! on why the dedupe check there also filters on `node`, not `run` alone: an
//! ordinary graph question's `run` is a real run id, and a coincidental
//! equality with some task's id must not be read as "about this task").
//! [`latest_triage_question`] follows the identical rule.
//!
//! # Why answers apply here rather than through `crate::queue::Task::block`
//!
//! `crate::conduct` blocks a task on its own question
//! (`Task::block(vec![question_id], …)`), and `crate::daemon::resolve_blockers`
//! unblocks it - unconditionally, back to `queued` - the moment that question
//! is answered, whatever the answer actually said. That is correct for
//! `conduct`: the *content* of the answer is meant for whoever reads
//! `Task::answers` next, not for the resolver.
//!
//! A triage question's answer is different: "not yet" and "discard it" do two
//! entirely different, non-resuming things, and only "resume it" may put the
//! task back in line. Reusing the generic blocked/unblock path would resume
//! every answer alike, so this module never calls [`Task::block`] and never
//! leaves a triaged task anything but `held` while its question is open.
//! [`interpret_answer`] reads [`Question::resolution`] itself and
//! [`run_once`] acts on it directly: [`Task::release`] for an actual "resume
//! it" choice, [`Queue::remove`] for "discard it" (捨ててよい really means
//! "you may throw this away", not "leave it sitting held" - the English
//! wording must say the same thing, not "leave it held"), and
//! [`Task::hold_manual`] for anything else - which both keeps the task held
//! and reclassifies it as a hold an operator has now actually seen, one
//! `crate::conduct` and a later triage pass leave alone.
//!
//! The choice is read by its **position** in [`Question::choices`]
//! ([`Wording::choices3`]/[`Wording::choices2`] always put "resume" first and
//! "discard" third), never by comparing the answer text against [`Wording`]'s
//! own strings picked from whatever config is in force *now* - the language a
//! question was filed in and the language a later `run_once` call happens to
//! read back are not guaranteed to be the same call's [`Config`], and a text
//! comparison would silently misread a real "resume" answer as "keep held"
//! the moment they disagree.
//!
//! # Idempotency
//!
//! [`run_once`] runs on every daemon idle tick (see `crate::daemon::poll`)
//! and on every `magi task triage`; the existing maintenance service also checks
//! disk waits while other tasks run, at most once per minute per task. Thus an answered question must be applied to
//! a task **at most once**, and a *fresh* question for the same task (once it
//! is held again, or goes stale) must still be possible. The record is
//! [`Task::triage_applied`], the question ids already applied. It cannot live
//! in [`Task::hold_reason`]: [`Task::release`] clears that, so a "resume"
//! answer left no trace, and a released task that failed back to `held` was
//! released again by the same old answer with its attempts reset - forever.
//! A task that comes back to `held` after an applied answer is therefore a
//! new hold, handled per [`HoldSource`] (a fresh question for a machine hold).
//! [`already_applied`] also still reads the `[triage:<short>]` marker
//! [`keep_held_note`] appends to the hold reason, for records that pre-date
//! the field.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;

use crate::ask::{Question, QuestionStatus, Questions};
use crate::config::Config;
use crate::queue::{HoldSource, OperatorResume, Queue, Task, TaskStatus};

/// Node recorded on every question this module files - `crate::conduct::NODE`
/// for the same idea applied to a `crate::conduct` decision instead.
pub const NODE: &str = "triage";

/// Node on the question filed about a stuck dependency root - see
/// [`ask_about_stuck_roots`]. Separate from [`NODE`] because the choices mean
/// something else (position 2 is "detach the dependants", not "discard").
pub const DEPS_NODE: &str = "triage-deps";

/// Seat name on a filed question. Not a real agent seat - there is no model
/// call anywhere in this module - but every [`Question`] needs one, and every
/// other deterministic filer (`crate::land`'s merge approval) names itself
/// the same way.
const SEAT: &str = "triage";

/// How long a [`HoldSource::Manual`] hold sits untouched before triage asks
/// whether it is still wanted.
///
/// A judgement call, not a `magi.toml` setting - the same reasoning
/// `ask::REPLY_QUIET_WINDOW` documents for itself: there is no operator
/// preference for "how long is too long to ignore my own hold" that a
/// per-repository config could be *right* about. Seven days is long enough
/// that an ordinary multi-day hold (waiting on a dependency, waiting on the
/// operator's own schedule) never gets nagged, and short enough that a hold
/// nobody has looked at in a week surfaces again rather than aging into the
/// kind of silent backlog this feature exists to prevent.
const MANUAL_STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Localised strings for a filed question, the same idea as `crate::land`'s
/// own `Words`/`words()` - only the languages magi can actually check are
/// translated, and anything else falls back to English.
struct Wording {
    lang: &'static str,
    resume: &'static str,
    wait: &'static str,
    discard: &'static str,
    resume_now: &'static str,
    keep_held: &'static str,
}

const EN: Wording = Wording {
    lang: "en",
    resume: "resume it",
    wait: "not yet",
    discard: "discard it",
    resume_now: "resume it",
    keep_held: "keep it held",
};

const JA: Wording = Wording {
    lang: "ja",
    resume: "再開してよい",
    wait: "まだ待って",
    discard: "捨ててよい",
    resume_now: "再開する",
    keep_held: "まだ止めておく",
};

/// Pick the wording. Codes and names both, the same acceptance
/// `crate::land::words` gives `[graph] language`.
fn wording(language: &str) -> &'static Wording {
    if crate::lang::is_japanese(language) {
        &JA
    } else {
        &EN
    }
}

impl Wording {
    fn choices3(&self) -> Vec<String> {
        vec![
            self.resume.to_owned(),
            self.wait.to_owned(),
            self.discard.to_owned(),
        ]
    }

    fn choices2(&self) -> Vec<String> {
        vec![self.resume_now.to_owned(), self.keep_held.to_owned()]
    }

    fn source_label(&self, source: Option<HoldSource>) -> &'static str {
        match (self.lang, source) {
            ("ja", Some(HoldSource::Machine)) => "machine（機械による自動保留）",
            ("ja", Some(HoldSource::Manual)) => "manual（操作者による手動保留）",
            ("ja", None) => "unknown（schema 3 未満の旧レコード、または理由未記録）",
            (_, Some(HoldSource::Machine)) => "machine (automatic recovery hold)",
            (_, Some(HoldSource::Manual)) => "manual (an operator held this)",
            (_, None) => "unknown (pre-schema-3 record, or never recorded)",
        }
    }

    /// The body under the summary: everything an operator needs to judge this
    /// without opening a terminal - id, title, hold reason, hold source.
    ///
    /// Falls back to [`Task::last_error`] when [`Task::hold_reason`] is empty:
    /// a record written before `Task::fail`/`Task::handed_off` started
    /// copying `why` into `hold_reason` too, or one written by a still older
    /// build, would otherwise leave the question blank for exactly the case
    /// requirement 4 exists for.
    fn detail(&self, task: &Task, why: &str) -> String {
        let none = if self.lang == "ja" {
            "（記録なし）"
        } else {
            "(none recorded)"
        };
        let reason = task
            .hold_reason
            .as_deref()
            .or(task.last_error.as_deref())
            .unwrap_or(none);
        format!(
            "task: {} ({})\ntitle: {}\nhold source: {}\nhold reason: {reason}\n\n{why}",
            task.id,
            task.short(),
            task.title,
            self.source_label(task.hold_source),
        )
    }

    fn summary_machine_unknown(&self, task: &Task) -> String {
        if self.lang == "ja" {
            format!("保留タスク {} の再開可否を判断してください", task.short())
        } else {
            format!("decide whether to resume held task {}", task.short())
        }
    }

    fn why_machine(&self) -> &'static str {
        if self.lang == "ja" {
            "機械的な保留(machine hold)ですが、原因がすでに解消しているかを自動では判断できませんでした。"
        } else {
            "This is a machine hold, but whether its cause has resolved could not be \
             checked automatically."
        }
    }

    fn summary_conductor_override(&self, task: &Task) -> String {
        if self.lang == "ja" {
            format!(
                "再開と回答済みのタスク {} を conductor が再び保留しました",
                task.short()
            )
        } else {
            format!(
                "task {} was resumed at your word, but the conductor held it again",
                task.short()
            )
        }
    }

    fn why_conductor_override(&self, o: &OperatorResume) -> String {
        let reason = o.conductor_rehold.as_deref().unwrap_or_default();
        if self.lang == "ja" {
            format!(
                "{} に再開と回答済みですが、conductor が再び hold しました。conductor の理由: \
                 {reason}\n\n強制再キューを選ぶと、以後 conductor はこのタスクを hold できません。",
                o.at
            )
        } else {
            format!(
                "You answered \"resume\" at {}, but the conductor held the task again. \
                 Its reason: {reason}\n\nForcing a requeue stops the conductor from \
                 holding this task again.",
                o.at
            )
        }
    }

    /// Positions match [`AnswerAction`]: 0 resume, 1 keep held, 2 discard.
    fn choices_conductor_override(&self) -> Vec<String> {
        if self.lang == "ja" {
            vec![
                "強制再キュー（conductor は再 hold 不可）".to_owned(),
                "手動 hold のまま".to_owned(),
                "捨ててよい".to_owned(),
            ]
        } else {
            vec![
                "force requeue (conductor must not hold again)".to_owned(),
                "keep held (manual)".to_owned(),
                "discard".to_owned(),
            ]
        }
    }

    fn summary_legacy(&self, task: &Task) -> String {
        if self.lang == "ja" {
            format!(
                "hold_source が不明な保留タスク {} を確認してください",
                task.short()
            )
        } else {
            format!(
                "held task {} has no recorded hold source - please take a look",
                task.short()
            )
        }
    }

    fn why_legacy(&self) -> &'static str {
        if self.lang == "ja" {
            "hold_source が記録されていません。schema 3 より前のレコードか、理由が記録されなかった \
             holdです。人が意図して止めたのか、クラッシュや強制再起動で宙に浮いただけなのか、\
             このデータからは区別できません。"
        } else {
            "No hold_source was recorded - either a pre-schema-3 record, or a hold whose \
             reason was never written down. Whether this was a deliberate hold or the \
             leftover of a crash cannot be told from the data alone."
        }
    }

    fn summary_manual_stale(&self, task: &Task, days: i64) -> String {
        if self.lang == "ja" {
            format!(
                "{days}日間 保留されたままの手動保留タスク {} を確認してください",
                task.short()
            )
        } else {
            format!(
                "held task {} has been on a manual hold for {days} day(s)",
                task.short()
            )
        }
    }

    fn why_manual(&self) -> &'static str {
        if self.lang == "ja" {
            "操作者が明示的に止めた保留ですが、長期間そのままになっています。まだ止めておくか、\
             再開するか教えてください。"
        } else {
            "An operator held this on purpose, but it has sat untouched for a while. Say \
             whether to keep holding it or resume it."
        }
    }
}

/// Which of the three situations this module recognises a held task is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    /// `HoldSource::Machine`, cause not verifiably resolved.
    MachineUnknown,
    /// `HoldSource::Machine`, held by the conductor after the operator
    /// answered "resume" (see [`Task::resume_override`]).
    ConductorOverride,
    /// `hold_source` is `None`.
    Legacy,
    /// `HoldSource::Manual`, held past [`MANUAL_STALE_AFTER`].
    ManualStale,
}

/// What one [`run_once`] pass did, task ids in each list.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// A `HoldSource::Machine` hold whose cause was found resolved, put back
    /// in line automatically.
    pub resumed: Vec<String>,
    /// A fresh question was filed this pass.
    pub asked: Vec<String>,
    /// An operator's answer to an earlier triage question was applied.
    pub answered: Vec<String>,
    /// A `blocked` task whose `blocked_by` named an id that no longer exists
    /// was moved to a machine hold this pass - see
    /// [`quarantine_orphaned_blocked`]. Distinct from `asked`: the question
    /// about it, if any, is filed in the same pass and only counted there.
    pub quarantined: Vec<String>,
    /// A disk-wait task whose untouched, automatically filed triage question
    /// was superseded as part of its automatic resume. No owner answer exists.
    pub superseded: Vec<String>,
    /// A recovered disk-wait task that still needs an explicit owner decision
    /// (an owner-touched or unrelated question/approval); monitoring continues.
    pub needs_owner: Vec<String>,
}

impl Report {
    /// Is there nothing to report? Callers use this to skip logging an empty
    /// pass rather than repeating "triaged 0 held task(s)" on every idle tick.
    pub fn is_empty(&self) -> bool {
        self.resumed.is_empty()
            && self.asked.is_empty()
            && self.answered.is_empty()
            && self.quarantined.is_empty()
            && self.superseded.is_empty()
            && self.needs_owner.is_empty()
    }
}

/// The repository a task's config and disk check should read from. Mirrors
/// `crate::conduct::repo_for`'s own fallback, duplicated rather than shared
/// because that one is private to its module and the two are one `if` each.
fn repo_for(task: &Task) -> PathBuf {
    if task.repo.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        task.repo.clone()
    }
}

/// Only exact legacy admission messages may migrate without structured evidence.
fn is_disk_hold(task: &Task) -> bool {
    task.has_disk_capacity_cause()
        || (task.schema < 22
            && task
                .hold_reason
                .as_deref()
                .is_some_and(crate::disk::is_gate_reason))
}

pub(crate) fn disk_wait_eligible(task: &Task) -> bool {
    disk_wait_known(task, &crate::run::home())
}

/// Run evidence is mandatory for resource-block and emergency-writer holds.
fn disk_wait_known(task: &Task, home: &Path) -> bool {
    if task.status != TaskStatus::Held
        || task.hold_source != Some(HoldSource::Machine)
        || task
            .resume_override
            .as_ref()
            .is_some_and(|o| o.conductor_rehold.is_some())
    {
        return false;
    }
    if task.has_disk_capacity_cause() {
        return true;
    }
    // Legacy prose is not authority to reinterpret an explicit recovery pin.
    if task.resume_override.is_some() {
        return false;
    }
    if is_disk_hold(task) {
        return true;
    }
    if task.schema >= 22 {
        return false;
    }
    if !task.hold_reason.as_deref().is_some_and(|r| {
        r.starts_with(crate::disk::RUNNING_PRESSURE) || r.starts_with(crate::cache::RESOURCE_HOLD)
    }) {
        return false;
    }
    let Some(id) = task.execution_runs().last() else {
        return false;
    };
    let Ok(state) = crate::run::RunState::load_under(id, home) else {
        return false;
    };
    state.id == *id
        && state.resource_block.as_ref().is_some_and(|b| {
            let expected = match b.kind.as_str() {
                "disk pressure" => format!("{}{}", crate::disk::RUNNING_PRESSURE, b.detail),
                "disk admission" => format!(
                    "{}{}; no attempt was spent; resume to retry the affected operation",
                    crate::cache::RESOURCE_HOLD,
                    b.description()
                ),
                _ => return false,
            };
            matches!(b.cause, crate::run::ResourceBlockCause::Other)
                && task.hold_reason.as_deref() == Some(expected.as_str())
        })
}

/// Has a `HoldSource::Machine` hold's cause resolved? `Some(true)` means yes -
/// safe to requeue. `Some(false)` means the same cause was checked and is
/// still in force. `None` means this hold's cause is not one this module
/// knows how to re-check at all, and a human has to look.
fn machine_cause_resolved(task: &Task, cfg: &Config) -> Option<bool> {
    machine_cause_resolved_under(task, cfg, &crate::run::home())
}

fn machine_cause_resolved_under(task: &Task, cfg: &Config, home: &Path) -> Option<bool> {
    machine_cause_resolved_with_admission(task, cfg, home, |paths, floor| {
        crate::retention::OutputLease::admit(home, paths, floor).is_ok()
    })
}

fn machine_cause_resolved_with_admission(
    task: &Task,
    cfg: &Config,
    home: &Path,
    admission_available: impl Fn(&[PathBuf], u64) -> bool,
) -> Option<bool> {
    if disk_wait_known(task, home) {
        if crate::daemon::disk_gate(&repo_for(task), cfg).is_some() {
            return Some(false);
        }
        let mut floor = cfg.disk.min_free_bytes;
        let mut paths = vec![repo_for(task)];
        paths.extend(cfg.cache_dir());
        paths.push(
            cfg.graph
                .worktree_root
                .clone()
                .unwrap_or_else(crate::run::default_worktree_root),
        );
        if let Some(id) = task.execution_runs().last() {
            let Ok(state) = crate::run::RunState::load_under(id, home) else {
                return Some(false);
            };
            if state.id != *id || !state.status.resumable() {
                return Some(false);
            }
            if crate::daemon::disk_gate(&state.repo, &state.config).is_some() {
                return Some(false);
            }
            floor = floor.max(state.config.disk.min_free_bytes);
            paths.extend([state.repo.clone(), state.worktree_root()]);
            paths.extend(state.candidates.iter().map(|c| c.worktree.clone()));
            paths.extend(state.config.cache_dir());
        }
        return Some(admission_available(&paths, floor));
    }
    if task
        .hold_reason
        .as_deref()
        .is_some_and(|r| r.starts_with(crate::cache::RESOURCE_HOLD))
    {
        // Require evidence from the exact latest run before auto-releasing.
        let id = task.execution_runs().last()?;
        let state = crate::run::RunState::load_under(id, home).ok()?;
        let block = state.resource_block.as_ref()?;
        if state.id != *id {
            return None;
        }
        // A free cache lease resolves contention, not malformed manifests,
        // failed writes, cancellation, or unknown infrastructure failures.
        if block.kind != "shared build cache"
            || !matches!(block.cause, crate::run::ResourceBlockCause::Timeout { .. })
        {
            return None;
        }
        return Some(
            cfg.cache_dir()
                .is_none_or(|dir| !crate::cache::in_use(home, &dir)),
        );
    }
    None
}

/// Is a `HoldSource::Manual` hold old enough to earn a "still wanted?"
/// question? Same comparison `crate::clean::due` uses for a run's fold grace,
/// against [`MANUAL_STALE_AFTER`] instead of a configured one.
fn manual_is_stale(task: &Task, now: Timestamp) -> bool {
    now.as_second() - task.updated_at.as_second() > MANUAL_STALE_AFTER.as_secs() as i64
}

/// The marker [`apply_answer`] writes into [`Task::hold_reason`] and
/// [`already_applied`] reads back - see this module's own doc on why.
fn marker_for(q: &Question) -> String {
    format!("[triage:{}]", q.short())
}

/// Has `q`'s answer already been applied to `task`? See this module's doc.
/// Checks [`Task::triage_applied`] first, which survives [`Task::release`].
fn already_applied(task: &Task, q: &Question) -> bool {
    if task.triage_applied(&q.id) {
        return true;
    }
    // Records written before `Task::triage_applied` existed carry only the
    // "keep held" marker in the hold reason.
    let marker = marker_for(q);
    task.hold_reason
        .as_deref()
        .is_some_and(|r| r.contains(marker.as_str()))
}

/// The most recent question this module filed for `task_id`, any status -
/// open (still waiting), answered (may need applying), or abandoned (settled
/// with nothing decided). Filters on both `node` and `run`, never `run`
/// alone - see this module's doc on why a bare `run` match is not safe.
fn latest_triage_question(questions: &Questions, task_id: &str) -> Option<Question> {
    latest_question(questions, NODE, task_id)
}

/// Latest recovery questions from one effect-view scan. Task filename ids
/// participate in prefix resolution even when their records are unreadable.
pub(crate) struct RecoveryIndex {
    latest: std::collections::HashMap<(String, String), Question>,
    open: std::collections::BTreeSet<String>,
}

impl RecoveryIndex {
    pub(crate) fn load(questions: &Questions) -> Self {
        let queue = Queue::at(
            questions
                .root()
                .parent()
                .unwrap_or(Path::new(""))
                .join("queue"),
        );
        let tasks = queue.list();
        let ownership = crate::task_chat::InquiryOwnership::from_tasks(&tasks);
        Self::from_scan(&queue, &tasks, questions.list_for_effects_in(&ownership))
    }

    fn from_scan(queue: &Queue, tasks: &[Task], all: Vec<Question>) -> Self {
        let ids = crate::queue::record_ids(queue.root(), "json").unwrap_or_default();
        let by_id: std::collections::HashMap<_, _> =
            tasks.iter().map(|task| (task.id.as_str(), task)).collect();
        let exact: std::collections::HashSet<_> = ids.iter().map(String::as_str).collect();
        let mut aliases: std::collections::HashMap<&str, Option<&str>> = Default::default();
        for id in &ids {
            for boundary in id
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(id.len()))
            {
                for alias in [&id[..boundary], &id[boundary..]] {
                    let entry = aliases.entry(alias).or_insert(Some(id));
                    if *entry != Some(id.as_str()) {
                        *entry = None;
                    }
                }
            }
        }
        let mut index = Self {
            latest: Default::default(),
            open: Default::default(),
        };
        for q in all {
            if q.node != NODE && q.node != DEPS_NODE {
                continue;
            }
            let resolved = if exact.contains(q.run.as_str()) {
                Some(q.run.as_str())
            } else {
                aliases.get(q.run.as_str()).copied().flatten()
            };
            if let Some(id) = resolved
                && by_id.get(id).is_some_and(|task| {
                    crate::repair_budget::reconcile::superseded_question(task, &q)
                })
            {
                continue;
            }
            if q.status.open() {
                index.open.insert(q.run.clone());
            }
            let key = (q.node.clone(), q.run.clone());
            let entry = index.latest.entry(key).or_insert_with(|| q.clone());
            if (q.asked_at, &q.id) > (entry.asked_at, &entry.id) {
                *entry = q;
            }
        }
        index
    }

    fn latest(&self, node: &str, task_id: &str) -> Option<Question> {
        self.latest
            .get(&(node.to_owned(), task_id.to_owned()))
            .cloned()
    }

    pub(crate) fn pending(&self, task: &Task) -> Vec<Question> {
        [NODE, DEPS_NODE]
            .into_iter()
            .filter_map(|node| {
                self.latest(node, &task.id).filter(|q| {
                    q.status.open()
                        || awaiting_task_chat(q, task)
                        || (q.status == QuestionStatus::Answered
                            && if node == NODE {
                                !already_applied(task, q)
                            } else {
                                !task.triage_applied(&q.id)
                            })
                })
            })
            .collect()
    }
}

/// Single-task compatibility entry point; batch callers retain the index.
fn latest_question(questions: &Questions, node: &str, task_id: &str) -> Option<Question> {
    RecoveryIndex::load(questions).latest(node, task_id)
}

/// What an answered triage question's choice means, independent of which
/// language it was filed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnswerAction {
    /// The first choice, always "resume it" / "再開してよい" / "再開する" -
    /// see [`Wording::choices3`] and [`Wording::choices2`], whose first entry
    /// is always the resume choice.
    Resume,
    /// The third choice, present only in [`Wording::choices3`]: "discard it"
    /// / "捨ててよい".
    Discard,
    /// Anything else: the second choice ("not yet" / "keep it held"), or an
    /// answer that does not match any offered choice at all (should not
    /// happen for a multiple-choice question, but the safe default is to
    /// keep holding rather than guess at "resume").
    KeepHeld,
}

/// Read `q`'s answer as an [`AnswerAction`].
///
/// Matched by **position in `q.choices`**, never by comparing the answer text
/// against [`Wording`]'s own strings: [`Wording`] is picked from the task's
/// *current* repository config, which can differ from whatever language was
/// in force when the question was filed (a `--config` override on one `magi
/// task triage` call and not the next, or the repository's config edited in
/// between). Comparing text would then silently misread an actual "resume"
/// answer as "keep held" - the choices themselves are fixed at filing time,
/// in [`file_question`], and never change afterwards, so their position is
/// the one thing that stays true regardless of which language reads them
/// back.
fn interpret_answer(q: &Question) -> AnswerAction {
    let resolution = q.resolution().unwrap_or_default();
    match q.choices.iter().position(|c| *c == resolution) {
        Some(0) => AnswerAction::Resume,
        Some(2) => AnswerAction::Discard,
        _ => AnswerAction::KeepHeld,
    }
}

/// Legacy triage has a fixed issuer-owned keep-held slot. Its resume slot
/// may requeue, so it deliberately does not qualify for contextual resume.
pub(crate) fn contextual_keep_held(q: &Question, label: &str) -> bool {
    q.node == NODE
        && q.seat == SEAT
        && q.actions.is_empty()
        && q.choices.get(1).is_some_and(|c| c == label)
}

/// The note [`already_applied`] looks for, appended to (never replacing)
/// whatever [`Task::hold_reason`] already said - the original cause is still
/// worth reading in `magi task show` after the operator answers "not yet",
/// and [`Task::hold_manual`] would otherwise overwrite it outright.
fn keep_held_note(task: &Task, q: &Question, resolution: &str) -> String {
    let marker = format!("{} operator: {resolution}", marker_for(q));
    match task.hold_reason.as_deref() {
        Some(existing) if !existing.is_empty() => format!("{existing}\n{marker}"),
        _ => marker,
    }
}

/// File a fresh triage question for `task` and return it. The caller is
/// responsible for having already established there is no open one - see
/// [`latest_triage_question`] - so this never checks again.
fn file_question(
    questions: &Questions,
    ownership: &crate::task_chat::InquiryOwnership,
    task: &Task,
    bucket: Bucket,
    w: &Wording,
    now: Timestamp,
) -> Option<Question> {
    let (summary, why, choices) = match bucket {
        Bucket::MachineUnknown => (
            w.summary_machine_unknown(task),
            w.why_machine().to_owned(),
            w.choices3(),
        ),
        Bucket::ConductorOverride => (
            w.summary_conductor_override(task),
            task.resume_override
                .as_ref()
                .map(|o| w.why_conductor_override(o))
                .unwrap_or_default(),
            w.choices_conductor_override(),
        ),
        Bucket::Legacy => (
            w.summary_legacy(task),
            w.why_legacy().to_owned(),
            w.choices3(),
        ),
        Bucket::ManualStale => {
            let days = (now.as_second() - task.updated_at.as_second()) / (24 * 60 * 60);
            (
                w.summary_manual_stale(task, days),
                w.why_manual().to_owned(),
                w.choices2(),
            )
        }
    };
    let mut q = Question::new(
        task.id.clone(),
        NODE.to_owned(),
        SEAT.to_owned(),
        summary,
        w.detail(task, &why),
        choices,
    );
    q.triage_generation = Some(task.resumption.as_ref().map_or(0, |r| r.generation));
    questions.put_executable_in(&mut q, ownership).ok()?;
    Some(q)
}

/// What a deputy is told about a triage question: the task, why it is held and
/// what each position does, in the words [`interpret_answer`] /
/// [`interpret_deps_answer`] apply them. Written from the stored record; a task
/// that no longer exists is said to be missing, never reconstructed.
pub(crate) fn deputy_brief(q: &Question, queue: &Queue) -> String {
    let task = queue.get(&q.run).ok();
    let mut s = match &task {
        Some(t) => format!(
            "Task {} (`magi task show {}`), \"{}\", is {}. Hold source: {}. Reason: {}.",
            t.id,
            t.id,
            t.title,
            t.status.as_str(),
            match t.hold_source {
                Some(HoldSource::Machine) => "machine (an automatic hold)",
                Some(HoldSource::Manual) => "manual (an operator held it)",
                None => "unknown (an old record, treated as the operator's)",
            },
            t.hold_reason
                .as_deref()
                .or(t.last_error.as_deref())
                .unwrap_or("(none recorded)")
        ),
        None => format!("Task {} no longer exists (its record is gone).", q.run),
    };
    if task.as_ref().is_some_and(disk_wait_eligible) {
        s.push_str("\n\nDisk capacity waits are monitored automatically. No owner answer is needed for an untouched automatic disk triage question. Explicit holds, discussion and other approvals still require owner resolution.");
    }
    s.push_str("\n\nWhat each option does, by position:");
    let n = q.choices.len();
    for (i, c) in q.choices.iter().enumerate() {
        let effect = if q.node == DEPS_NODE {
            match i {
                0 => "the dependency is released so the loop runs it",
                1 => {
                    "the dependants are detached and the dependency task is DELETED; this cannot be undone"
                }
                2 => "the dependants are detached; the dependency stays as it is",
                _ => "nothing is applied",
            }
        } else {
            match i {
                0 => "the task is released and requeued",
                2 if n > 2 => "the task is DELETED; this cannot be undone",
                _ => "the task stays held, with the answer noted",
            }
        };
        s.push_str(&format!("\n- `{c}`: {effect}"));
    }
    s.push_str(
        "\n\nThe daemon applies the answer on its next pass; you apply nothing. When \
         the owner's words pick an option - including releasing a task they held \
         themselves, or deleting it - settle it.",
    );
    s
}

/// Move every `blocked` task whose `blocked_by` names a task or question id
/// that no longer exists to a machine hold, before the per-`held` walk
/// [`run_once`] does gets a look at it.
///
/// `crate::daemon::resolve_blockers` already catches the same situation on
/// every idle poll, and [`Queue::remove`] already catches it the moment a
/// dependency is deleted through `magi task rm` - both call the same
/// [`crate::queue::missing_blockers`]/[`crate::queue::missing_blocker_hold_reason`]
/// this does. This third copy exists because a dependency can also be deleted
/// by hand (the file just removed from disk, not through either of those
/// paths), and because a queue can carry a `blocked` task with a
/// long-since-deleted dependency from *before* either catch above ever
/// existed - and such a task is `blocked`, never `held`, so it is invisible
/// to the rest of this module without this pass. Running it here, first, is
/// also what makes `magi task triage` alone - with no daemon running at all -
/// enough to fix one: the task lands `held` in this same call, and the
/// ordinary loop below files its question in the very same pass.
fn quarantine_orphaned_blocked(queue: &Queue, questions: &Questions) -> Vec<String> {
    let mut quarantined = Vec::new();
    for listed in queue.list() {
        if listed.status != TaskStatus::Blocked || listed.blocked_by.is_empty() {
            continue;
        }
        let Ok(_claim) = queue.claim(&listed.id) else {
            continue;
        };
        let Ok(mut task) = queue.get(&listed.id) else {
            continue;
        };
        if task.status != TaskStatus::Blocked {
            continue;
        }
        let deleted = queue.apply_deleted_blockers(&mut task, questions);
        if !deleted.is_empty() && queue.put(&mut task).is_ok() {
            for id in &deleted {
                queue.note_dependency_deleted(&task, id);
            }
        }
        if task.status != TaskStatus::Blocked {
            continue;
        }
        let missing = crate::queue::missing_blockers(queue, questions, &task.blocked_by);
        if missing.is_empty() {
            continue;
        }
        let language = crate::lang::of_repo(&repo_for(&task));
        let mut cause = missing.clone();
        cause.sort();
        cause.dedup();
        task.notice_cause(&format!("missing-blockers:{}", cause.join(",")), None, None);
        task.hold_machine(Some(crate::queue::missing_blocker_hold_reason_in(
            &task.blocked_by,
            &missing,
            &language,
        )));
        if queue.put(&mut task).is_ok() {
            quarantined.push(task.id.clone());
        }
    }
    quarantined
}

/// Choices of a [`DEPS_NODE`] question, by position: release the root, discard
/// it, or detach the dependants from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepsAction {
    Release,
    Discard,
    Detach,
    /// An answer that matches no offered choice: nothing is changed.
    Nothing,
}

fn interpret_deps_answer(q: &Question) -> DepsAction {
    let resolution = q.resolution().unwrap_or_default();
    match q.choices.iter().position(|c| *c == resolution) {
        Some(0) => DepsAction::Release,
        Some(1) => DepsAction::Discard,
        Some(2) => DepsAction::Detach,
        _ => DepsAction::Nothing,
    }
}

/// Summary, body and choices of the question about one stuck root.
fn deps_texts(w: &Wording, root: &Task, dependants: &[&Task]) -> (String, String, Vec<String>) {
    let ja = w.lang == "ja";
    let reason = root
        .hold_reason
        .as_deref()
        .or(root.last_error.as_deref())
        .unwrap_or(if ja {
            "（記録なし）"
        } else {
            "(none recorded)"
        });
    let list: String = dependants
        .iter()
        .map(|t| format!("- {} {}\n", t.short(), t.title))
        .collect();
    if ja {
        (
            format!(
                "{} ({}) が {} 件のタスクを止めています",
                root.short(),
                root.status.as_str(),
                dependants.len()
            ),
            format!(
                "task: {} ({})\ntitle: {}\n状態: {}\n理由: {reason}\n\n\
                 このタスクは自動では実行されないため、依存している次のタスクは永遠に待ち続けます:\n{list}",
                root.id,
                root.short(),
                root.title,
                root.status.as_str()
            ),
            vec![
                "依存先を再開する".to_owned(),
                "依存先を捨てる（依存タスクは切り離して実行）".to_owned(),
                "依存タスクを切り離す（依存先はそのまま）".to_owned(),
            ],
        )
    } else {
        (
            format!(
                "{} ({}) is freezing {} blocked task(s)",
                root.short(),
                root.status.as_str(),
                dependants.len()
            ),
            format!(
                "task: {} ({})\ntitle: {}\nstatus: {}\nreason: {reason}\n\n\
                 Nothing in the loop will ever run this task, so these dependants wait \
                 forever:\n{list}",
                root.id,
                root.short(),
                root.title,
                root.status.as_str()
            ),
            vec![
                "release the dependency".to_owned(),
                "discard the dependency (dependants are detached and run)".to_owned(),
                "detach the dependants (the dependency stays as it is)".to_owned(),
            ],
        )
    }
}

/// Detach every `blocked` task that names `root` directly: drop that one id
/// from its `blocked_by` (`Task::unblock`), so a dependant with another
/// unresolved blocker keeps waiting on it. Idempotent.
fn detach_dependants(queue: &Queue, questions: &Questions, root: &str) {
    for listed in queue.list() {
        if listed.status != TaskStatus::Blocked || !listed.blocked_by.iter().any(|b| matches!(crate::queue::dependency(queue, questions, b), crate::queue::Dependency::Task(id) if id == root)) {
            continue;
        }
        let Ok(_claim) = queue.claim(&listed.id) else {
            continue;
        };
        let Ok(mut t) = queue.get(&listed.id) else {
            continue;
        };
        if t.status != TaskStatus::Blocked {
            continue;
        }
        for id in t.blocked_by.clone() {
            if matches!(crate::queue::dependency(queue, questions, &id), crate::queue::Dependency::Task(full) if full == root)
            {
                t.unblock(&id);
            }
        }
        let _ = queue.put(&mut t);
    }
}

/// Apply an answered [`DEPS_NODE`] question. Every step is idempotent, so a
/// pass that dies half way is finished by the next one. Returns whether the
/// answer was consumed.
fn apply_deps_answer(
    queue: &Queue,
    questions: &Questions,
    q: &Question,
    now: Timestamp,
    ownership: &crate::task_chat::InquiryOwnership,
) -> bool {
    let Ok(_claim) = queue.claim(&q.run) else {
        return false;
    };
    let Ok(current) = questions.get_for_effects_in(&q.id, ownership) else {
        return false;
    };
    if current.status != QuestionStatus::Answered {
        return false;
    }
    let q = &current;
    let Ok(mut root) = queue.get(&q.run) else {
        return false;
    };
    if root.triage_applied(&q.id) {
        return false;
    }
    if root.operator_decided_at.is_some_and(|at| at > q.asked_at)
        || q.triage_generation
            .is_some_and(|g| g != root.resumption.as_ref().map_or(0, |r| r.generation))
    {
        root.mark_triage_applied(&q.id);
        return queue.put(&mut root).is_ok();
    }
    match interpret_deps_answer(q) {
        DepsAction::Release => {
            if root.status == TaskStatus::Running {
                return false;
            }
            if crate::repair_budget::reconcile::blocker(queue, &root).is_some() {
                let Ok(receipt) = crate::repair_budget::reconcile::blocked_resume(q) else {
                    return false;
                };
                root.decision_effects.insert(q.id.clone(), receipt);
                root.mark_triage_applied(&q.id);
                return queue.put(&mut root).is_ok();
            }
            root.answers.push(crate::queue::AnsweredQuestion {
                question: q.summary.clone(),
                answer: q.resolution().unwrap_or_default(),
            });
            if crate::base_sync_recovery::held_task(queue, &root) {
                root.mark_triage_applied(&q.id);
                return queue.put(&mut root).is_ok();
            }
            let before = root.operator_decided_at;
            let contract =
                crate::repair_budget::contract(&root, crate::repair_budget::Kind::Review).ok();
            root.release_operator("triage-dependencies", Some(&q.id));
            if let Some(contract) = contract
                && let Ok(evidence) = crate::repair_budget::reconcile::effect_evidence(
                    q,
                    before,
                    &contract,
                    root.resumption.as_ref().expect("release"),
                )
            {
                root.decision_effects.insert(q.id.clone(), evidence);
            }
            root.resume_override = Some(OperatorResume {
                question_id: q.id.clone(),
                at: now,
                conductor_rehold: None,
                forced: false,
                pinned_run: None,
            });
        }
        DepsAction::Discard => {
            detach_dependants(queue, questions, &root.id);
            return queue.discard_triage(&root.id, questions, &q.id).is_ok();
        }
        DepsAction::Detach => detach_dependants(queue, questions, &root.id),
        // Matches no offered choice, so there is nothing to apply. Left unmarked:
        // marking it applied would read as "settled", and the next stuck check
        // would file an identical question at once. Unmarked, it stays an
        // answered question awaiting application, which suppresses re-asking.
        DepsAction::Nothing => return false,
    }
    root.mark_triage_applied(&q.id);
    queue.put(&mut root).is_ok()
}

fn repair_receipt_notice(queue: &Queue, task: &Task) -> bool {
    let Some(detail) = crate::repair_budget::reconcile::blocker(queue, task) else {
        return false;
    };
    if let Some(home) = queue.root().parent() {
        let _ = crate::task_inquiry::Outbox::at(home).file_for_occurrence(
            task,
            task.runs.last().cloned(),
            "repair-receipt-recovery".into(),
            &task
                .repair_grants
                .last()
                .expect("diagnosed receipt")
                .request
                .request,
            "Retained repair receipt needs reconciliation",
            &detail,
        );
    }
    true
}

/// One question per stuck dependency root. Answers are applied by the
/// pre-scheduling sweep. See [`crate::blockers`] for what "stuck" means.
///
/// Not re-asked: a root with an open or answered-unapplied question; one whose
/// own hold question ([`NODE`]) is still pending; and one whose question was
/// abandoned unless the root or a task it freezes changed since. An *applied*
/// answer leaves nothing stuck (released, discarded, or detached), so a root
/// that is stuck again is a new situation and is asked about afresh.
fn ask_about_stuck_roots(
    queue: &Queue,
    questions: &Questions,
    config_override: Option<&Path>,
    report: &mut Report,
) {
    let all = questions.list_for_effects();
    let tasks = queue.list();
    let recovery = RecoveryIndex::from_scan(queue, &tasks, all.clone());
    let inv = crate::blockers::Inventory::load_with_tasks(queue, questions, tasks);
    let mut frozen: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (id, roots) in inv.stuck() {
        for root in roots {
            if root != id {
                frozen.entry(root).or_default().push(id.clone());
            }
        }
    }
    // A cycle root freezes the others but may have no dependants of its own
    // listed above when it is alone; a root with nothing to name is not asked.

    let ownership = crate::task_chat::InquiryOwnership::default();
    for mut q in all
        .iter()
        .filter(|q| q.node == DEPS_NODE && q.status.open())
        .cloned()
    {
        if !frozen.contains_key(&q.run) {
            q.abandon("nothing waits on this task anymore");
            let _ = questions.put_executable_in(&mut q, &ownership);
        }
    }

    for (root_id, dependant_ids) in &frozen {
        let Ok(_claim) = queue.claim(root_id) else {
            continue;
        };
        // Recheck the chain immediately before asking: an earlier inventory
        // may have seen a held prerequisite that has since started running.
        let seeds: Vec<_> = std::iter::once(root_id)
            .chain(dependant_ids.iter())
            .filter_map(|id| queue.get(id).ok())
            .collect();
        let current = crate::blockers::Inventory::load_for_tasks(queue, questions, &seeds);
        let Some(root) = current.task(root_id) else {
            continue;
        };
        if crate::base_sync_recovery::held_task(queue, root) {
            continue;
        }
        let dependants: Vec<&Task> = dependant_ids
            .iter()
            .filter_map(|d| current.task(d))
            .filter(|t| current.stuck_roots(t).contains(root_id))
            .collect();
        if dependants.is_empty() {
            continue;
        }
        if repair_receipt_notice(queue, root) {
            continue;
        }
        let latest = recovery.latest(DEPS_NODE, root_id);
        match latest.as_ref() {
            Some(q) if q.status.open() || awaiting_task_chat(q, root) => continue,
            Some(q) if q.status == QuestionStatus::Answered && !root.triage_applied(&q.id) => {
                continue;
            }
            Some(q) if q.status == QuestionStatus::Abandoned => {
                let changed = root.updated_at > q.asked_at
                    || dependants.iter().any(|t| t.updated_at > q.asked_at);
                if !changed {
                    continue;
                }
            }
            _ => {}
        }
        if !recovery.pending(root).is_empty() {
            continue;
        }
        let cfg = Config::discover(&repo_for(root), config_override)
            .ok()
            .map(|(c, _)| c);
        let w = wording(cfg.as_ref().map_or("en", |c| c.graph.language.as_str()));
        let (summary, detail, choices) = deps_texts(w, root, &dependants);
        let mut question = Question::new(
            root.id.clone(),
            DEPS_NODE.to_owned(),
            SEAT.to_owned(),
            summary,
            detail,
            choices,
        );
        question.triage_generation = Some(root.resumption.as_ref().map_or(0, |r| r.generation));
        if questions
            .put_executable_in(&mut question, &ownership)
            .is_ok()
        {
            report.asked.push(root.id.clone());
        }
    }
}

/// Apply settled built-in answers before scheduling or conductor invocation.
/// Kept separate from diagnosis so queued tasks can recover legacy answers too.
pub fn apply_answers(queue: &Queue, questions: &Questions) -> Vec<String> {
    let mut applied = Vec::new();
    let ownership = crate::task_chat::InquiryOwnership::default();
    for q in questions.list_for_effects_in(&ownership) {
        if q.node == DEPS_NODE
            && q.status == QuestionStatus::Answered
            && apply_deps_answer(queue, questions, &q, Timestamp::now(), &ownership)
        {
            applied.push(q.run.clone());
        }
    }
    // Dependency answers above may change task and question state.
    let tasks = queue.list();
    let ownership = crate::task_chat::InquiryOwnership::from_tasks(&tasks);
    let recovery =
        RecoveryIndex::from_scan(queue, &tasks, questions.list_for_effects_in(&ownership));
    for listed in tasks {
        let Some(q) = recovery.latest(NODE, &listed.id) else {
            continue;
        };
        if q.status != QuestionStatus::Answered || already_applied(&listed, &q) {
            continue;
        }
        let Ok(_claim) = queue.claim(&listed.id) else {
            continue;
        };
        let Ok(mut task) = queue.get(&listed.id) else {
            continue;
        };
        // Settlement holds this same task claim; discard a reply read before
        // it won the claim rather than executing a settled answer.
        let Ok(q) = questions.get_for_effects_in(&q.id, &ownership) else {
            continue;
        };
        if q.status != QuestionStatus::Answered {
            continue;
        }
        if already_applied(&task, &q)
            || !matches!(task.status, TaskStatus::Held | TaskStatus::Queued)
        {
            continue;
        }
        let generation = task.resumption.as_ref().map_or(0, |r| r.generation);
        let action = interpret_answer(&q);
        let mut newer_decision = q.triage_generation.is_some_and(|g| g != generation)
            || task.resumption.as_ref().is_some_and(|r| r.at > q.asked_at)
            || task.held_at.is_some_and(|at| at > q.asked_at)
            || task.operator_decided_at.is_some_and(|at| at > q.asked_at);
        // A later explicit keep-held answer is itself a newer operator
        // decision, even when the question predates the release. Resume and
        // discard still require the question's original scope to match.
        if matches!(action, AnswerAction::KeepHeld)
            && q.answered_at.is_some_and(|at| {
                task.operator_decided_at
                    .is_none_or(|decision| at > decision)
                    && task.resumption.as_ref().is_none_or(|r| at > r.at)
            })
        {
            newer_decision = false;
        }
        // Audit the answer even when a later release or manual hold wins.
        let resolution = q.resolution().unwrap_or_default();
        task.answers.push(crate::queue::AnsweredQuestion {
            question: q.summary.clone(),
            answer: if newer_decision {
                format!("{resolution} (superseded by a later operator decision)")
            } else {
                resolution
            },
        });
        if !newer_decision {
            match action {
                AnswerAction::Resume if crate::base_sync_recovery::held_task(queue, &task) => {
                    // The original answer remains in answers; it cannot authorize
                    // a new implementation or an additional base rebase.
                }
                AnswerAction::Resume
                    if crate::repair_budget::reconcile::blocker(queue, &task).is_some() =>
                {
                    let Ok(receipt) = crate::repair_budget::reconcile::blocked_resume(&q) else {
                        continue;
                    };
                    task.decision_effects.insert(q.id.clone(), receipt);
                }
                AnswerAction::Resume => {
                    let before = task.operator_decided_at;
                    let contract =
                        crate::repair_budget::contract(&task, crate::repair_budget::Kind::Review)
                            .ok();
                    task.release_operator("triage", Some(&q.id));
                    if let Some(contract) = contract
                        && let Ok(evidence) = crate::repair_budget::reconcile::effect_evidence(
                            &q,
                            before,
                            &contract,
                            task.resumption.as_ref().expect("release"),
                        )
                    {
                        task.decision_effects.insert(q.id.clone(), evidence);
                    }
                    task.resume_override = Some(OperatorResume {
                        question_id: q.id.clone(),
                        at: Timestamp::now(),
                        conductor_rehold: None,
                        forced: true,
                        pinned_run: None,
                    });
                }
                AnswerAction::KeepHeld => {
                    let note = keep_held_note(&task, &q, &q.resolution().unwrap_or_default());
                    task.hold_manual(Some(note));
                }
                AnswerAction::Discard => {
                    if queue.discard_triage(&task.id, questions, &q.id).is_ok() {
                        applied.push(task.id);
                    }
                    continue;
                }
            }
        }
        task.mark_triage_applied(&q.id);
        if queue.put(&mut task).is_ok() {
            applied.push(task.id);
        }
    }
    applied
}
const DISK_SUPERSEDED: &str =
    "Automatic disk wait superseded unanswered triage; no owner answer or approval was created";

fn untouched_disk_question(q: &Question, task: &Task) -> bool {
    q.status.open()
        && q.node == NODE
        && q.seat == SEAT
        && q.run == task.id
        && q.routed_task.as_deref().is_none_or(|id| id == task.id)
        && q.triage_generation == Some(task.resumption.as_ref().map_or(0, |r| r.generation))
        && task.held_at.is_some_and(|at| q.asked_at >= at)
        && task.operator_decided_at.is_none_or(|at| at <= q.asked_at)
        && q.answer.is_none()
        && q.answered_at.is_none()
        && q.accepted_answer_at.is_none()
        && q.thread.is_empty()
        && q.actions.is_empty()
        && q.merge_target.is_none()
        && [&EN, &JA].iter().any(|w| {
            q.summary == w.summary_machine_unknown(task)
                && q.choices == w.choices3()
                && q.detail == w.detail(task, w.why_machine())
        })
}

/// Refresh old persisted wording without raising, relighting or delivering a notice.
fn refresh_disk_notices(home: &Path, task: &Task) {
    let notices = crate::notices::Notices::at(home.join("notifications"));
    let Some(task_notice) = crate::notices::task_held(task) else {
        return;
    };
    for (key, message) in [
        (task_notice.key, task_notice.message),
        (
            format!("disk:{}", repo_for(task).display()),
            format!(
                "Task {} is waiting for disk space. It will resume automatically when disk and output admission checks succeed.",
                task.id
            ),
        ),
    ] {
        let id = crate::notices::id_of(&key);
        if notices
            .get(&id)
            .is_ok_and(|n| n.key == key && n.message != message)
        {
            let _ = notices.update(&id, |n| {
                if n.key == key {
                    n.message = message;
                }
            });
        }
    }
}

/// All source changes use the question lock; native decisions additionally use
/// their execution claim. Abandonment precedes requeue, so interrupted writes
/// leave a held task that the next pass can safely finish, never fake consent.
fn resume_disk_task(
    queue: &Queue,
    questions: &Questions,
    task: &mut Task,
    ownership: &crate::task_chat::InquiryOwnership,
) -> anyhow::Result<bool> {
    let expected = task.clone();
    let home = questions
        .root()
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing question home"))?;
    let _lease = if let Some(id) = task.execution_runs().last() {
        anyhow::ensure!(
            task.resume_override
                .as_ref()
                .and_then(|o| o.pinned_run.as_ref())
                .is_none_or(|pin| pin == id),
            "selected recovery run differs from disk wait evidence"
        );
        let state = crate::run::RunState::load_under(id, home)?;
        anyhow::ensure!(
            state.id == *id && state.status.resumable() && !task.fresh_start,
            "retained execution cannot resume"
        );
        anyhow::ensure!(
            state.liveness_under(
                home,
                crate::daemon::is_working_on(home, id, Timestamp::now())
            ) == crate::run::Liveness::Dead,
            "retained execution is not proven idle"
        );
        Some(crate::driver::maintenance(home, id)?)
    } else {
        None
    };
    let outbox = crate::task_inquiry::Outbox::at(home);
    let all = questions.list_for_effects_in(ownership);
    let pending: Vec<_> = all
        .iter()
        .filter(|q| {
            // Legacy task/run aliases are blockers, never supersession proof.
            (!q.run.is_empty()
                && std::iter::once(task.id.as_str())
                    .chain(task.execution_runs().iter().map(String::as_str))
                    .any(|id| id == q.run || id.starts_with(&q.run) || id.ends_with(&q.run)))
                && (q.status.open()
                    || ((q.node == NODE || q.node == DEPS_NODE)
                        && (awaiting_task_chat(q, task)
                            || (q.status == QuestionStatus::Answered
                                && !already_applied(task, q)))))
        })
        .collect();
    // Multiple questions or any different approval requires owner resolution.
    anyhow::ensure!(pending.len() <= 1, "multiple decisions remain");
    let candidate = pending.first().copied();
    if let Some(q) = candidate {
        anyhow::ensure!(untouched_disk_question(q, task), "owner decision remains");
    }
    for inquiry in outbox.pending()? {
        if inquiry.task == task.id && inquiry.decision.is_some() {
            anyhow::ensure!(
                candidate.is_some_and(|q| q.id == inquiry.id),
                "unrelated decision remains"
            );
        }
    }
    if let Some(q) = candidate {
        let _execution = outbox.execution_claim(&q.id)?;
        questions.update_in(&q.id, ownership, |current| {
            anyhow::ensure!(untouched_disk_question(current, task), "question changed");
            let inquiry = match outbox.get(&q.id) {
                Ok(inquiry) => inquiry,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    anyhow::ensure!(
                        current.routed_chat.is_none(),
                        "delivered inquiry is missing; owner reconciliation required"
                    );
                    // Persist a no-effect native receipt even if delivery has
                    // not run yet. A concurrent producer replay preserves it.
                    outbox.enqueue(crate::task_chat::live_record(home, task, current))?
                }
                Err(error) => return Err(error),
            };
            anyhow::ensure!(
                inquiry.task == task.id
                    && inquiry.repo == task.repo
                    && inquiry.source == format!("{}:{}:{}", NODE, SEAT, task.id)
                    && current
                        .routed_chat
                        .as_ref()
                        .is_none_or(|chat| inquiry.delivered_to.as_ref() == Some(chat)),
                "inquiry binding changed"
            );
            outbox.update_decision(&q.id, |d| {
                if d.status == crate::task_decision::Status::Resolved {
                    anyhow::ensure!(
                        d.request.is_none()
                            && d.history
                                .last()
                                .is_some_and(|e| e.evidence == DISK_SUPERSEDED),
                        "decision already settled by owner"
                    );
                    return Ok(false);
                }
                anyhow::ensure!(
                    d.status == crate::task_decision::Status::AwaitingOwner
                        && d.request.is_none()
                        && d.source_answer_at.is_none(),
                    "owner request remains"
                );
                d.settle(DISK_SUPERSEDED)
            })?;
            current.abandon(DISK_SUPERSEDED);
            Ok(())
        })?;
    }
    if let Some(q) = candidate {
        task.mark_triage_applied(&q.id);
    }
    task.resume_disk_wait();
    queue.commit_disk_wait(task, &expected)?;
    let notices = crate::notices::Notices::at(home.join("notifications"));
    let keys = [
        format!("disk-recovered:{}", task.id),
        format!("disk:{}", repo_for(task).display()),
        format!("task:{}", task.id),
    ];
    let repo_waiting = queue
        .list()
        .iter()
        .any(|other| other.repo == task.repo && disk_wait_eligible(other));
    for notice in notices.list() {
        if notice.key == keys[0]
            || notice.key == keys[2]
            || (!repo_waiting && notice.key == keys[1])
        {
            let _ = notices.dismiss(&notice.id);
        }
    }
    Ok(candidate.is_some())
}

/// Run one deterministic triage pass over every `held` task in `queue`. No
/// model call anywhere in this function - see this module's own doc for what
/// each `HoldSource` gets instead.
///
/// `config_override` is threaded straight to [`Config::discover`], the same
/// role `daemon::Opts::config` plays for `daemon::prepare` - an explicit
/// `--config` from the caller, or `None` to let each task's own repository
/// pick its layers.
///
/// Safe to call on every daemon idle tick and from `magi task triage` alike:
/// a task already answered and applied is left alone (see
/// [`already_applied`]), and a task with an open question is left alone too,
/// so repeated calls with nothing new to say do nothing.
///
/// Also runs [`quarantine_orphaned_blocked`], so a `blocked` task whose
/// dependency no longer exists is caught and turned into a fresh `held`
/// question in this same pass, not left for a later call to notice.
/// Reconcile held tasks and requeue proven resource recoveries without owner consent.
pub fn run_once(
    queue: &Queue,
    questions: &Questions,
    config_override: Option<&Path>,
    now: Timestamp,
) -> Report {
    run_once_inner(queue, questions, config_override, now, false)
}

/// Existing maintenance service calls this even while other repositories run.
pub(crate) fn monitor_disk_waits(
    queue: &Queue,
    questions: &Questions,
    config: Option<&Path>,
    now: Timestamp,
) -> Report {
    run_once_inner(queue, questions, config, now, true)
}

fn run_once_inner(
    queue: &Queue,
    questions: &Questions,
    config_override: Option<&Path>,
    now: Timestamp,
    disk_only: bool,
) -> Report {
    run_once_with_probe(
        queue,
        questions,
        config_override,
        now,
        disk_only,
        machine_cause_resolved,
    )
}

fn run_once_with_probe(
    queue: &Queue,
    questions: &Questions,
    config_override: Option<&Path>,
    now: Timestamp,
    disk_only: bool,
    resolved: impl Fn(&Task, &Config) -> Option<bool>,
) -> Report {
    let mut report = Report {
        answered: if disk_only {
            Vec::new()
        } else {
            apply_answers(queue, questions)
        },
        quarantined: if disk_only {
            Vec::new()
        } else {
            quarantine_orphaned_blocked(queue, questions)
        },
        ..Report::default()
    };
    if !disk_only {
        ask_about_stuck_roots(queue, questions, config_override, &mut report);
    }
    let recovery = RecoveryIndex::load(questions);
    let ownership = crate::task_chat::InquiryOwnership::default();
    for listed in queue.list() {
        if listed.status != TaskStatus::Held {
            continue;
        }
        let Ok(_claim) = queue.claim(&listed.id) else {
            continue;
        };
        let Ok(mut task) = queue.get(&listed.id) else {
            continue;
        };
        // Re-read under the claim: a release or a re-hold landed by a human
        // between the listing above and the claim just taken must not be
        // clobbered by a decision based on the stale copy.
        if task.status != TaskStatus::Held {
            continue;
        }
        if crate::base_sync_recovery::held_task(queue, &task) {
            continue;
        }
        if repair_receipt_notice(queue, &task) {
            continue;
        }
        // A question about what this task freezes is already the one question
        // it owes the operator; a second, about the hold itself, would ask two
        // things at once.
        if deps_pending_in(&recovery, &task) {
            continue;
        }

        let explicit = match &task.overrides {
            Some(o) if !o.inherit_daemon_options => o.config.as_deref(),
            _ => config_override,
        };
        let cfg = Config::discover(&repo_for(&task), explicit)
            .ok()
            .map(|(c, _)| c);
        let w = wording(cfg.as_ref().map_or("en", |c| c.graph.language.as_str()));

        if disk_wait_known(&task, questions.root().parent().unwrap_or(Path::new(""))) {
            if task
                .disk_checked_at
                .is_some_and(|at| now.as_second() - at.as_second() < 60)
            {
                continue;
            }
            let expected = task.clone();
            task.disk_checked_at = Some(now);
            if !task.has_disk_capacity_cause() {
                // Legacy exact-format migration only; never re-bind a stale cause.
                task.mark_disk_capacity();
            }
            if queue.commit_disk_wait(&mut task, &expected).is_err() {
                continue;
            }
            refresh_disk_notices(questions.root().parent().unwrap_or(Path::new("")), &task);
            // Even missing configuration/evidence stays a monitored wait. Never
            // turn measurement failure or continued pressure into an owner poll.
            if cfg.as_ref().and_then(|c| resolved(&task, c)) == Some(true) {
                match resume_disk_task(queue, questions, &mut task, &ownership) {
                    Ok(superseded) => {
                        if superseded {
                            report.superseded.push(task.id.clone());
                        }
                        report.resumed.push(task.id.clone());
                    }
                    Err(error) => {
                        if !report.needs_owner.contains(&task.id) {
                            report.needs_owner.push(task.id.clone());
                        }
                        crate::notices::raise_in(
                        questions.root().parent().unwrap_or(Path::new("")),
                        crate::notices::Notice::warn(
                            &format!("disk-recovered:{}", task.id),
                            format!(
                                "Task {}: disk capacity recovered; automatic recovery is still blocked ({error}). Existing owner decisions and unrelated approvals require explicit resolution. Disk monitoring continues.",
                                task.id
                            ),
                        ).link(crate::notices::Link::Task { id: task.id.clone() }).about([task.id.clone()]),
                    );
                    }
                }
            }
            continue;
        }
        if disk_only {
            continue;
        }

        if let Some(q) = recovery.latest(NODE, &task.id) {
            if q.status.open() || awaiting_task_chat(&q, &task) {
                // Already asked, still waiting - nothing to do this pass.
                continue;
            }
            // Abandoned, or an already-applied answer: fall through to the
            // ordinary per-source handling below, which is how a stale
            // `HoldSource::Manual` re-ask - or a fresh machine/legacy
            // question, once a prior one settled the task back into a hold -
            // gets filed.
        }

        match task.hold_source {
            Some(HoldSource::Machine) => {
                let overridden = task
                    .resume_override
                    .as_ref()
                    .is_some_and(|o| o.conductor_rehold.is_some() && !o.forced);
                if overridden {
                    if file_question(
                        questions,
                        &ownership,
                        &task,
                        Bucket::ConductorOverride,
                        w,
                        now,
                    )
                    .is_some()
                    {
                        report.asked.push(task.id.clone());
                    }
                } else if cfg.as_ref().and_then(|c| resolved(&task, c)) == Some(true) {
                    task.release();
                    if queue.put(&mut task).is_ok() {
                        report.resumed.push(task.id.clone());
                    }
                } else if file_question(
                    questions,
                    &ownership,
                    &task,
                    Bucket::MachineUnknown,
                    w,
                    now,
                )
                .is_some()
                {
                    report.asked.push(task.id.clone());
                }
            }
            None => {
                if file_question(questions, &ownership, &task, Bucket::Legacy, w, now).is_some() {
                    report.asked.push(task.id.clone());
                }
            }
            Some(HoldSource::Manual) => {
                if manual_is_stale(&task, now)
                    && file_question(questions, &ownership, &task, Bucket::ManualStale, w, now)
                        .is_some()
                {
                    report.asked.push(task.id.clone());
                }
            }
        }
    }
    report
}

/// The open triage question about `task_id`, if any - what `magi task show`
/// prints so a held task's card names the question waiting on it, not only
/// its hold reason. `None` once it is answered or abandoned: nothing is
/// waiting on it anymore.
pub fn open_question_for(questions: &Questions, task_id: &str) -> Option<Question> {
    latest_triage_question(questions, task_id).filter(|q| q.status.open())
}

/// Does this module still have unfinished business with `task`?
///
/// True while its latest triage question is still open (waiting on an
/// answer), and true for a beat longer than [`open_question_for`] alone
/// would say: once answered, the question sits [`QuestionStatus::Answered`]
/// but unread until the next [`run_once`] pass actually applies it (see
/// [`already_applied`]), and [`run_once`] only ever runs on a fully idle
/// daemon tick - far less often than `crate::conduct` polls. A caller that
/// only checked "is a question open" would walk straight through that gap
/// the moment the operator answers, moving the task out of `held` before
/// [`run_once`] gets a turn - orphaning the very answer it was about to
/// apply, the same failure mode this function exists to keep `crate::conduct`
/// out of. `crate::conduct::apply_one` is exactly that caller.
pub fn pending_for(questions: &Questions, task: &Task) -> bool {
    !pending_recovery_questions(questions, task).is_empty()
}

/// The exact recovery decisions that prevent triage/conductor progress,
/// including routed decisions and answers not yet applied to this task.
pub(crate) fn pending_recovery_questions(questions: &Questions, task: &Task) -> Vec<Question> {
    RecoveryIndex::load(questions).pending(task)
}

fn awaiting_task_chat(q: &Question, task: &Task) -> bool {
    q.status != QuestionStatus::Abandoned
        && q.routed_chat.is_some()
        && !already_applied(task, q)
        && task.resumption.as_ref().map_or(0, |r| r.generation) == q.triage_generation.unwrap_or(0)
        && task.operator_decided_at.is_none_or(|at| at <= q.asked_at)
}

/// Is the latest [`DEPS_NODE`] question about `task` still open, or answered
/// but not yet applied? Same reasoning as [`pending_for`]: the gap between an
/// answer and the next [`run_once`] pass must not be walked through.
fn deps_pending_in(recovery: &RecoveryIndex, task: &Task) -> bool {
    match recovery.latest(DEPS_NODE, &task.id) {
        Some(q) if q.status.open() || awaiting_task_chat(&q, task) => true,
        Some(q) if q.status == QuestionStatus::Answered => !task.triage_applied(&q.id),
        _ => false,
    }
}

/// Every task id with an open triage question right now - what `magi task
/// list` uses to mark a held task that is already waiting on an operator
/// decision, rather than have it read identically to one nobody has looked
/// at yet.
pub fn open_task_ids(questions: &Questions) -> std::collections::BTreeSet<String> {
    RecoveryIndex::load(questions).open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::Answer;
    use crate::queue::Source;
    use jiff::SignedDuration;

    fn store() -> (tempfile::TempDir, Queue, Questions) {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let s = Questions::at(dir.path().join("questions"));
        (dir, q, s)
    }

    fn task(title: &str, repo: PathBuf) -> Task {
        Task::new(title.to_owned(), format!("do {title}"), repo, Source::Human)
    }

    fn disk_task(repo: PathBuf) -> Task {
        let mut t = task("capacity", repo);
        t.hold_machine(crate::disk::gate_in(1_477_050_368, 3_221_225_472, "ja"));
        t.mark_disk_capacity();
        t.attempts = 3;
        t
    }

    #[test]
    fn disk_wait_repeated_pressure_measurement_failure_and_recovery_cost_nothing() {
        let (dir, queue, questions) = store();
        let cfg = gate_disabled_config(dir.path());
        let mut t = disk_task(dir.path().to_owned());
        queue.put(&mut t).unwrap();
        let notices = crate::notices::Notices::at(dir.path().join("notifications"));
        let before = serde_json::to_value(notices.list()).unwrap();
        let start = Timestamp::now();
        let probes = std::cell::Cell::new(0);
        for seconds in [0, 1, 10, 60, 61, 120] {
            let report = run_once_with_probe(
                &queue,
                &questions,
                Some(&cfg),
                start + SignedDuration::from_secs(seconds),
                true,
                |_, _| {
                    probes.set(probes.get() + 1);
                    Some(false) // includes a failed measurement: never permission
                },
            );
            assert!(report.is_empty());
            assert_eq!(queue.get(&t.id).unwrap().attempts, 3);
        }
        assert_eq!(
            probes.get(),
            3,
            "resource probes are bounded to one per minute"
        );
        assert!(questions.list().is_empty());
        assert_eq!(serde_json::to_value(notices.list()).unwrap(), before);
        let recovered = run_once_with_probe(
            &queue,
            &questions,
            Some(&cfg),
            start + SignedDuration::from_secs(180),
            true,
            |_, _| Some(true),
        );
        assert_eq!(recovered.resumed, [t.id.clone()]);
        let saved = queue.get(&t.id).unwrap();
        assert_eq!(saved.status, TaskStatus::Queued);
        assert_eq!(saved.attempts, 3);
        let again = run_once_with_probe(
            &queue,
            &questions,
            Some(&cfg),
            start + SignedDuration::from_secs(240),
            true,
            |_, _| panic!("queued tasks are not relaunched"),
        );
        assert!(again.is_empty());
    }

    #[test]
    fn disk_wait_supersedes_only_untouched_automatic_questions_in_both_stores() {
        for routed in [false, true] {
            let (dir, queue, questions) = store();
            let cfg = gate_disabled_config(dir.path());
            let mut t = disk_task(dir.path().to_owned());
            queue.put(&mut t).unwrap();
            let q = file_question(
                &questions,
                &crate::task_chat::InquiryOwnership::default(),
                &t,
                Bucket::MachineUnknown,
                &JA,
                Timestamp::now(),
            )
            .unwrap();
            if routed {
                crate::task_chat::sweep(
                    &queue,
                    &crate::talk::Talks::at(dir.path().join("talks")),
                    &questions,
                    dir.path(),
                );
                assert!(
                    crate::task_inquiry::Outbox::at(dir.path())
                        .get(&q.id)
                        .is_ok()
                );
            }
            let start = Timestamp::now();
            for seconds in [0, 60, 120] {
                assert!(
                    run_once_with_probe(
                        &queue,
                        &questions,
                        Some(&cfg),
                        start + SignedDuration::from_secs(seconds),
                        true,
                        |_, _| Some(false)
                    )
                    .is_empty()
                );
                assert!(questions.read_history(&q.id).unwrap().status.open());
                assert_eq!(questions.list().len(), 1);
            }
            let report = monitor_disk_waits(
                &queue,
                &questions,
                Some(&cfg),
                start + SignedDuration::from_secs(180),
            );
            assert_eq!(report.resumed, [t.id.clone()], "routed={routed}");
            let source = questions.read_history(&q.id).unwrap();
            assert_eq!(source.status, QuestionStatus::Abandoned);
            assert!(source.answer.is_none());
            assert!(source.answered_at.is_none());
            assert!(!pending_for(&questions, &queue.get(&t.id).unwrap()));
            if routed {
                let d = crate::task_inquiry::Outbox::at(dir.path())
                    .get(&q.id)
                    .unwrap()
                    .decision
                    .unwrap();
                assert_eq!(d.status, crate::task_decision::Status::Resolved);
                assert!(
                    d.request.is_none(),
                    "system supersession fabricates no owner request"
                );
            }
        }
    }

    #[test]
    fn disk_wait_respects_owner_hold_discussion_and_unrelated_approvals() {
        for kind in [
            "manual",
            "answered",
            "discussion",
            "unrelated",
            "override",
            "native-hold",
            "missing-native",
        ] {
            let (dir, queue, questions) = store();
            let cfg = gate_disabled_config(dir.path());
            let mut t = disk_task(dir.path().to_owned());
            queue.put(&mut t).unwrap();
            let q = file_question(
                &questions,
                &crate::task_chat::InquiryOwnership::default(),
                &t,
                Bucket::MachineUnknown,
                &EN,
                Timestamp::now(),
            )
            .unwrap();
            match kind {
                "manual" => {
                    t.hold_manual(Some("owner hold".into()));
                    queue.put(&mut t).unwrap();
                }
                "answered" => {
                    questions
                        .update(&q.id, |q| q.answer(Answer::Choice(q.choices[1].clone())))
                        .unwrap();
                }
                "discussion" => {
                    questions
                        .update(&q.id, |q| q.say("please wait".to_owned()))
                        .unwrap();
                }
                "unrelated" => {
                    questions
                        .update(&q.id, |q| {
                            q.node = "land".into();
                            Ok(())
                        })
                        .unwrap();
                }
                "override" => {
                    t.resume_override = Some(OperatorResume {
                        question_id: "owner".into(),
                        at: Timestamp::now(),
                        conductor_rehold: Some("pending conductor decision".into()),
                        forced: true,
                        pinned_run: None,
                    });
                    queue.put(&mut t).unwrap();
                }
                "native-hold" | "missing-native" => {
                    crate::task_chat::sweep(
                        &queue,
                        &crate::talk::Talks::at(dir.path().join("talks")),
                        &questions,
                        dir.path(),
                    );
                    if kind == "native-hold" {
                        crate::task_inquiry::Outbox::at(dir.path())
                            .update_decision(&q.id, |d| d.accept("owner requested hold"))
                            .unwrap();
                    } else {
                        std::fs::remove_file(
                            dir.path()
                                .join("notifications/task-inquiries")
                                .join(format!("{}.json", q.id)),
                        )
                        .unwrap();
                    }
                }
                _ => unreachable!(),
            }
            let report = run_once(&queue, &questions, Some(&cfg), Timestamp::now());
            assert!(report.resumed.is_empty(), "{kind}");
            assert_eq!(queue.get(&t.id).unwrap().status, TaskStatus::Held, "{kind}");
            assert!(
                questions.read_history(&q.id).unwrap().status != QuestionStatus::Abandoned,
                "{kind}"
            );
        }
    }

    #[test]
    fn disk_wait_reuses_retained_execution_and_preserves_attempt_and_approval_evidence() {
        let (dir, queue, questions) = store();
        let cfg = gate_disabled_config(dir.path());
        let mut run = crate::run::RunState::new(
            dir.path().to_owned(),
            "main".into(),
            "base".into(),
            "capacity".into(),
            Config::default(),
        );
        run.status = crate::run::RunStatus::Blocked;
        run.driver_exited = true;
        run.save_under(dir.path()).unwrap();
        let mut t = disk_task(dir.path().to_owned());
        t.runs.push(run.id.clone());
        t.resume_override = Some(OperatorResume {
            question_id: "approved-recovery".into(),
            at: Timestamp::now(),
            conductor_rehold: None,
            forced: true,
            pinned_run: Some(run.id.clone()),
        });
        queue.put(&mut t).unwrap();
        let before = std::fs::read(dir.path().join("runs").join(&run.id).join("run.json")).unwrap();
        let report = run_once_with_probe(
            &queue,
            &questions,
            Some(&cfg),
            Timestamp::now(),
            true,
            |_, _| Some(true),
        );
        assert_eq!(report.resumed, [t.id.clone()]);
        let resumed = queue.get(&t.id).unwrap();
        assert_eq!(resumed.runs, t.runs);
        assert_eq!(resumed.disk_resume_run.as_deref(), Some(run.id.as_str()));
        assert_eq!(resumed.attempts, t.attempts);
        assert_eq!(resumed.resumption, t.resumption);
        assert_eq!(
            serde_json::to_value(&resumed.resume_override).unwrap(),
            serde_json::to_value(&t.resume_override).unwrap()
        );
        assert_eq!(
            std::fs::read(dir.path().join("runs").join(&run.id).join("run.json")).unwrap(),
            before
        );
        assert!(
            run_once_with_probe(
                &queue,
                &questions,
                Some(&cfg),
                Timestamp::now(),
                true,
                |_, _| panic!("no duplicate launch")
            )
            .is_empty()
        );
    }

    #[test]
    fn disk_wait_interrupted_question_supersession_is_idempotent() {
        for abandoned in [false, true] {
            let (dir, queue, questions) = store();
            let cfg = gate_disabled_config(dir.path());
            let mut t = disk_task(dir.path().to_owned());
            queue.put(&mut t).unwrap();
            let q = file_question(
                &questions,
                &crate::task_chat::InquiryOwnership::default(),
                &t,
                Bucket::MachineUnknown,
                &EN,
                Timestamp::now(),
            )
            .unwrap();
            let outbox = crate::task_inquiry::Outbox::at(dir.path());
            outbox
                .enqueue(crate::task_chat::live_record(dir.path(), &t, &q))
                .unwrap();
            outbox
                .update_decision(&q.id, |d| d.settle(DISK_SUPERSEDED))
                .unwrap();
            if abandoned {
                questions
                    .update(&q.id, |q| {
                        q.abandon(DISK_SUPERSEDED);
                        Ok(())
                    })
                    .unwrap();
            }
            let report = monitor_disk_waits(&queue, &questions, Some(&cfg), Timestamp::now());
            assert_eq!(report.resumed, [t.id.clone()]);
            assert_eq!(queue.get(&t.id).unwrap().attempts, 3);
            assert!(questions.read_history(&q.id).unwrap().answer.is_none());
            assert_eq!(
                outbox.get(&q.id).unwrap().decision.unwrap().history.len(),
                1
            );
        }
    }

    #[test]
    fn disk_wait_legacy_migration_requires_machine_provenance_and_exact_localized_format() {
        let (dir, queue, _) = store();
        for (language, source, suffix, expected) in [
            ("en", Some(HoldSource::Machine), "", true),
            ("ja", Some(HoldSource::Machine), "", true),
            ("ja", Some(HoldSource::Manual), "", false),
            ("ja", None, "", false),
            ("ja", Some(HoldSource::Machine), " owner hold", false),
        ] {
            let mut t = task("legacy", dir.path().to_owned());
            t.hold_machine(Some(format!(
                "{}{suffix}",
                crate::disk::gate_in(1477050368, 3221225472, language).unwrap()
            )));
            t.schema = 21;
            t.hold_source = source;
            queue.put(&mut t).unwrap();
            let loaded = queue.get(&t.id).unwrap();
            assert_eq!(loaded.machine_hold_cause.is_some(), expected);
            assert_eq!(disk_wait_eligible(&loaded), expected);
        }
    }

    #[test]
    fn disk_wait_refreshes_stale_notice_wording_without_redelivery() {
        let (dir, queue, questions) = store();
        let cfg = gate_disabled_config(dir.path());
        let mut t = disk_task(dir.path().to_owned());
        queue.put(&mut t).unwrap();
        let notices = crate::notices::Notices::at(dir.path().join("notifications"));
        let key = format!("disk:{}", t.repo.display());
        let n = notices
            .raise(crate::notices::Notice::warn(
                &key,
                "Free some, then release it from the queue.",
            ))
            .unwrap();
        notices.mark_read(&n.id).unwrap();
        let before = notices.get(&n.id).unwrap();
        let report = run_once_with_probe(
            &queue,
            &questions,
            Some(&cfg),
            Timestamp::now(),
            true,
            |_, _| Some(false),
        );
        assert!(report.is_empty());
        let after = notices.get(&n.id).unwrap();
        assert!(after.message.contains("automatically"));
        assert_eq!(before.generation, after.generation);
        assert_eq!(before.count, after.count);
        assert_eq!(before.read_at, after.read_at);
    }

    #[test]
    fn disk_wait_owner_discard_is_applied_without_automatic_resumption() {
        let (dir, queue, questions) = store();
        let cfg = gate_disabled_config(dir.path());
        let mut t = disk_task(dir.path().to_owned());
        queue.put(&mut t).unwrap();
        let q = file_question(
            &questions,
            &crate::task_chat::InquiryOwnership::default(),
            &t,
            Bucket::MachineUnknown,
            &EN,
            Timestamp::now(),
        )
        .unwrap();
        questions
            .update(&q.id, |q| q.answer(Answer::Choice(q.choices[2].clone())))
            .unwrap();
        let report = run_once(&queue, &questions, Some(&cfg), Timestamp::now());
        assert!(report.resumed.is_empty());
        assert!(queue.get(&t.id).is_err());
        assert!(questions.read_history(&q.id).unwrap().answer.is_some());
    }

    #[test]
    fn disk_wait_commit_cannot_overwrite_a_concurrent_owner_hold() {
        let (dir, queue, questions) = store();
        let mut stale = disk_task(dir.path().to_owned());
        queue.put(&mut stale).unwrap();
        let mut current = queue.get(&stale.id).unwrap();
        current.hold_manual(Some("wait for owner".into()));
        queue.put(&mut current).unwrap();
        assert!(
            resume_disk_task(
                &queue,
                &questions,
                &mut stale,
                &crate::task_chat::InquiryOwnership::default()
            )
            .is_err()
        );
        assert_eq!(
            queue.get(&current.id).unwrap().hold_source,
            Some(HoldSource::Manual)
        );
    }

    #[test]
    fn recovery_index_preserves_supersession_and_ambiguous_file_references() {
        let (dir, queue, questions) = store();
        let mut t = task("supersession", dir.path().join("repo"));
        t.id = "task-alpha-end".into();
        queue.put(&mut t).unwrap();
        let at: Timestamp = "2026-11-01T00:00:00Z".parse().unwrap();
        let mut all = Vec::new();
        for (i, run) in ["task-alpha-end", "task-alpha", "end"]
            .into_iter()
            .enumerate()
        {
            let mut q = Question::new(
                run.into(),
                NODE.into(),
                SEAT.into(),
                "superseded".into(),
                String::new(),
                EN.choices3(),
            );
            q.id = format!("superseded-{i}");
            q.asked_at = at;
            all.push(q);
        }
        let evidence: Vec<_> = all
            .iter()
            .map(|q| {
                (
                    q.id.clone(),
                    crate::repair_budget::reconcile::source(q).unwrap(),
                )
            })
            .collect();
        t.repair_reconciliations.push(
            serde_json::from_value(serde_json::json!({
                "request": {
                    "task": t.id, "run": "run", "head": "head", "expect_pass": 0,
                    "receipt": "grant", "question": "original", "answer_at": at,
                    "request": "reconcile", "reason": "fixture"
                },
                "grant": {
                    "request": {
                        "task": t.id, "run": "run", "kind": "review", "rounds": 1,
                        "head": "head", "expect_pass": 0, "request": "grant", "reason": "fixture"
                    },
                    "at": at, "decision": null, "contract": "fixture"
                },
                "source": "fixture", "application": "fixture", "decision": at,
                "superseded_questions": evidence
            }))
            .unwrap(),
        );
        queue.put(&mut t).unwrap();
        let index = RecoveryIndex::from_scan(&queue, &[t.clone()], all.clone());
        assert!(index.open.is_empty());
        // Unreadable filenames still make prefixes and suffixes ambiguous.
        std::fs::write(queue.path_of("task-alphabet-other"), "{").unwrap();
        std::fs::write(queue.path_of("broken-end"), "{").unwrap();
        let index = RecoveryIndex::from_scan(&queue, &[t.clone()], all.clone());
        assert_eq!(index.open, ["end".into(), "task-alpha".into()].into());
        all[0].detail.push_str("changed evidence");
        let index = RecoveryIndex::from_scan(&queue, &[t], all);
        assert!(index.open.contains("task-alpha-end"));
        assert_eq!(questions.list().len(), 0);
    }

    #[test]
    fn ownership_index_requires_positive_foreground_provenance() {
        let (dir, queue, _) = store();
        let mut run = crate::run::RunState::new(
            dir.path().join("repo"),
            "main".into(),
            "origin".into(),
            "fixture".into(),
            Config::default(),
        );
        run.origin = Some(crate::run::Origin::operator());
        run.save_under(dir.path()).unwrap();
        let foreground = run.id.clone();
        run.id = "owned-run".into();
        run.origin = Some(crate::run::Origin::queue("missing-task"));
        run.save_under(dir.path()).unwrap();
        let mut task = task("owner", dir.path().join("repo"));
        task.runs = vec!["linked-run".into()];
        queue.put(&mut task).unwrap();
        let ownership = crate::task_chat::InquiryOwnership::default();
        for (run, cwd, node, seat, expected) in [
            (foreground.as_str(), true, "node", "seat", true),
            (foreground.as_str(), false, "node", "seat", false),
            ("owned-run", true, "ask", "operator", false),
            ("linked-run", true, "ask", "operator", false),
            ("missing-run", true, "node", "seat", false),
            ("missing-run", false, "ask", "operator", true),
        ] {
            let mut q = Question::new(
                run.into(),
                node.into(),
                seat.into(),
                "provenance".into(),
                String::new(),
                Vec::new(),
            );
            q.cwd = cwd.then(|| dir.path().join("repo").display().to_string());
            assert_eq!(standalone_by_scanning(dir.path(), &q), expected);
            assert_eq!(
                crate::task_chat::standalone_inquiry_in(dir.path(), &q, &ownership),
                expected
            );
        }
    }

    // Deliberately retain the pre-index predicate as an independent oracle.
    fn standalone_by_scanning(home: &Path, q: &Question) -> bool {
        if q.routed_task.is_some()
            || (q.node == crate::bump::NOTICE_NODE && q.seat == "release-watch")
        {
            return false;
        }
        if q.legacy_foreground || q.run.is_empty() {
            return true;
        }
        if Queue::at(home.join("queue"))
            .list()
            .iter()
            .any(|task| task.id == q.run || task.runs.contains(&q.run))
        {
            return false;
        }
        let run = crate::run::RunState::load_under(&q.run, home).ok();
        let origin = run.as_ref().and_then(|run| run.origin.as_ref());
        if origin.is_some_and(|origin| origin.task.is_some()) {
            return false;
        }
        (q.node == "ask" && q.seat == "operator")
            || (q.cwd.is_some() && origin.is_some_and(|origin| origin.task.is_none()))
    }

    fn scan_counts() -> (usize, usize, usize) {
        (
            crate::queue::LIST_READS.with(|c| c.replace(0)),
            crate::queue::TASK_READS.with(|c| c.replace(0)),
            crate::ask::QUESTION_SCANS.with(|c| c.replace(0)),
        )
    }

    #[test]
    fn creating_and_updating_questions_share_one_queue_read() {
        let (dir, queue, questions) = store();
        let size = 200;
        let mut ids = Vec::new();
        for i in 0..size {
            let mut t = task("scan fixture", dir.path().join("repo"));
            t.id = format!("task-{i:04}");
            t.runs = vec![format!("run-{i:04}")];
            t.hold_manual(Some("held for scan fixture".into()));
            queue.put(&mut t).unwrap();
            ids.push(t.id);
        }
        let ownership = crate::task_chat::InquiryOwnership::default();
        scan_counts();
        let mut created = Vec::new();
        for id in &ids {
            let mut q = Question::new(
                id.clone(),
                NODE.into(),
                SEAT.into(),
                "scan fixture".into(),
                String::new(),
                EN.choices3(),
            );
            questions.put_executable_in(&mut q, &ownership).unwrap();
            assert_eq!(q.routed_task.as_deref(), Some(id.as_str()));
            created.push(q.id);
        }
        for id in &created {
            questions
                .update_in(id, &ownership, |q| {
                    q.answer_delivered = true;
                    Ok(())
                })
                .unwrap();
        }
        let (lists, _, _) = scan_counts();
        assert_eq!(lists, 1, "one queue read for {size} creations and updates");
    }

    #[test]
    fn recovery_scans_are_linear_and_match_legacy_decisions() {
        let mut previous = None;
        for size in [150, 300] {
            let (dir, queue, questions) = store();
            let at: Timestamp = "2026-11-01T00:00:00Z".parse().unwrap();
            let mut sources = Vec::new();
            let mut expected_applied = std::collections::BTreeSet::new();
            for i in 0..size {
                let mut t = task("scan fixture", dir.path().join("repo"));
                t.id = format!("task-{i:04}");
                t.runs = vec![format!("run-{i:04}")];
                t.hold_manual(Some("held for scan fixture".into()));
                queue.put(&mut t).unwrap();
                let mut q = Question::new(
                    t.id.clone(),
                    NODE.into(),
                    SEAT.into(),
                    "scan fixture".into(),
                    String::new(),
                    EN.choices3(),
                );
                q.id = format!("question-{i:04}");
                q.asked_at = at;
                match i % 10 {
                    0 => {
                        q.answer(Answer::Choice(q.choices[1].clone())).unwrap();
                        q.accepted_answer_at = q.answered_at;
                        expected_applied.insert(t.id.clone());
                    }
                    1 => q.node = DEPS_NODE.into(),
                    2 => {
                        q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
                    }
                    3 => {
                        q.run = format!("missing-{i}");
                        q.legacy_foreground = true;
                    }
                    4 => {
                        q.run.clear();
                        q.node = crate::bump::NOTICE_NODE.into();
                        q.seat = "release-watch".into();
                    }
                    5 => {
                        q.run = format!("missing-{i}");
                        q.node = "ask".into();
                        q.seat = "operator".into();
                    }
                    6 => q.run = t.runs[0].clone(),
                    7 => q.run = format!("missing-{i}"),
                    8 => q.abandon("fixture"),
                    _ => {}
                }
                questions.put(&mut q).unwrap();
                sources.push(q.id.clone());
                if i % 10 == 9 {
                    q.id = format!("question-{i:04}-tie");
                    questions.put(&mut q).unwrap();
                    sources.push(q.id.clone());
                }
            }
            let migration = dir.path().join("notifications/task-chat-migration-v1");
            std::fs::create_dir_all(&migration).unwrap();
            std::fs::write(
                migration.join("state.json"),
                serde_json::to_vec(&serde_json::json!({
                    "schema": 1, "classification_revision": 1, "pending": [], "sources": sources
                }))
                .unwrap(),
            )
            .unwrap();

            scan_counts();
            let all = questions.list_for_effects();
            let (lists, reads, scans) = scan_counts();
            assert_eq!((lists, reads, scans), (1, size, 1));
            let tasks = queue.list();
            let ownership = crate::task_chat::InquiryOwnership::from_tasks(&tasks);
            for q in &all {
                assert_eq!(
                    crate::task_chat::standalone_inquiry_in(dir.path(), q, &ownership),
                    standalone_by_scanning(dir.path(), q),
                    "{}",
                    q.id,
                );
                if q.id.ends_with("0002") {
                    assert!(q.answer.is_none(), "saved consent remains fenced");
                }
            }
            let recovery = RecoveryIndex::from_scan(&queue, &tasks, all.clone());
            for task in &tasks {
                for node in [NODE, DEPS_NODE] {
                    let expected = all
                        .iter()
                        .filter(|q| {
                            q.node == node
                                && q.run == task.id
                                && !queue.get(&q.run).ok().is_some_and(|t| {
                                    crate::repair_budget::reconcile::superseded_question(&t, q)
                                })
                        })
                        .max_by(|a, b| a.asked_at.cmp(&b.asked_at).then_with(|| a.id.cmp(&b.id)));
                    assert_eq!(
                        recovery.latest(node, &task.id).map(|q| q.id),
                        expected.map(|q| q.id.clone())
                    );
                }
            }

            scan_counts();
            crate::daemon::apply_task_choice_actions(&queue, &questions, dir.path());
            assert_eq!(
                scan_counts(),
                (1, size, 1),
                "choice-action rechecks share ownership"
            );

            scan_counts();
            let applied: std::collections::BTreeSet<_> =
                apply_answers(&queue, &questions).into_iter().collect();
            let counts = scan_counts();
            assert_eq!(applied, expected_applied);
            assert!(counts.0 <= 3, "queue scans: {counts:?}");
            assert!(counts.1 <= 4 * size, "task reads: {counts:?}");
            assert_eq!(counts.2, 2, "question scans: {counts:?}");
            if let Some((_, reads, scans)) = previous {
                assert!(counts.1 <= 2 * reads);
                assert_eq!(counts.2, scans);
            }
            previous = Some(counts);

            let config = gate_disabled_config(dir.path());
            scan_counts();
            run_once(&queue, &questions, Some(&config), Timestamp::now());
            let lap = scan_counts();
            assert!(lap.0 <= 12, "triage queue scans: {lap:?}");
            assert!(lap.1 <= 14 * size, "triage task reads: {lap:?}");
            assert!(lap.2 <= 6, "triage question scans: {lap:?}");
        }
    }

    #[test]
    fn run_once_filing_many_new_questions_reads_the_queue_a_constant_number_of_times() {
        let (dir, queue, questions) = store();
        let size = 200;
        for i in 0..size {
            let mut t = task("scan fixture", dir.path().join("repo"));
            t.id = format!("task-{i:04}");
            t.hold_machine(Some("gate red".to_owned()));
            queue.put(&mut t).unwrap();
        }
        scan_counts();
        let report = run_once(&queue, &questions, None, Timestamp::now());
        let (lists, _, _) = scan_counts();
        assert_eq!(report.asked.len(), size, "every held task is asked about");
        assert!(
            lists <= 12,
            "queue scans while filing {size} questions: {lists}"
        );
    }

    #[test]
    fn base_recovery_stops_suppress_generic_and_dependency_resume_offers() {
        let (dir, queue, questions) = store();
        let mut root = task("base stop", dir.path().to_path_buf());
        let mut run = crate::run::RunState::new(
            dir.path().to_path_buf(),
            "main".into(),
            "base".into(),
            "retained implementation".into(),
            Config::default(),
        );
        run.status = crate::run::RunStatus::Voting;
        run.base_sync = Some(crate::run::BaseSync {
            tip: "tip".into(),
            behind: 3,
            attempts: 4,
            conflict: Some("persisted exhaustion".into()),
            already_in: None,
        });
        run.save_under(dir.path()).unwrap();
        root.link_run(&run.id);
        root.hold_machine(Some("base stop".into()));
        queue.put(&mut root).unwrap();
        let mut dependent = task("dependent", dir.path().to_path_buf());
        dependent.blocked_by = vec![root.id.clone()];
        dependent.status = TaskStatus::Blocked;
        queue.put(&mut dependent).unwrap();
        for _ in 0..2 {
            let report = run_once(&queue, &questions, None, Timestamp::now());
            assert!(report.asked.is_empty());
            assert!(report.resumed.is_empty());
        }
        // A generic question offered by an older launcher cannot release this
        // task into a replacement competition after the fix is installed.
        let q = file_question(
            &questions,
            &crate::task_chat::InquiryOwnership::default(),
            &root,
            Bucket::MachineUnknown,
            &EN,
            Timestamp::now(),
        )
        .unwrap();
        let mut q = q;
        q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
        questions.put_executable(&mut q).unwrap();
        apply_answers(&queue, &questions);
        let saved = queue.get(&root.id).unwrap();
        assert_eq!(saved.status, TaskStatus::Held);
        assert!(saved.triage_applied(&q.id));
        assert_eq!(saved.runs, vec![run.id]);
    }

    #[test]
    fn resource_holds_only_auto_release_a_proven_cache_wait_timeout() {
        use crate::run::{ResourceBlock, ResourceBlockCause, RunState};
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default(); // No configured cache means no busy lease.
        assert!(cfg.cache_dir().is_none());
        let mut state = RunState::new(
            dir.path().into(),
            "main".into(),
            "base".into(),
            "verify".into(),
            cfg.clone(),
        );
        let mut task = task("verify", dir.path().into());
        task.runs = vec![state.id.clone()];
        task.hold_machine(Some("[resource] cache operation stopped".into()));
        for cause in [
            ResourceBlockCause::Other,
            ResourceBlockCause::Unrecorded,
            ResourceBlockCause::Cancelled { park: None },
        ] {
            state.resource_block = Some(ResourceBlock {
                node: "gate".into(),
                kind: "shared build cache".into(),
                detail: "persistent metadata or manifest error".into(),
                cause,
                at: Timestamp::now(),
            });
            state.save_under(dir.path()).unwrap();
            // Repeated triage must retain the hold even though the lease is free.
            for _ in 0..3 {
                assert_eq!(machine_cause_resolved_under(&task, &cfg, dir.path()), None);
            }
        }
        let block = state.resource_block.as_mut().unwrap();
        block.cause = ResourceBlockCause::Timeout {
            waited_secs: 10,
            limit_secs: 10,
        };
        state.save_under(dir.path()).unwrap();
        assert_eq!(
            machine_cause_resolved_under(&task, &cfg, dir.path()),
            Some(true)
        );
        state.resource_block.as_mut().unwrap().kind = "repository coordination".into();
        state.save_under(dir.path()).unwrap();
        assert_eq!(machine_cause_resolved_under(&task, &cfg, dir.path()), None);
        task.runs = vec!["unreadable-run".into()];
        assert_eq!(machine_cause_resolved_under(&task, &cfg, dir.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn disk_admission_holds_recheck_live_reservations_and_the_floor() {
        use crate::retention::OutputLease;
        use crate::run::{ResourceBlock, ResourceBlockCause, RunState};
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let mut cfg = Config::default();
        cfg.graph.worktree_root = Some(home.join("worktrees"));
        cfg.disk.min_free_bytes = 100;
        let cache = home.join("cache");
        cfg.verify.gate = vec![format!(
            "CARGO_TARGET_DIR='{}' cargo build",
            cache.display()
        )];
        let mut state = RunState::new(
            home.clone(),
            "main".into(),
            "base".into(),
            "verify".into(),
            cfg.clone(),
        );
        let saved_cache = home.join("saved-cache");
        state.config.verify.gate = vec![format!(
            "CARGO_TARGET_DIR='{}' cargo build",
            saved_cache.display()
        )];
        state.resource_block = Some(ResourceBlock {
            node: "gate".into(),
            kind: "disk admission".into(),
            detail: "writer headroom unavailable".into(),
            cause: ResourceBlockCause::Other,
            at: Timestamp::now(),
        });
        state.save_under(&home).unwrap();
        let mut task = task("verify", home.clone());
        task.runs = vec![state.id.clone()];
        task.hold_machine(Some(format!(
            "{}{}; no attempt was spent; resume to retry the affected operation",
            crate::cache::RESOURCE_HOLD,
            state.resource_block.as_ref().unwrap().description()
        )));
        task.schema = 21; // exact resource-block evidence migrates an older hold
        let gib = 1024 * 1024 * 1024;
        let free = std::cell::Cell::new(100 + gib - 1);
        let check = || {
            machine_cause_resolved_with_admission(&task, &cfg, &home, |paths, floor| {
                assert!(
                    paths.contains(&cache),
                    "the cache volume must also be checked"
                );
                assert!(
                    paths.contains(&saved_cache),
                    "saved and current cache locations both matter"
                );
                assert!(paths.contains(&state.worktree_root()));
                OutputLease::admit_with(&home, paths, floor, |_| Ok(free.get())).is_ok()
            })
        };
        assert_eq!(check(), Some(false), "the unchanged floor still applies");
        assert_eq!(
            machine_cause_resolved_with_admission(&task, &cfg, &home, |paths, floor| {
                OutputLease::admit_with(&home, paths, floor, |_| {
                    Err(anyhow::anyhow!("measurement unavailable"))
                })
                .is_ok()
            }),
            Some(false),
            "failed output-volume measurements must remain closed"
        );
        free.set(100 + gib);
        let writer = OutputLease::admit_with(
            &home,
            std::slice::from_ref(&home),
            cfg.disk.min_free_bytes,
            |_| Ok(free.get()),
        )
        .unwrap();
        assert_eq!(
            check(),
            Some(false),
            "df alone cannot resolve a live reservation"
        );
        drop(writer);
        assert_eq!(
            check(),
            Some(true),
            "releasing headroom resolves the machine hold"
        );
        assert_eq!(
            check(),
            Some(true),
            "the probe must release its temporary reservation"
        );

        // A broken admission namespace still fails closed after free space recovers.
        let namespace = home.join("disk-admission");
        let saved = home.join("saved-admission");
        std::fs::rename(&namespace, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &namespace).unwrap();
        assert_eq!(check(), Some(false));
        std::fs::remove_file(&namespace).unwrap();
        std::fs::rename(saved, namespace).unwrap();
        assert_eq!(check(), Some(true));

        state.resource_block.as_mut().unwrap().cause = ResourceBlockCause::Cancelled { park: None };
        state.save_under(&home).unwrap();
        assert_eq!(
            machine_cause_resolved_with_admission(&task, &cfg, &home, |_, _| panic!(
                "a cancelled operation must remain held"
            )),
            None
        );
        state.resource_block.as_mut().unwrap().kind = "incremental cache admission".into();
        state.resource_block.as_mut().unwrap().cause = ResourceBlockCause::Other;
        state.save_under(&home).unwrap();
        assert_eq!(
            machine_cause_resolved_with_admission(&task, &cfg, &home, |_, _| panic!(
                "unproven incremental caches must remain held"
            )),
            None
        );
        task.runs.push("unreadable-run".into());
        assert_eq!(
            machine_cause_resolved_with_admission(&task, &cfg, &home, |_, _| panic!(
                "missing evidence must not release a hold"
            )),
            None
        );
    }

    /// `[disk] min_free_bytes = 0` written next to a fictional repo, so
    /// `machine_cause_resolved` never has to ask the real disk anything - the
    /// same fixture pattern `daemon`'s own idle-loop tests use.
    fn gate_disabled_config(dir: &std::path::Path) -> PathBuf {
        let config = dir.join("magi.toml");
        std::fs::write(&config, "[disk]\nmin_free_bytes = 0\n").unwrap();
        config
    }

    #[test]
    fn resumption_later_keep_held_answer_wins_but_an_older_answer_cannot_undo_release() {
        for source in ["cli", "web"] {
            for answer_after_release in [true, false] {
                let (dir, queue, questions) = store();
                let mut t = task("recovery", dir.path().join("repo"));
                t.hold_machine(Some("inspect remote first".into()));
                queue.put(&mut t).unwrap();
                let mut q = file_question(
                    &questions,
                    &crate::task_chat::InquiryOwnership::default(),
                    &t,
                    Bucket::MachineUnknown,
                    &JA,
                    Timestamp::now(),
                )
                .unwrap();
                if !answer_after_release {
                    q.answer(Answer::Choice(q.choices[1].clone())).unwrap();
                    questions.put(&mut q).unwrap();
                }
                t.release_operator(source, None);
                queue.put(&mut t).unwrap();
                if answer_after_release {
                    q.answer(Answer::Choice(q.choices[1].clone())).unwrap();
                    questions.put(&mut q).unwrap();
                }
                apply_answers(&queue, &questions);
                let after = queue.get(&t.id).unwrap();
                assert!(after.triage_applied(&q.id));
                assert_eq!(after.operator_held(), answer_after_release);
                assert_eq!(
                    after.status,
                    if answer_after_release {
                        TaskStatus::Held
                    } else {
                        TaskStatus::Queued
                    }
                );
                assert!(apply_answers(&queue, &questions).is_empty());
            }
        }
    }

    #[test]
    fn explicit_resumption_applies_legacy_queued_triage_once_and_preserves_later_holds() {
        let (dir, queue, questions) = store();
        let mut t = task("recover", dir.path().join("repo"));
        t.hold_machine(Some("inspect first".into()));
        queue.put(&mut t).unwrap();
        let mut q = file_question(
            &questions,
            &crate::task_chat::InquiryOwnership::default(),
            &t,
            Bucket::MachineUnknown,
            &JA,
            Timestamp::now(),
        )
        .unwrap();
        assert!(q.actions.is_empty());
        q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
        questions.put(&mut q).unwrap();
        // Older release paths queued the task without recording approval.
        t.release();
        t.instruction = "Reuse approved PR head".into();
        queue.put(&mut t).unwrap();
        assert_eq!(apply_answers(&queue, &questions), [t.id.clone()]);
        let mut after = queue.get(&t.id).unwrap();
        assert_eq!(
            after.resumption.as_ref().unwrap().question_id.as_deref(),
            Some(q.id.as_str())
        );
        assert_eq!(
            after.resumption.as_ref().unwrap().instruction,
            t.instruction
        );
        assert_eq!(after.answers[0].answer, JA.resume);
        assert!(after.triage_applied(&q.id));
        assert!(apply_answers(&Queue::at(dir.path().join("queue")), &questions).is_empty());
        after.hold_machine(Some("new blocker".into()));
        queue.put(&mut after).unwrap();
        let mut late = file_question(
            &questions,
            &crate::task_chat::InquiryOwnership::default(),
            &after,
            Bucket::MachineUnknown,
            &EN,
            Timestamp::now(),
        )
        .unwrap();
        after.hold_manual(Some("keep held now".into()));
        queue.put(&mut after).unwrap();
        late.answer(Answer::Choice(late.choices[0].clone()))
            .unwrap();
        questions.put(&mut late).unwrap();
        apply_answers(&queue, &questions);
        let held = queue.get(&t.id).unwrap();
        assert!(held.operator_held());
        assert_eq!(held.hold_reason.as_deref(), Some("keep held now"));
        assert_eq!(held.resumption.as_ref().unwrap().generation, 1);
    }

    #[test]
    fn a_resolved_machine_hold_is_requeued_automatically() {
        let (dir, q, questions) = store();
        let config = gate_disabled_config(dir.path());
        let mut t = task("disk pressure", dir.path().join("repo"));
        t.hold_machine(Some(
            "not enough free space to start a run: 10 bytes free, 100 required by \
             `[disk] min_free_bytes`"
                .to_owned(),
        ));
        t.schema = 21; // persisted admission hold from before typed causes
        q.put(&mut t).unwrap();

        let report = run_once(&q, &questions, Some(&config), Timestamp::now());
        assert_eq!(report.resumed, [t.id.clone()]);
        assert!(report.asked.is_empty());

        let back = q.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Queued);
        assert!(back.hold_source.is_none());
        assert!(questions.list().is_empty(), "nothing needed asking");
    }

    #[test]
    fn a_machine_hold_with_no_recognised_cause_gets_one_question_not_two() {
        let (dir, q, questions) = store();
        let mut t = task("gate went red", dir.path().join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();

        let first = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(first.asked, [t.id.clone()]);
        assert!(first.resumed.is_empty());

        let open: Vec<_> = questions
            .list()
            .into_iter()
            .filter(|q| q.status.open())
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].run, t.id);
        assert_eq!(open[0].node, NODE);
        assert_eq!(open[0].choices.len(), 3);

        // A second pass with nothing new must not file a second question.
        let second = run_once(&q, &questions, None, Timestamp::now());
        assert!(second.asked.is_empty());
        assert_eq!(
            questions
                .list()
                .into_iter()
                .filter(|q| q.status.open())
                .count(),
            1
        );
    }

    #[test]
    fn a_legacy_hold_with_no_recorded_source_gets_exactly_one_question() {
        let (dir, q, questions) = store();
        let mut t = task("schema 1 record", dir.path().join("repo"));
        t.status = TaskStatus::Held;
        assert!(t.hold_source.is_none(), "the case this test is about");
        q.put(&mut t).unwrap();

        let first = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(first.asked, [t.id.clone()]);

        let second = run_once(&q, &questions, None, Timestamp::now());
        assert!(
            second.asked.is_empty(),
            "the same legacy hold must not be asked about twice"
        );
        assert_eq!(
            questions
                .list()
                .into_iter()
                .filter(|q| q.status.open())
                .count(),
            1
        );
    }

    #[test]
    fn dependency_resolution_detaches_legacy_short_refs_without_dropping_other_waits() {
        let (dir, queue, questions) = store();
        let mut root = task("root", dir.path().join("repo"));
        root.id = "20261013-193611-ca5b".into();
        root.hold_machine(Some("needs owner decision".into()));
        queue.put(&mut root).unwrap();
        let mut other = task("other", dir.path().join("repo"));
        other.id = "20261013-192213-feb2".into();
        queue.put(&mut other).unwrap();
        let mut waiting = task("waiting", dir.path().join("repo"));
        waiting.block(vec!["ca5b".into(), other.id.clone()], None);
        queue.put(&mut waiting).unwrap();
        detach_dependants(&queue, &questions, &root.id);
        let after = queue.get(&waiting.id).unwrap();
        assert_eq!(after.status, TaskStatus::Blocked);
        assert_eq!(after.blocked_by, [other.id]);
    }

    #[test]
    fn a_manual_hold_is_never_auto_resumed() {
        let (dir, q, questions) = store();
        let mut t = task("operator stopped this", dir.path().join("repo"));
        t.hold_manual(Some("waiting on a decision".to_owned()));
        q.put(&mut t).unwrap();

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert!(report.resumed.is_empty());
        // Fresh, not stale yet - no question either.
        assert!(report.asked.is_empty());

        let back = q.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Held);
        assert_eq!(back.hold_source, Some(HoldSource::Manual));
        assert!(questions.list().is_empty());
    }

    #[test]
    fn a_blocked_task_on_a_deleted_dependency_is_held_and_asked_about_in_one_pass() {
        // The five real tasks this whole change exists for are `blocked`, not
        // `held`, and no daemon has to be running for `magi task triage` alone
        // to reach them - `run_once` must both quarantine and ask in the same
        // call.
        let (dir, q, questions) = store();
        let mut still_going = task("still valid", dir.path().join("repo"));
        q.put(&mut still_going).unwrap();

        let mut t = task("orphaned", dir.path().join("repo"));
        t.block(
            vec!["20260101-000000-gone".to_owned(), still_going.id.clone()],
            Some("waits on both".to_owned()),
        );
        q.put(&mut t).unwrap();

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(report.quarantined, [t.id.clone()]);
        assert_eq!(
            report.asked,
            [t.id.clone()],
            "the fresh machine hold must earn a question in the same pass"
        );

        let after = q.get(&t.id).unwrap();
        assert_eq!(after.status, TaskStatus::Held);
        assert_eq!(after.hold_source, Some(HoldSource::Machine));
        assert!(after.blocked_by.is_empty());

        // The reason is not disk-pressure wording, so this must not be read
        // as a disk hold and silently auto-resumed.
        assert!(!is_disk_hold(&after));

        let open: Vec<_> = questions
            .list()
            .into_iter()
            .filter(|q| q.status.open())
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].run, t.id);

        // A second pass with nothing new files no second question.
        let second = run_once(&q, &questions, None, Timestamp::now());
        assert!(second.quarantined.is_empty());
        assert!(second.asked.is_empty());
    }

    /// A held root with a chain of dependants: 9db7 <- 4135 <- 6081, and a
    /// second direct dependant that also waits on a queued task.
    fn stuck_chain(q: &Queue, dir: &std::path::Path) -> (Task, Task, Task) {
        let mut root = task("root", dir.join("repo"));
        root.hold_manual(Some("waiting".to_owned()));
        q.put(&mut root).unwrap();
        let mut mid = task("mid", dir.join("repo"));
        mid.block(vec![root.id.clone()], None);
        q.put(&mut mid).unwrap();
        let mut leaf = task("leaf", dir.join("repo"));
        leaf.block(vec![mid.id.clone()], None);
        q.put(&mut leaf).unwrap();
        (root, mid, leaf)
    }

    fn deps_questions(questions: &Questions) -> Vec<Question> {
        questions
            .list()
            .into_iter()
            .filter(|q| q.node == DEPS_NODE)
            .collect()
    }

    fn answer_deps(questions: &Questions, choice: usize) {
        let mut asked = deps_questions(questions).remove(0);
        let c = asked.choices[choice].clone();
        asked.answer(Answer::Choice(c)).unwrap();
        questions.put(&mut asked).unwrap();
    }

    #[test]
    fn a_stuck_chain_earns_one_question_naming_every_dependant_and_is_not_reasked() {
        let (dir, q, questions) = store();
        let (root, mid, leaf) = stuck_chain(&q, dir.path());

        let first = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(first.asked, std::slice::from_ref(&root.id));
        let asked = deps_questions(&questions);
        assert_eq!(asked.len(), 1, "one question per root, not per dependant");
        assert_eq!(asked[0].run, root.id);
        assert_eq!(asked[0].choices.len(), 3);
        assert!(asked[0].detail.contains(mid.short()), "{}", asked[0].detail);
        assert!(
            asked[0].detail.contains(leaf.short()),
            "{}",
            asked[0].detail
        );

        let second = run_once(&q, &questions, None, Timestamp::now());
        assert!(second.asked.is_empty());
        assert_eq!(deps_questions(&questions).len(), 1);
        assert!(pending_for(&questions, &q.get(&root.id).unwrap()));
    }

    #[test]
    fn a_stuck_root_with_a_machine_hold_gets_only_the_dependency_question() {
        let (dir, q, questions) = store();
        let mut root = task("root", dir.path().join("repo"));
        root.hold_machine(Some("gate red".to_owned()));
        q.put(&mut root).unwrap();
        let mut dep = task("dep", dir.path().join("repo"));
        dep.block(vec![root.id.clone()], None);
        q.put(&mut dep).unwrap();

        run_once(&q, &questions, None, Timestamp::now());
        run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(deps_questions(&questions).len(), 1);
        assert_eq!(
            questions.list().len(),
            1,
            "no second question about the hold"
        );
    }

    #[test]
    fn a_dependency_that_can_still_run_asks_nothing() {
        let (dir, q, questions) = store();
        let mut live = task("live", dir.path().join("repo"));
        q.put(&mut live).unwrap();
        let mut dep = task("dep", dir.path().join("repo"));
        dep.block(vec![live.id.clone()], None);
        q.put(&mut dep).unwrap();
        let report = run_once(&q, &questions, None, Timestamp::now());
        assert!(report.asked.is_empty());
        assert!(questions.list().is_empty());
    }

    #[test]
    fn answering_release_requeues_the_root_and_is_not_reasked() {
        let (dir, q, questions) = store();
        let (root, _mid, _leaf) = stuck_chain(&q, dir.path());
        run_once(&q, &questions, None, Timestamp::now());
        answer_deps(&questions, 0);

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(report.answered, std::slice::from_ref(&root.id));
        assert!(report.asked.is_empty(), "applying must not re-ask");
        assert_eq!(q.get(&root.id).unwrap().status, TaskStatus::Queued);
        let again = run_once(&q, &questions, None, Timestamp::now());
        assert!(again.asked.is_empty() && again.answered.is_empty());
        assert_eq!(deps_questions(&questions).len(), 1);
    }

    #[test]
    fn answering_detach_frees_the_direct_dependant_and_keeps_the_root() {
        let (dir, q, questions) = store();
        let (root, mid, leaf) = stuck_chain(&q, dir.path());
        run_once(&q, &questions, None, Timestamp::now());
        answer_deps(&questions, 2);

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert!(report.asked.is_empty());
        assert_eq!(q.get(&root.id).unwrap().status, TaskStatus::Held);
        assert_eq!(q.get(&mid.id).unwrap().status, TaskStatus::Queued);
        let leaf = q.get(&leaf.id).unwrap();
        assert_eq!(leaf.status, TaskStatus::Blocked, "still waits on mid");
        assert_eq!(leaf.blocked_by, std::slice::from_ref(&mid.id));
    }

    #[test]
    fn a_dependant_that_can_progress_through_another_blocker_is_not_stuck() {
        let (dir, q, questions) = store();
        let (root, mid, _leaf) = stuck_chain(&q, dir.path());
        let mut live = task("live", dir.path().join("repo"));
        q.put(&mut live).unwrap();
        let mut both = q.get(&mid.id).unwrap();
        both.block(vec![root.id.clone(), live.id.clone()], None);
        q.put(&mut both).unwrap();
        // `mid` can still progress through `live`, so nothing is stuck yet.
        assert!(
            run_once(&q, &questions, None, Timestamp::now())
                .asked
                .is_empty()
        );
    }

    #[test]
    fn answering_discard_detaches_then_removes_the_root() {
        let (dir, q, questions) = store();
        let (root, mid, _leaf) = stuck_chain(&q, dir.path());
        run_once(&q, &questions, None, Timestamp::now());
        answer_deps(&questions, 1);

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert!(q.get(&root.id).is_err());
        assert_eq!(q.get(&mid.id).unwrap().status, TaskStatus::Queued);
        assert!(
            report.asked.is_empty(),
            "no per-dependant quarantine question"
        );
        assert!(report.quarantined.is_empty());
    }

    #[test]
    fn an_unmatched_deps_answer_changes_nothing_and_is_not_reasked() {
        let (dir, q, questions) = store();
        let (root, mid, _leaf) = stuck_chain(&q, dir.path());
        run_once(&q, &questions, None, Timestamp::now());
        let mut asked = deps_questions(&questions).remove(0);
        asked.choices.clear();
        asked.answer(Answer::Text("dunno".to_owned())).unwrap();
        questions.put(&mut asked).unwrap();

        for _ in 0..2 {
            let report = run_once(&q, &questions, None, Timestamp::now());
            assert!(report.asked.is_empty() && report.answered.is_empty());
        }
        assert_eq!(deps_questions(&questions).len(), 1);
        assert_eq!(q.get(&root.id).unwrap().status, TaskStatus::Held);
        assert_eq!(q.get(&mid.id).unwrap().status, TaskStatus::Blocked);
    }

    #[test]
    fn a_dependency_cycle_terminates_and_is_asked_about_once() {
        let (dir, q, questions) = store();
        let mut a = task("a", dir.path().join("repo"));
        let mut b = task("b", dir.path().join("repo"));
        a.block(vec![b.id.clone()], None);
        b.block(vec![a.id.clone()], None);
        q.put(&mut a).unwrap();
        q.put(&mut b).unwrap();

        let first = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(first.asked.len(), 1);
        let second = run_once(&q, &questions, None, Timestamp::now());
        assert!(second.asked.is_empty());
        assert_eq!(deps_questions(&questions).len(), 1);
    }

    #[test]
    fn a_missing_dependency_is_still_quarantined_not_asked_about_as_stuck() {
        let (dir, q, questions) = store();
        let mut t = task("orphan", dir.path().join("repo"));
        t.block(vec!["20260101-000000-gone".to_owned()], None);
        q.put(&mut t).unwrap();
        let report = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(report.quarantined, [t.id.clone()]);
        assert!(deps_questions(&questions).is_empty());
    }

    #[test]
    fn a_stale_manual_hold_earns_a_two_choice_question() {
        let (dir, q, questions) = store();
        let mut t = task("been sitting a while", dir.path().join("repo"));
        t.hold_manual(Some("waiting on a decision".to_owned()));
        q.put(&mut t).unwrap();
        // Back-date the hold past MANUAL_STALE_AFTER without waiting a week.
        let mut back = q.get(&t.id).unwrap();
        back.updated_at = Timestamp::now() - SignedDuration::new(8 * 24 * 60 * 60, 0);
        std::fs::write(
            q.path_of(&back.id),
            serde_json::to_string_pretty(&back).unwrap(),
        )
        .unwrap();

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(report.asked, [t.id.clone()]);
        let open: Vec<_> = questions
            .list()
            .into_iter()
            .filter(|q| q.status.open())
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].choices.len(), 2);
    }

    #[test]
    fn answering_resume_releases_the_task() {
        let (dir, q, questions) = store();
        let mut t = task("gate went red", dir.path().join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();
        run_once(&q, &questions, None, Timestamp::now());

        let mut asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        asked.answer(Answer::Choice(EN.resume.to_owned())).unwrap();
        questions.put(&mut asked).unwrap();

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(report.answered, [t.id.clone()]);
        let back = q.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Queued);
        assert!(back.hold_source.is_none());
    }

    /// A task released by a "resume" answer, run, and failed back to `held`.
    fn resumed_then_failed(q: &Queue, questions: &Questions, dir: &std::path::Path) -> Task {
        let mut t = task("gate went red", dir.join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();
        run_once(q, questions, None, Timestamp::now());
        let mut asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        asked.answer(Answer::Choice(EN.resume.to_owned())).unwrap();
        questions.put(&mut asked).unwrap();

        let report = run_once(q, questions, None, Timestamp::now());
        assert_eq!(report.answered, [t.id.clone()]);
        let mut back = q.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Queued);
        assert_eq!(back.attempts, 0);

        back.start("run-1".to_owned());
        back.fail("rebase conflict", 1);
        assert_eq!(back.status, TaskStatus::Held);
        q.put(&mut back).unwrap();
        back
    }

    #[test]
    fn a_resume_answer_is_applied_once_and_a_new_machine_hold_is_asked_about() {
        let (dir, q, questions) = store();
        let t = resumed_then_failed(&q, &questions, dir.path());

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert!(report.answered.is_empty(), "the old answer must not replay");
        assert_eq!(report.asked, std::slice::from_ref(&t.id));
        let back = q.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Held);
        assert_eq!(
            questions
                .list()
                .into_iter()
                .filter(|q| q.status.open())
                .count(),
            1
        );
    }

    #[test]
    fn a_manual_hold_placed_after_a_resume_is_not_undone_by_the_old_answer() {
        let (dir, q, questions) = store();
        let mut t = resumed_then_failed(&q, &questions, dir.path());
        t.hold_manual(Some("operator stopped this".to_owned()));
        q.put(&mut t).unwrap();
        let before = questions.list().len();

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert!(report.answered.is_empty());
        assert!(report.asked.is_empty());
        let back = q.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Held);
        assert_eq!(back.hold_source, Some(HoldSource::Manual));
        assert_eq!(questions.list().len(), before);
    }

    #[test]
    fn answering_not_yet_keeps_it_held_as_a_manual_hold_and_does_not_reapply() {
        let (dir, q, questions) = store();
        let mut t = task("gate went red", dir.path().join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();
        run_once(&q, &questions, None, Timestamp::now());

        let mut asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        asked.answer(Answer::Choice(EN.wait.to_owned())).unwrap();
        questions.put(&mut asked).unwrap();

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(report.answered, [t.id.clone()]);
        let back = q.get(&t.id).unwrap();
        assert_eq!(back.status, TaskStatus::Held);
        assert_eq!(back.hold_source, Some(HoldSource::Manual));
        assert!(
            back.hold_reason
                .as_deref()
                .is_some_and(|r| r.contains("gate red")),
            "the original cause must survive a \"not yet\" answer, not just the \
             triage marker: {:?}",
            back.hold_reason
        );

        // A third pass, same instant: the manual hold is fresh (just
        // touched), so nothing more happens - in particular the already
        // answered question is not re-applied.
        let third = run_once(&q, &questions, None, Timestamp::now());
        assert!(third.answered.is_empty());
        assert!(third.asked.is_empty());
    }

    #[test]
    fn answering_discard_removes_the_task_entirely() {
        let (dir, q, questions) = store();
        let mut t = task("gate went red", dir.path().join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();
        run_once(&q, &questions, None, Timestamp::now());

        let mut asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        asked.answer(Answer::Choice(EN.discard.to_owned())).unwrap();
        questions.put(&mut asked).unwrap();

        let report = run_once(&q, &questions, None, Timestamp::now());
        assert_eq!(report.answered, [t.id.clone()]);
        assert!(
            q.get(&t.id).is_err(),
            "\"discard it\" (捨ててよい) must actually discard the task, not \
             just leave it sitting held forever"
        );
    }

    #[test]
    fn an_answer_is_read_by_its_position_in_choices_not_by_the_callers_current_language() {
        // Filed while the repo's config reads Japanese...
        let (dir, q, questions) = store();
        let ja_config = dir.path().join("ja.toml");
        std::fs::write(&ja_config, "[graph]\nlanguage = \"ja\"\n").unwrap();
        let mut t = task("gate went red", dir.path().join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();
        run_once(&q, &questions, Some(&ja_config), Timestamp::now());

        let mut asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        assert_eq!(asked.choices[0], JA.resume, "filed in Japanese");
        asked.answer(Answer::Choice(JA.resume.to_owned())).unwrap();
        questions.put(&mut asked).unwrap();

        // ...but applied against an English config (a later `--config`, or an
        // edited repository config). The Japanese "再開してよい" answer must
        // still be read as a resume, not silently misread as "keep held"
        // because it fails a text comparison against the English wording.
        let en_config = dir.path().join("en.toml");
        std::fs::write(&en_config, "[graph]\nlanguage = \"en\"\n").unwrap();
        let report = run_once(&q, &questions, Some(&en_config), Timestamp::now());
        assert_eq!(report.answered, [t.id.clone()]);
        let back = q.get(&t.id).unwrap();
        assert_eq!(
            back.status,
            TaskStatus::Queued,
            "a resume answer must resume the task regardless of which \
             language it is read back in"
        );
    }

    #[test]
    fn a_question_falls_back_to_last_error_when_hold_reason_was_never_set() {
        // A record written before `Task::fail` started copying `why` into
        // `hold_reason` too (or one written by a still older build) carries
        // only `last_error`. The question detail must still name a cause
        // rather than reading "(none recorded)".
        let (dir, q, questions) = store();
        let mut t = task("kept failing the gate", dir.path().join("repo"));
        t.start("run-1".to_owned());
        t.fail("gate red three times running", 1);
        assert_eq!(t.status, TaskStatus::Held);
        t.hold_reason = None; // simulate a pre-fix or pre-schema record
        q.put(&mut t).unwrap();

        run_once(&q, &questions, None, Timestamp::now());
        let asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        assert!(
            asked.detail.contains("gate red three times running"),
            "the question must surface `last_error` when there is no \
             `hold_reason` to show instead: {}",
            asked.detail
        );
    }

    #[test]
    fn open_question_for_and_open_task_ids_reflect_only_what_is_still_waiting() {
        let (dir, q, questions) = store();
        let mut t = task("gate went red", dir.path().join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();

        assert!(open_question_for(&questions, &t.id).is_none());
        assert!(!open_task_ids(&questions).contains(&t.id));

        run_once(&q, &questions, None, Timestamp::now());
        assert!(open_question_for(&questions, &t.id).is_some());
        assert!(open_task_ids(&questions).contains(&t.id));

        let mut asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        asked.answer(Answer::Choice(EN.resume.to_owned())).unwrap();
        questions.put(&mut asked).unwrap();

        assert!(
            open_question_for(&questions, &t.id).is_none(),
            "an answered question is no longer open"
        );
        assert!(!open_task_ids(&questions).contains(&t.id));
    }

    #[test]
    fn pending_for_stays_true_between_an_answer_and_the_next_run_once_pass() {
        // `open_question_for` alone goes `None` the instant the operator
        // answers, well before `run_once` - idle-tick only - gets a turn to
        // actually apply that answer (see `already_applied`). `pending_for`
        // exists so a caller polling far more often than `run_once` does -
        // `crate::conduct::apply_one` - does not walk through that gap.
        let (dir, q, questions) = store();
        let mut t = task("gate went red", dir.path().join("repo"));
        t.hold_machine(Some("gate red".to_owned()));
        q.put(&mut t).unwrap();

        assert!(!pending_for(&questions, &t));

        run_once(&q, &questions, None, Timestamp::now());
        let held = q.get(&t.id).unwrap();
        assert!(pending_for(&questions, &held), "still waiting on an answer");

        let mut asked = questions
            .list()
            .into_iter()
            .find(|q| q.run == t.id)
            .unwrap();
        asked.answer(Answer::Choice(EN.wait.to_owned())).unwrap();
        questions.put(&mut asked).unwrap();
        assert!(!asked.status.open());

        // The race window: answered, but `run_once` has not run again yet.
        let still_held = q.get(&t.id).unwrap();
        assert!(
            pending_for(&questions, &still_held),
            "answered but not yet applied is still pending"
        );

        run_once(&q, &questions, None, Timestamp::now());
        let after = q.get(&t.id).unwrap();
        assert!(
            !pending_for(&questions, &after),
            "the answer is applied now, nothing left pending"
        );
    }
}
