//! Dependency inventory for `blocked` tasks.
//!
//! [`crate::daemon::resolve_blockers`] frees a task when its dependency is
//! `done` (or its question answered) and quarantines it when the dependency no
//! longer exists. It has nothing to say about a dependency that is alive but
//! *stuck*: a `held` task never becomes `done` on its own, so everything
//! waiting on it waits forever, and nothing shows why. [`Inventory`] answers
//! that from one snapshot of the queue and the questions:
//!
//! - [`Inventory::waits_on`] - what a blocked task waits on and each
//!   dependency's state, following a chain of blocked dependencies
//!   (`4135 (blocked → 9db7 held)`), for `magi task list` and the web UI.
//! - [`Inventory::stuck_roots`] - the tasks nothing in the loop will ever run
//!   that a blocked task is frozen behind. [`crate::triage`] asks about each
//!   such root once, naming everything it freezes.
//!
//! A blocked task is *stuck* when it has at least one unresolved dependency
//! and every one of them is either `held` or itself a stuck blocked task.
//! Anything that can still make progress - a queued, running or failed
//! (retried) task, an open question, a dependency that no longer exists (which
//! `resolve_blockers` quarantines) - makes it not stuck. A dependency cycle
//! terminates: its members are stuck, and the smallest id in the cycle is its
//! root.

use std::collections::{BTreeMap, BTreeSet};

use crate::ask::{Question, QuestionStatus};
use crate::queue::{Task, TaskStatus};

/// How many hops [`Inventory::waits_on`] follows before it stops describing.
const CHAIN_DEPTH: usize = 4;

/// One snapshot of the tasks and questions a `blocked_by` id can name.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    tasks: BTreeMap<String, Task>,
    questions: BTreeMap<String, QuestionStatus>,
}

enum Dep<'a> {
    Task(&'a Task),
    Question(QuestionStatus),
    Missing,
}

type Roots = Option<BTreeSet<String>>;

/// The last dash-separated part of an id: what `Task::short` shows.
fn short(id: &str) -> &str {
    id.split('-').next_back().unwrap_or(id)
}

impl Inventory {
    /// Build the snapshot. Callers take `queue.list()` and `questions.list()`
    /// once, so a listing over many tasks never rescans the disk per task.
    pub fn new(tasks: Vec<Task>, questions: &[Question]) -> Self {
        Self {
            tasks: tasks.into_iter().map(|t| (t.id.clone(), t)).collect(),
            questions: questions.iter().map(|q| (q.id.clone(), q.status)).collect(),
        }
    }

