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
//! A triage question's answer is different: "not yet" and "leave it held"
//! (discard) do two entirely different, non-resuming things, and only
//! "resume it" may put the task back in line. Reusing the generic
//! blocked/unblock path would resume every answer alike, so this module
//! never calls [`Task::block`] and never leaves a triaged task anything but
//! `held` while its question is open. [`interpret_answer`] reads
//! [`Question::resolution`] itself and [`run_once`] acts on it directly:
//! [`Task::release`] for an actual "resume it" choice, [`Queue::remove`] for
//! "leave it held" (捨ててよい really means "you may throw this away", not
//! "leave it sitting held"), and [`Task::hold_manual`] for anything else -
//! which both keeps the task held and reclassifies it as a hold an operator
//! has now actually seen, one `crate::conduct` and a later triage pass leave
//! alone.
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
//! # Idempotency, without a new field
//!
//! [`run_once`] runs on every daemon idle tick (see `crate::daemon::poll`)
//! and on every `magi task triage`, so applying the *same* answered question
//! twice has to be harmless - and, once a `HoldSource::Manual`/`None`
//! question has been answered "not yet", finding a *fresh* one for the same
//! task later (once it goes stale again) has to still be possible. Neither
//! [`crate::queue::Task`] nor [`crate::ask::Question`] has a field for "this
//! answer was already applied", so [`already_applied`] reads the same
//! [`Question::short`] id back out of [`Task::hold_reason`] that
//! [`keep_held_note`] appended to it - the same trick [`Question::abandon`]
//! already uses to fold a fact into a text field that has no dedicated one.
//! Appended, not written wholesale: the reason the hold happened in the first
//! place is still worth reading in `magi task show` after an operator says
//! "not yet". A "resume" or "discard" answer needs no marker at all: the task
//! either leaves `held` entirely or stops existing, and either way it is
//! never looked at by this module again.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;

use crate::ask::{Question, QuestionStatus, Questions};
use crate::config::Config;
use crate::disk;
use crate::queue::{HoldSource, Queue, Task, TaskStatus};

/// Node recorded on every question this module files - `crate::conduct::NODE`
/// for the same idea applied to a `crate::conduct` decision instead.
pub const NODE: &str = "triage";

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
    discard: "leave it held",
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
}

impl Report {
    /// Is there nothing to report? Callers use this to skip logging an empty
    /// pass rather than repeating "triaged 0 held task(s)" on every idle tick.
    pub fn is_empty(&self) -> bool {
        self.resumed.is_empty() && self.asked.is_empty() && self.answered.is_empty()
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
fn already_applied(task: &Task, q: &Question) -> bool {
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
    questions
        .list()
        .into_iter()
        .filter(|q| q.node == NODE && q.run == task_id)
        .max_by(|a, b| a.id.cmp(&b.id))
}

/// What an answered triage question's choice means, independent of which
/// language it was filed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnswerAction {
    /// The first choice, always "resume it" / "再開してよい" / "再開する" -
    /// see [`Wording::choices3`] and [`Wording::choices2`], whose first entry
    /// is always the resume choice.
    Resume,
    /// The third choice, present only in [`Wording::choices3`]: "leave it
    /// held" / "捨ててよい".
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
            w.why_machine(),
            w.choices3(),
        ),
        Bucket::Legacy => (w.summary_legacy(task), w.why_legacy(), w.choices3()),
        Bucket::ManualStale => {
            let days = (now.as_second() - task.updated_at.as_second()) / (24 * 60 * 60);
            (
                w.summary_manual_stale(task, days),
                w.why_manual(),
                w.choices2(),
            )
        }
    };
    let mut q = Question::new(
        task.id.clone(),
        NODE.to_owned(),
        SEAT.to_owned(),
        summary,
        w.detail(task, why),
        choices,
    );
    questions.put(&mut q).ok()?;
    Some(q)
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
pub fn run_once(
    queue: &Queue,
    questions: &Questions,
    config_override: Option<&Path>,
    now: Timestamp,
) -> Report {
    let mut report = Report::default();
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
                        task.release();
                        if queue.put(&mut task).is_ok() {
                            report.answered.push(task.id.clone());
                        }
                    }
                    AnswerAction::Discard => {
                        if queue.remove(&task.id, false).is_ok() {
                            report.answered.push(task.id.clone());
                        }
                    }
                    AnswerAction::KeepHeld => {
                        let resolution = q.resolution().unwrap_or_default();
                        let note = keep_held_note(&task, &q, &resolution);
                        task.hold_manual(Some(note));
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
                if cfg.as_ref().and_then(|c| machine_cause_resolved(&task, c)) == Some(true) {
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

/// Every task id with an open triage question right now - what `magi task
/// list` uses to mark a held task that is already waiting on an operator
/// decision, rather than have it read identically to one nobody has looked
/// at yet.
pub fn open_task_ids(questions: &Questions) -> std::collections::BTreeSet<String> {
    questions
        .list()
        .into_iter()
        .filter(|q| q.node == NODE && q.status.open())
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
            "\"leave it held\" (捨ててよい) must actually discard the task, not \
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
}
