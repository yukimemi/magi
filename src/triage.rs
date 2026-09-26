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
//!   cause is gone, the task goes straight back to `queued` - the same effect
//!   as `magi task release`, just automatic. When it cannot tell, a
//!   [`Question`] is filed once and the task stays held until answered.
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
//! and on every `magi task triage`, so an answered question must be applied to
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
use crate::disk;
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
    let l = language.trim();
    if l.eq_ignore_ascii_case("ja")
        || l.eq_ignore_ascii_case("jp")
        || l.eq_ignore_ascii_case("japanese")
        || l.eq_ignore_ascii_case("日本語")
    {
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
    /// the most common `HoldSource::Machine` hold of all - `Task::fail` once
    /// attempts run out - only ever sets `last_error`, never `hold_reason`, so
    /// reading `hold_reason` alone would leave the question blank for exactly
    /// the case requirement 4 exists for.
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
}

impl Report {
    /// Is there nothing to report? Callers use this to skip logging an empty
    /// pass rather than repeating "triaged 0 held task(s)" on every idle tick.
    pub fn is_empty(&self) -> bool {
        self.resumed.is_empty()
            && self.asked.is_empty()
            && self.answered.is_empty()
            && self.quarantined.is_empty()
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

/// Recognise a `HoldSource::Machine` hold caused by the free-space gate
/// (`crate::disk::gate`'s message, or `daemon::disk_gate`'s "could not
/// measure" fallback) from `hold_reason` text alone.
///
/// There is no field recording *why* a machine hold happened - `hold_machine`
/// takes only a reason string - so this is the one signal available, and disk
/// pressure is the one cause this module can safely re-measure without
/// touching git, a run, or an agent CLI. Keep the two prefixes here in sync
/// with `crate::disk::gate`'s formatted string and `daemon::disk_gate`'s own
/// message if either changes; nothing else ties them together.
fn is_disk_hold(task: &Task) -> bool {
    task.hold_reason.as_deref().is_some_and(|r| {
        r.starts_with("not enough free space to start a run:")
            || r.starts_with("could not measure free space on ")
    })
}

/// Has a `HoldSource::Machine` hold's cause resolved? `Some(true)` means yes -
/// safe to requeue. `Some(false)` means the same cause was checked and is
/// still in force. `None` means this hold's cause is not one this module
/// knows how to re-check at all, and a human has to look.
fn machine_cause_resolved(task: &Task, cfg: &Config) -> Option<bool> {
    if !is_disk_hold(task) {
        return None;
    }
    let min = cfg.disk.min_free_bytes;
    if min == 0 {
        // The operator turned the gate off since this hold was placed - its
        // one possible cause is gone by construction, no measurement needed.
        return Some(true);
    }
    let free = disk::free_bytes(&repo_for(task)).ok()?;
    Some(disk::gate(free, min).is_none())
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

/// [`latest_triage_question`] for any of this module's nodes.
fn latest_question(questions: &Questions, node: &str, task_id: &str) -> Option<Question> {
    questions
        .list()
        .into_iter()
        .filter(|q| q.node == node && q.run == task_id)
        // `asked_at` first: ids carry only whole seconds plus a random
        // suffix, so two questions filed in the same second order randomly.
        .max_by(|a, b| a.asked_at.cmp(&b.asked_at).then_with(|| a.id.cmp(&b.id)))
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
    questions.put(&mut q).ok()?;
    Some(q)
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
        let missing = crate::queue::missing_blockers(queue, questions, &task.blocked_by);
        if missing.is_empty() {
            continue;
        }
        task.hold_machine(Some(crate::queue::missing_blocker_hold_reason(
            &task.blocked_by,
            &missing,
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
fn detach_dependants(queue: &Queue, root: &str) {
    for listed in queue.list() {
        if listed.status != TaskStatus::Blocked || !listed.blocked_by.iter().any(|b| b == root) {
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
        t.unblock(root);
        let _ = queue.put(&mut t);
    }
}

/// Apply an answered [`DEPS_NODE`] question. Every step is idempotent, so a
/// pass that dies half way is finished by the next one. Returns whether the
/// answer was consumed.
fn apply_deps_answer(queue: &Queue, questions: &Questions, q: &Question, now: Timestamp) -> bool {
    let Ok(_claim) = queue.claim(&q.run) else {
        return false;
    };
    let Ok(mut root) = queue.get(&q.run) else {
        return false;
    };
    if root.triage_applied(&q.id) {
        return false;
    }
    match interpret_deps_answer(q) {
        DepsAction::Release => {
            if root.status == TaskStatus::Running {
                return false;
            }
            root.release();
            root.resume_override = Some(OperatorResume {
                question_id: q.id.clone(),
                at: now,
                conductor_rehold: None,
                forced: false,
            });
        }
        DepsAction::Discard => {
            detach_dependants(queue, &root.id);
            return queue.remove(&root.id, false, questions).is_ok();
        }
        DepsAction::Detach => detach_dependants(queue, &root.id),
        DepsAction::Nothing => {}
    }
    root.mark_triage_applied(&q.id);
    queue.put(&mut root).is_ok()
}

/// One question per stuck dependency root, and apply the answers to earlier
/// ones. See [`crate::blockers`] for what "stuck" means.
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
    now: Timestamp,
    report: &mut Report,
) {
    for q in questions.list() {
        if q.node == DEPS_NODE
            && q.status == QuestionStatus::Answered
            && apply_deps_answer(queue, questions, &q, now)
        {
            report.answered.push(q.run.clone());
        }
    }

    let all = questions.list();
    let inv = crate::blockers::Inventory::new(queue.list(), &all);
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

    for mut q in all
        .iter()
        .filter(|q| q.node == DEPS_NODE && q.status.open())
        .cloned()
    {
        if !frozen.contains_key(&q.run) {
            q.abandon("nothing waits on this task anymore");
            let _ = questions.put(&mut q);
        }
    }

    for (root_id, dependant_ids) in &frozen {
        let Some(root) = inv.task(root_id) else {
            continue;
        };
        let dependants: Vec<&Task> = dependant_ids.iter().filter_map(|d| inv.task(d)).collect();
        let latest = all
            .iter()
            .filter(|q| q.node == DEPS_NODE && q.run == *root_id)
            .max_by(|a, b| a.asked_at.cmp(&b.asked_at).then_with(|| a.id.cmp(&b.id)));
        match latest {
            Some(q) if q.status.open() => continue,
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
        if pending_for(questions, root) {
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
        if questions.put(&mut question).is_ok() {
            report.asked.push(root.id.clone());
        }
    }
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
/// Also runs [`quarantine_orphaned_blocked`] first, so a `blocked` task whose
/// dependency no longer exists is caught and turned into a fresh `held`
/// question in this same pass, not left for a later call to notice.
pub fn run_once(
    queue: &Queue,
    questions: &Questions,
    config_override: Option<&Path>,
    now: Timestamp,
) -> Report {
    let mut report = Report {
        quarantined: quarantine_orphaned_blocked(queue, questions),
        ..Report::default()
    };
    ask_about_stuck_roots(queue, questions, config_override, now, &mut report);
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
        // A question about what this task freezes is already the one question
        // it owes the operator; a second, about the hold itself, would ask two
        // things at once.
        if deps_pending(questions, &task) {
            continue;
        }

        let cfg = Config::discover(&repo_for(&task), config_override)
            .ok()
            .map(|(c, _)| c);
        let w = wording(cfg.as_ref().map_or("en", |c| c.graph.language.as_str()));

        if let Some(q) = latest_triage_question(questions, &task.id) {
            if q.status.open() {
                // Already asked, still waiting - nothing to do this pass.
                continue;
            }
            if q.status == QuestionStatus::Answered && !already_applied(&task, &q) {
                match interpret_answer(&q) {
                    AnswerAction::Resume => {
                        // A second "resume", to the question about the
                        // conductor's re-hold, forces it: the conductor may
                        // not hold this task again. Any other resume records
                        // the answer so a re-hold can be recognised.
                        let contradicted = task
                            .resume_override
                            .as_ref()
                            .is_some_and(|o| o.conductor_rehold.is_some());
                        let record = match task.resume_override.take() {
                            Some(mut o) if contradicted => {
                                o.forced = true;
                                o
                            }
                            _ => OperatorResume {
                                question_id: q.id.clone(),
                                at: now,
                                conductor_rehold: None,
                                forced: false,
                            },
                        };
                        task.release();
                        task.resume_override = Some(record);
                        task.mark_triage_applied(&q.id);
                        if queue.put(&mut task).is_ok() {
                            report.answered.push(task.id.clone());
                        }
                    }
                    AnswerAction::Discard => {
                        if queue.remove(&task.id, false, questions).is_ok() {
                            report.answered.push(task.id.clone());
                        }
                    }
                    AnswerAction::KeepHeld => {
                        let resolution = q.resolution().unwrap_or_default();
                        let note = keep_held_note(&task, &q, &resolution);
                        task.hold_manual(Some(note));
                        task.mark_triage_applied(&q.id);
                        if queue.put(&mut task).is_ok() {
                            report.answered.push(task.id.clone());
                        }
                    }
                }
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
                    if file_question(questions, &task, Bucket::ConductorOverride, w, now).is_some()
                    {
                        report.asked.push(task.id.clone());
                    }
                } else if cfg.as_ref().and_then(|c| machine_cause_resolved(&task, c)) == Some(true)
                {
                    task.release();
                    if queue.put(&mut task).is_ok() {
                        report.resumed.push(task.id.clone());
                    }
                } else if file_question(questions, &task, Bucket::MachineUnknown, w, now).is_some()
                {
                    report.asked.push(task.id.clone());
                }
            }
            None => {
                if file_question(questions, &task, Bucket::Legacy, w, now).is_some() {
                    report.asked.push(task.id.clone());
                }
            }
            Some(HoldSource::Manual) => {
                if manual_is_stale(&task, now)
                    && file_question(questions, &task, Bucket::ManualStale, w, now).is_some()
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
    let own = match latest_triage_question(questions, &task.id) {
        Some(q) if q.status.open() => true,
        Some(q) if q.status == QuestionStatus::Answered => !already_applied(task, &q),
        _ => false,
    };
    own || deps_pending(questions, task)
}

/// Is the latest [`DEPS_NODE`] question about `task` still open, or answered
/// but not yet applied? Same reasoning as [`pending_for`]: the gap between an
/// answer and the next [`run_once`] pass must not be walked through.
fn deps_pending(questions: &Questions, task: &Task) -> bool {
    match latest_question(questions, DEPS_NODE, &task.id) {
        Some(q) if q.status.open() => true,
        Some(q) if q.status == QuestionStatus::Answered => !task.triage_applied(&q.id),
        _ => false,
    }
}

/// Every task id with an open triage question right now - what `magi task
/// list` uses to mark a held task that is already waiting on an operator
/// decision, rather than have it read identically to one nobody has looked
/// at yet.
pub fn open_task_ids(questions: &Questions) -> std::collections::BTreeSet<String> {
    questions
        .list()
        .into_iter()
        .filter(|q| (q.node == NODE || q.node == DEPS_NODE) && q.status.open())
        .map(|q| q.run)
        .collect()
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

    /// `[disk] min_free_bytes = 0` written next to a fictional repo, so
    /// `machine_cause_resolved` never has to ask the real disk anything - the
    /// same fixture pattern `daemon`'s own idle-loop tests use.
    fn gate_disabled_config(dir: &std::path::Path) -> PathBuf {
        let config = dir.join("magi.toml");
        std::fs::write(&config, "[disk]\nmin_free_bytes = 0\n").unwrap();
        config
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
        // `Task::fail` - the ordinary "out of attempts" machine hold - only
        // ever sets `last_error`, never `hold_reason`. The question detail
        // must still name a cause rather than reading "(none recorded)".
        let (dir, q, questions) = store();
        let mut t = task("kept failing the gate", dir.path().join("repo"));
        t.start("run-1".to_owned());
        t.fail("gate red three times running", 1);
        assert_eq!(t.status, TaskStatus::Held);
        assert!(t.hold_reason.is_none(), "the case this test is about");
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