    fn dep(&self, id: &str) -> Dep<'_> {
        if let Some(t) = self.tasks.get(id) {
            Dep::Task(t)
        } else if let Some(s) = self.questions.get(id) {
            Dep::Question(*s)
        } else {
            Dep::Missing
        }
    }

    /// A dependency that has nothing left to wait for.
    fn resolved(&self, id: &str) -> bool {
        match self.dep(id) {
            Dep::Task(t) => t.status == TaskStatus::Done,
            Dep::Question(s) => s == QuestionStatus::Answered,
            Dep::Missing => false,
        }
    }

    fn walk(
        &self,
        id: &str,
        path: &mut Vec<String>,
        memo: &mut BTreeMap<String, Roots>,
    ) -> (Roots, bool) {
        if let Some(v) = memo.get(id) {
            return (v.clone(), false);
        }
        let Some(task) = self.tasks.get(id) else {
            return (None, false);
        };
        path.push(id.to_owned());
        let mut roots = BTreeSet::new();
        let mut moving = false;
        let mut cyclic = false;
        for b in &task.blocked_by {
            match self.dep(b) {
                Dep::Task(t) => match t.status {
                    TaskStatus::Done => {}
                    TaskStatus::Held => {
                        roots.insert(b.clone());
                    }
                    TaskStatus::Blocked => {
                        if let Some(pos) = path.iter().position(|p| p == b) {
                            // A cycle: its smallest id speaks for all of it, so
                            // every entry point names the same root.
                            if let Some(min) = path[pos..].iter().min() {
                                roots.insert(min.clone());
                            }
                            cyclic = true;
                        } else {
                            let (sub, c) = self.walk(b, path, memo);
                            cyclic |= c;
                            match sub {
                                Some(r) => roots.extend(r),
                                None => moving = true,
                            }
                        }
                    }
                    TaskStatus::Queued | TaskStatus::Running | TaskStatus::Failed => {
                        moving = true;
                    }
                },
                Dep::Question(QuestionStatus::Answered) => {}
                Dep::Question(_) | Dep::Missing => moving = true,
            }
        }
        path.pop();
        let verdict = (!moving && !roots.is_empty()).then_some(roots);
        if !cyclic {
            memo.insert(id.to_owned(), verdict.clone());
        }
        (verdict, cyclic)
    }

    /// Every stuck `blocked` task, with the root ids it is frozen behind.
    pub fn stuck(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut memo = BTreeMap::new();
        let mut out = BTreeMap::new();
        for (id, t) in &self.tasks {
            if t.status != TaskStatus::Blocked {
                continue;
            }
            let (verdict, _) = self.walk(id, &mut Vec::new(), &mut memo);
            if let Some(roots) = verdict {
                out.insert(id.clone(), roots);
            }
        }
        out
    }

    /// The roots `task` is frozen behind; empty when it is not stuck.
    pub fn stuck_roots(&self, task: &Task) -> BTreeSet<String> {
        if task.status != TaskStatus::Blocked {
            return BTreeSet::new();
        }
        self.walk(&task.id, &mut Vec::new(), &mut BTreeMap::new())
            .0
            .unwrap_or_default()
    }

    /// Each dependency of `task` with its state, e.g. `9db7 (held)` or
    /// `4135 (blocked → 9db7 held)`. Empty unless `task` is `blocked`.
    pub fn waits_on(&self, task: &Task) -> Vec<String> {
        if task.status != TaskStatus::Blocked {
            return Vec::new();
        }
        task.blocked_by
            .iter()
            .map(|b| {
                let chain = self.chain(b, &mut vec![task.id.clone()]);
                format!("{} ({chain})", short(b))
            })
            .collect()
    }

    fn chain(&self, id: &str, seen: &mut Vec<String>) -> String {
        match self.dep(id) {
            Dep::Missing => "missing".to_owned(),
            Dep::Question(s) => format!("question {}", s.as_str()),
            Dep::Task(t) => {
                let mut out = t.status.as_str().to_owned();
                if t.status != TaskStatus::Blocked {
                    return out;
                }
                if seen.iter().any(|s| s == id) {
                    return format!("{out}, cycle");
                }
                if seen.len() > CHAIN_DEPTH {
                    return format!("{out} → …");
                }
                seen.push(id.to_owned());
                if let Some(next) = t.blocked_by.iter().find(|b| !self.resolved(b)) {
                    out.push_str(&format!(" → {} {}", short(next), self.chain(next, seen)));
                }
                out
            }
        }
    }

    /// The task with this id, if it is one.
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Source;
    use std::path::PathBuf;

    fn t(id: &str, status: TaskStatus, blocked_by: &[&str]) -> Task {
        let mut t = Task::new(
            id.to_owned(),
            "x".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        t.id = format!("2026-{id}");
        t.status = status;
        t.blocked_by = blocked_by.iter().map(|b| format!("2026-{b}")).collect();
        t
    }

    fn inv(tasks: Vec<Task>) -> Inventory {
        Inventory::new(tasks, &[])
    }

    #[test]
    fn a_chain_reads_at_a_glance_and_its_root_is_the_held_task() {
        let i = inv(vec![
            t("9db7", TaskStatus::Held, &[]),
            t("4135", TaskStatus::Blocked, &["9db7"]),
            t("6081", TaskStatus::Blocked, &["4135"]),
        ]);
        let six = i.task("2026-6081").unwrap();
        assert_eq!(i.waits_on(six), ["4135 (blocked → 9db7 held)"]);
        let four = i.task("2026-4135").unwrap();
        assert_eq!(i.waits_on(four), ["9db7 (held)"]);
        let stuck = i.stuck();
        assert_eq!(stuck.len(), 2);
        assert!(stuck.values().all(|r| r.iter().eq(["2026-9db7"].iter())));
    }

    #[test]
    fn a_dependency_that_can_still_run_is_not_stuck() {
        let i = inv(vec![
            t("aaaa", TaskStatus::Queued, &[]),
            t("bbbb", TaskStatus::Held, &[]),
            t("cccc", TaskStatus::Blocked, &["aaaa", "bbbb"]),
            t("dddd", TaskStatus::Blocked, &["missing1"]),
            t("eeee", TaskStatus::Blocked, &["cccc"]),
        ]);
        assert!(i.stuck().is_empty(), "{:?}", i.stuck());
    }

    #[test]
    fn a_done_dependency_does_not_hide_a_held_one() {
        let i = inv(vec![
            t("aaaa", TaskStatus::Done, &[]),
            t("bbbb", TaskStatus::Held, &[]),
            t("cccc", TaskStatus::Blocked, &["aaaa", "bbbb"]),
        ]);
        assert_eq!(i.stuck().len(), 1);
    }

    #[test]
    fn a_cycle_terminates_and_names_its_smallest_id_from_every_entry() {
        let i = inv(vec![
            t("bbbb", TaskStatus::Blocked, &["cccc"]),
            t("cccc", TaskStatus::Blocked, &["aaaa"]),
            t("aaaa", TaskStatus::Blocked, &["bbbb"]),
            t("dddd", TaskStatus::Blocked, &["cccc"]),
        ]);
        let stuck = i.stuck();
        assert_eq!(stuck.len(), 4);
        for roots in stuck.values() {
            assert!(roots.iter().eq(["2026-aaaa"].iter()), "{roots:?}");
        }
        let d = i.task("2026-dddd").unwrap();
        assert!(i.waits_on(d)[0].contains("cycle"), "{:?}", i.waits_on(d));
    }

    #[test]
    fn a_self_dependency_is_a_stuck_cycle_of_one() {
        let i = inv(vec![t("aaaa", TaskStatus::Blocked, &["aaaa"])]);
        assert_eq!(i.stuck().len(), 1);
    }

    #[test]
    fn an_open_question_keeps_a_task_alive_and_an_answered_one_resolves() {
        let mut open = Question::new(
            "r".into(),
            "n".into(),
            "s".into(),
            "?".into(),
            String::new(),
            vec![],
        );
        open.id = "2026-qqqq".into();
        let mut answered = open.clone();
        answered.id = "2026-rrrr".into();
        answered.status = QuestionStatus::Answered;
        let tasks = vec![
            t("bbbb", TaskStatus::Held, &[]),
            t("cccc", TaskStatus::Blocked, &["bbbb", "qqqq"]),
            t("dddd", TaskStatus::Blocked, &["bbbb", "rrrr"]),
        ];
        let i = Inventory::new(tasks, &[open, answered]);
        let stuck = i.stuck();
        assert!(!stuck.contains_key("2026-cccc"));
        assert!(stuck.contains_key("2026-dddd"));
        let c = i.task("2026-cccc").unwrap();
        assert_eq!(i.waits_on(c)[1], "qqqq (question open)");
    }
}
