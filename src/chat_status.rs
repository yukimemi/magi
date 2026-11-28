//! List-level conversation status for the Chat list.
//!
//! Display and triage only: nothing here grants authority, resolves a decision,
//! or controls a task. Evidence is gathered once per store revision into an
//! [`Inventory`] keyed by conversation; [`derive`] is a pure function of that
//! evidence plus the conversation's own cheap facts. Transcript prose, titles,
//! notifications, a bare goal and a `Done` task are never evidence of approval
//! or of active execution.
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::{
    ask::{Question, QuestionStatus},
    queue::{Task, TaskStatus},
    task_decision::Status as DecisionStatus,
    task_inquiry::Outbox,
};

/// Coarse state shown on a Chat list row, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An unresolved owner decision or an explicit owner wait / manual hold.
    NeedsOwner,
    /// A failure or a decision effect that needs reconciliation.
    NeedsAttention,
    /// Evidence is unreadable or incomplete; nothing is assumed.
    Unknown,
    /// Verified current turn / task activity.
    Working,
    /// Queued, blocked or otherwise waiting without an owner.
    Waiting,
    /// Closed by hand with nothing outstanding.
    Closed,
    /// Recorded work finished and nothing is outstanding.
    Completed,
    /// Open and just discussing.
    Idle,
}

/// Where a status can be examined further. Rendered as a hash route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    /// `decision`, `task` or `run`.
    pub kind: &'static str,
    /// Full identity of the target.
    pub id: String,
}

/// The wire projection of one row's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatStatus {
    /// Coarse state.
    pub kind: Kind,
    /// Short phrase, empty when the label says it all.
    /// Short phrase, empty when the label says it all.
    pub reason: String,
    /// Distinct unresolved owner decisions (`needs_owner` only).
    /// Number of unresolved owner decisions.
    pub owner_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Where to look further, when there is a single obvious place.
    pub link: Option<Link>,
}

/// Per-conversation goal facts, counted from the saved goals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Goals {
    /// Goals waiting for a fresh owner decision.
    pub owner: usize,
    /// Goals with a turn eligible to run.
    pub runnable: usize,
    /// Goals paused on interrupted or uncertain execution.
    pub paused: usize,
    /// Goals whose end condition was observed.
    pub completed: usize,
    /// Waiting goals: no activity is inferred from them.
    pub waiting: usize,
}

impl Goals {
    /// Count the saved goals by state.
    pub fn of(goals: &[crate::chat_goal::Goal]) -> Self {
        use crate::chat_goal::State;
        let mut out = Self::default();
        for goal in goals {
            match goal.state {
                State::Owner => out.owner += 1,
                State::Runnable => out.runnable += 1,
                State::Paused => out.paused += 1,
                State::Completed => out.completed += 1,
                State::Waiting => out.waiting += 1,
                State::Stopped => {}
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default)]
struct Item {
    reason: String,
    link: Option<Link>,
}

/// Everything durable that points at one conversation.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    owner: Vec<Item>,
    attention: Vec<Item>,
    working: Vec<Item>,
    waiting: Vec<Item>,
    tasks: usize,
    tasks_done: usize,
}

/// Evidence for every conversation, built once per store revision.
#[derive(Debug, Default)]
pub struct Inventory {
    chats: HashMap<String, Evidence>,
    busy_chats: HashSet<String>,
    /// Records that exist but could not be read or attributed to a chat.
    unattributed_unreadable: bool,
}

fn chat_of(task: &Task) -> Option<&str> {
    task.chat
        .as_deref()
        .or_else(|| task.filed_by_chat())
        .or(task.origin_chat.as_deref())
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

impl Inventory {
    /// Chats with an unresolved owner decision, whatever their open state.
    pub fn needs_owner(&self) -> HashSet<&str> {
        self.chats
            .iter()
            .filter(|(_, e)| !e.owner.is_empty())
            .map(|(id, _)| id.as_str())
            .collect()
    }

    fn evidence(&mut self, chat: &str) -> &mut Evidence {
        self.chats.entry(chat.to_owned()).or_default()
    }

    /// Read-only gather. A failure of one store degrades to explicit unknown
    /// rather than to an empty (healthy-looking) answer.
    pub fn collect(home: &Path, tasks: &[Task], questions: &[Question]) -> Self {
        let mut inv = Self::default();
        match crate::worker::observe_active(home) {
            Ok(jobs) => {
                for job in jobs.iter().filter(|job| job.kind == "chat") {
                    if let Some(chat) = job.payload.get("talk").and_then(serde_json::Value::as_str)
                    {
                        inv.busy_chats.insert(chat.to_owned());
                        // A reservation protects admission but is not proof
                        // of a running turn. The row's turn lease supplies that.
                        inv.evidence(chat).waiting.push(Item {
                            reason: "reply queued".into(),
                            link: None,
                        });
                    } else {
                        inv.mark_unreadable();
                    }
                }
            }
            Err(_) => inv.mark_unreadable(),
        }
        let by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let by_question: HashMap<&str, &Question> =
            questions.iter().map(|q| (q.id.as_str(), q)).collect();
        let mut mirrored: HashSet<String> = HashSet::new();

        match Outbox::at(home).list_readable() {
            Ok((items, unreadable)) => {
                if unreadable > 0 {
                    inv.unattributed_unreadable = true;
                }
                for inquiry in items {
                    if let Some(id) = inquiry.key.strip_prefix("question:") {
                        mirrored.insert(id.to_owned());
                    }
                    let Some(decision) = inquiry.decision.as_ref() else {
                        continue;
                    };
                    if inquiry.archived || decision.status == DecisionStatus::Resolved {
                        continue;
                    }
                    let task = by_id.get(inquiry.task.as_str()).copied();
                    // A decision filed under an earlier repository assignment
                    // belongs to history, not to the task's current work.
                    if task.is_some_and(|t| {
                        t.repo != inquiry.repo
                            || inquiry
                                .key
                                .strip_prefix("question:")
                                .and_then(|id| by_question.get(id))
                                .is_some_and(|q| t.stale_question(q))
                            || t.reassignments
                                .last()
                                .is_some_and(|r| inquiry.created_at <= r.at)
                            || inquiry
                                .run
                                .as_ref()
                                .is_some_and(|run| !t.execution_run(run))
                    }) {
                        continue;
                    }
                    let Some(chat) = inquiry
                        .delivered_to
                        .as_deref()
                        .or_else(|| task.and_then(chat_of))
                    else {
                        inv.unattributed_unreadable = true;
                        continue;
                    };
                    let link = Some(Link {
                        kind: "decision",
                        id: inquiry.id.clone(),
                    });
                    let item = |reason: &str| Item {
                        reason: reason.to_owned(),
                        link: link.clone(),
                    };
                    let ev = inv.evidence(chat);
                    match decision.status {
                        DecisionStatus::AwaitingOwner => ev.owner.push(Item {
                            reason: decision.reason.clone(),
                            link,
                        }),
                        DecisionStatus::Queued => ev.waiting.push(item("decision queued")),
                        DecisionStatus::InProgress => {
                            ev.waiting.push(item("decision in progress"));
                        }
                        DecisionStatus::RetryNeeded => {
                            ev.attention.push(item("decision needs reconciliation"));
                        }
                        DecisionStatus::Resolved => {}
                    }
                }
            }
            Err(error) => {
                tracing::warn!("chat status: inquiries unreadable: {error:#}");
                inv.unattributed_unreadable = true;
            }
        }

        // Legacy approvals that are still operative and not mirrored natively.
        for q in questions {
            if q.status != QuestionStatus::Open
                || q.node != crate::land::APPROVAL_NODE
                || mirrored.contains(&q.id)
            {
                continue;
            }
            let Some(task) = tasks
                .iter()
                .find(|t| t.execution_runs().last() == Some(&q.run))
            else {
                continue;
            };
            if task.stale_question(q) {
                continue;
            }
            let Some(chat) = chat_of(task) else {
                continue;
            };
            match crate::run::RunState::load_under(&q.run, home) {
                Ok(run)
                    if task.assignment_accepts(&run)
                        && run.status == crate::run::RunStatus::Landing
                        && !run.released()
                        && run.retired_to.is_none()
                        && !run.merge_decision.as_ref().is_some_and(|d| d.closed)
                        && run
                            .land_approval
                            .as_ref()
                            .is_some_and(|a| a.question == q.id) => {}
                Ok(_) => continue,
                Err(_) => {
                    inv.mark_unreadable();
                    continue;
                }
            }
            let task_id = task.id.clone();
            inv.evidence(chat).owner.push(Item {
                reason: "merge approval".into(),
                link: Some(Link {
                    kind: "task",
                    id: task_id,
                }),
            });
        }

        for task in tasks {
            let Some(chat) = chat_of(task) else { continue };
            let link = Some(Link {
                kind: "task",
                id: task.id.clone(),
            });
            let reason = |what: &str| Item {
                reason: format!("task {} {what}", short(&task.id)),
                link: link.clone(),
            };
            let ev = inv.evidence(chat);
            ev.tasks += 1;
            match task.status {
                TaskStatus::Done => ev.tasks_done += 1,
                TaskStatus::Running => {
                    use crate::run::{Liveness, RunState, RunStatus};
                    match task
                        .execution_runs()
                        .last()
                        .and_then(|id| RunState::load_under(id, home).ok())
                    {
                        Some(run) if task.assignment_accepts(&run) => match run
                            .liveness_under(home, false)
                        {
                            Liveness::Live
                                if run.resource_wait.is_some()
                                    || (run.status == RunStatus::Landing
                                        && run.active.is_empty()) =>
                            {
                                ev.waiting.push(reason("waiting"))
                            }
                            Liveness::Live => ev.working.push(reason("running")),
                            Liveness::Dead => ev.attention.push(reason("execution stopped")),
                            Liveness::Unknown => ev.attention.push(reason("execution unverified")),
                        },
                        _ => ev.attention.push(reason("execution evidence unavailable")),
                    }
                }
                TaskStatus::Queued | TaskStatus::Parked => ev.waiting.push(reason("queued")),
                TaskStatus::Blocked => ev.waiting.push(reason("blocked")),
                TaskStatus::Failed => ev.attention.push(reason("failed")),
                TaskStatus::Held if task.operator_held() => ev.owner.push(reason("on hold")),
                TaskStatus::Held
                    if task.has_disk_capacity_cause()
                        || task
                            .hold_reason
                            .as_deref()
                            .is_some_and(|r| r.starts_with(crate::cache::RESOURCE_HOLD)) =>
                {
                    ev.waiting.push(reason("waiting for resources"))
                }
                TaskStatus::Held => ev.attention.push(reason("held")),
            }
        }
        inv
    }

    /// Mark incomplete source inventories without treating absence as idle.
    pub fn mark_unreadable(&mut self) {
        self.unattributed_unreadable = true;
    }

    /// A chat worker has reserved this conversation.
    pub fn busy(&self, chat: &str) -> bool {
        self.busy_chats.contains(chat)
    }
}

fn pick(items: &[Item], label: &str) -> (String, Option<Link>) {
    let first = items.first();
    let reason = match items.len() {
        0 => String::new(),
        1 => first.map(|i| i.reason.clone()).unwrap_or_default(),
        n => format!("{n} {label}"),
    };
    (reason, first.and_then(|i| i.link.clone()))
}

/// Pure status decision. Precedence: needs owner, attention / unknown,
/// working, waiting, closed, completed, idle.
pub fn derive(
    open: bool,
    ev: Option<&Evidence>,
    goals: Goals,
    thinking: bool,
    unattributed_unreadable: bool,
) -> ChatStatus {
    let empty = Evidence::default();
    let ev = ev.unwrap_or(&empty);
    let status = |kind, reason: String, owner_count, link| ChatStatus {
        kind,
        reason,
        owner_count,
        link,
    };

    let owner = ev.owner.len() + goals.owner;
    if owner > 0 {
        let (reason, link) = if ev.owner.is_empty() {
            ("goal needs your decision".to_owned(), None)
        } else {
            pick(&ev.owner, "decisions")
        };
        let reason = if owner > 1 && !ev.owner.is_empty() {
            format!("{owner} decisions")
        } else {
            reason
        };
        return status(Kind::NeedsOwner, reason, owner, link);
    }
    if !ev.attention.is_empty() || goals.paused > 0 {
        let (reason, link) = if ev.attention.is_empty() {
            ("goal paused".to_owned(), None)
        } else {
            pick(&ev.attention, "need attention")
        };
        return status(Kind::NeedsAttention, reason, 0, link);
    }
    if unattributed_unreadable {
        return status(
            Kind::Unknown,
            "some status records are unreadable".into(),
            0,
            None,
        );
    }
    if thinking || !ev.working.is_empty() {
        let (reason, link) = if ev.working.is_empty() {
            ("replying".to_owned(), None)
        } else {
            pick(&ev.working, "running")
        };
        return status(Kind::Working, reason, 0, link);
    }
    if !ev.waiting.is_empty() || goals.runnable > 0 {
        let (reason, link) = if ev.waiting.is_empty() {
            ("goal turn queued".to_owned(), None)
        } else {
            pick(&ev.waiting, "waiting")
        };
        return status(Kind::Waiting, reason, 0, link);
    }
    if !open {
        return status(Kind::Closed, String::new(), 0, None);
    }
    let done = (ev.tasks > 0 && ev.tasks_done == ev.tasks && goals.waiting == 0)
        || (goals.completed > 0 && goals.waiting == 0 && ev.tasks_done == ev.tasks);
    if done {
        return status(Kind::Completed, String::new(), 0, None);
    }
    status(Kind::Idle, String::new(), 0, None)
}

impl ChatStatus {
    /// Cached rows carry this until the per-request status is filled in.
    pub fn placeholder() -> Self {
        Self {
            kind: Kind::Idle,
            reason: String::new(),
            owner_count: 0,
            link: None,
        }
    }
}

impl Inventory {
    /// Status of one conversation from the shared evidence.
    pub fn status_of(&self, id: &str, open: bool, goals: Goals, thinking: bool) -> ChatStatus {
        derive(
            open,
            self.chats.get(id),
            goals,
            thinking,
            self.unattributed_unreadable,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(reason: &str) -> Item {
        Item {
            reason: reason.into(),
            link: None,
        }
    }

    #[test]
    fn owner_decision_outranks_work_and_closure() {
        let ev = Evidence {
            owner: vec![item("approve")],
            working: vec![item("task running")],
            ..Evidence::default()
        };
        let s = derive(false, Some(&ev), Goals::default(), true, false);
        assert_eq!(s.kind, Kind::NeedsOwner);
        assert_eq!(s.owner_count, 1);
    }

    #[test]
    fn precedence_walks_down_the_ladder() {
        let g = Goals::default();
        let mut ev = Evidence {
            attention: vec![item("failed")],
            working: vec![item("run")],
            waiting: vec![item("queued")],
            ..Evidence::default()
        };
        assert_eq!(
            derive(true, Some(&ev), g, false, false).kind,
            Kind::NeedsAttention
        );
        ev.attention.clear();
        assert_eq!(derive(true, Some(&ev), g, false, false).kind, Kind::Working);
        ev.working.clear();
        assert_eq!(derive(true, Some(&ev), g, false, false).kind, Kind::Waiting);
        ev.waiting.clear();
        assert_eq!(derive(true, Some(&ev), g, true, false).kind, Kind::Working);
        assert_eq!(derive(true, Some(&ev), g, false, false).kind, Kind::Idle);
        assert_eq!(derive(false, Some(&ev), g, false, false).kind, Kind::Closed);
    }

    #[test]
    fn done_task_or_waiting_goal_alone_is_not_activity() {
        let ev = Evidence {
            tasks: 1,
            tasks_done: 1,
            ..Evidence::default()
        };
        assert_eq!(
            derive(true, Some(&ev), Goals::default(), false, false).kind,
            Kind::Completed
        );
        let waiting = Goals {
            waiting: 1,
            ..Goals::default()
        };
        assert_eq!(derive(true, None, waiting, false, false).kind, Kind::Idle);
    }

    #[test]
    fn goal_owner_state_needs_owner_and_unreadable_evidence_is_unknown() {
        let g = Goals {
            owner: 1,
            ..Goals::default()
        };
        assert_eq!(derive(false, None, g, false, false).kind, Kind::NeedsOwner);
        assert_eq!(
            derive(true, None, Goals::default(), false, true).kind,
            Kind::Unknown
        );
        // Unreadable evidence never hides a proven owner decision.
        let ev = Evidence {
            owner: vec![item("a"), item("b")],
            ..Evidence::default()
        };
        let s = derive(true, Some(&ev), Goals::default(), false, true);
        assert_eq!((s.kind, s.owner_count), (Kind::NeedsOwner, 2));
    }

    fn task(chat: &str, repo: &str, status: TaskStatus) -> Task {
        let mut t = Task::new(
            format!("work {chat} {repo}"),
            "text".into(),
            repo.into(),
            crate::queue::Source::Human,
        );
        t.chat = Some(chat.into());
        t.status = status;
        t
    }

    #[test]
    fn collect_counts_distinct_live_decisions_and_skips_stale_or_resolved() {
        let home = tempfile::TempDir::new().unwrap();
        let outbox = Outbox::at(home.path());
        let a = task("chat-a", "/repo", TaskStatus::Running);
        let b = task("chat-a", "/repo", TaskStatus::Done);
        let moved = task("chat-b", "/new", TaskStatus::Queued);
        outbox.file(&a, None, "s".into(), "one", "d").unwrap();
        outbox.file(&b, None, "s".into(), "two", "d").unwrap();
        let resolved = outbox.file(&moved, None, "s".into(), "old", "d").unwrap();
        outbox
            .update_decision(&resolved.id, |d| d.settle("done by hand"))
            .unwrap();
        // Filed under an earlier assignment: the task has since moved repo.
        let mut earlier = moved.clone();
        earlier.repo = "/old".into();
        outbox
            .file(&earlier, None, "s".into(), "stale", "d")
            .unwrap();

        let inv = Inventory::collect(home.path(), &[a, b, moved], &[]);
        let s = inv.status_of("chat-a", false, Goals::default(), false);
        assert_eq!((s.kind, s.owner_count), (Kind::NeedsOwner, 2), "{s:?}");
        assert_eq!(s.link.unwrap().kind, "decision");
        assert!(inv.needs_owner().contains("chat-a"));
        let other = inv.status_of("chat-b", true, Goals::default(), false);
        assert_eq!(other.kind, Kind::Waiting, "{other:?}");
    }

    #[test]
    fn machine_wait_and_unverified_running_are_distinct() {
        let home = tempfile::TempDir::new().unwrap();
        let mut held = task("held", "/repo", TaskStatus::Queued);
        held.hold_machine(Some("[resource] cache busy".into()));
        let running = task("running", "/repo", TaskStatus::Running);
        let inv = Inventory::collect(home.path(), &[held, running], &[]);
        assert_eq!(
            inv.status_of("held", true, Goals::default(), false).kind,
            Kind::Waiting
        );
        assert_eq!(
            inv.status_of("running", true, Goals::default(), false).kind,
            Kind::NeedsAttention
        );
    }

    #[test]
    fn failed_release_is_attention_not_automatic_wait() {
        let home = tempfile::TempDir::new().unwrap();
        let mut t = task("chat", "/repo", TaskStatus::Queued);
        t.hold_machine(Some("[release] command failed".into()));
        let inv = Inventory::collect(home.path(), &[t], &[]);
        assert_eq!(
            inv.status_of("chat", true, Goals::default(), false).kind,
            Kind::NeedsAttention
        );
    }

    #[test]
    fn only_current_landing_approval_counts() {
        let home = tempfile::TempDir::new().unwrap();
        let mut t = task("chat", "/repo", TaskStatus::Parked);
        let mut run = crate::run::RunState::new(
            t.repo.clone(),
            "main".into(),
            "base".into(),
            "task".into(),
            crate::config::Config::default(),
        );
        run.status = crate::run::RunStatus::Landing;
        t.runs.push(run.id.clone());
        let question = |summary: &str| {
            Question::new(
                run.id.clone(),
                crate::land::APPROVAL_NODE.into(),
                "approval".into(),
                summary.into(),
                String::new(),
                vec!["merge".into()],
            )
        };
        let mut old = question("old");
        old.id = "old".into();
        let mut current = question("current");
        current.id = "current".into();
        run.land_approval = Some(crate::run::LandApproval {
            question: current.id.clone(),
            head: "head".into(),
        });
        run.save_under(home.path()).unwrap();
        let inv = Inventory::collect(home.path(), &[t], &[old, current]);
        let status = inv.status_of("chat", true, Goals::default(), false);
        assert_eq!((status.kind, status.owner_count), (Kind::NeedsOwner, 1));
    }

    #[test]
    fn returning_to_same_repo_does_not_revive_old_decision() {
        let home = tempfile::TempDir::new().unwrap();
        let mut t = task("chat", "/repo", TaskStatus::Queued);
        let inquiry = Outbox::at(home.path())
            .file(&t, None, "s".into(), "old", "d")
            .unwrap();
        t.reassignments.push(crate::queue::Reassignment {
            from: "/other".into(),
            to: t.repo.clone(),
            actor: "operator".into(),
            at: inquiry.created_at,
            run_boundary: 0,
        });
        let inv = Inventory::collect(home.path(), &[t], &[]);
        assert_eq!(
            inv.status_of("chat", true, Goals::default(), false).kind,
            Kind::Waiting
        );
    }

    #[test]
    fn unreadable_inquiry_is_explicit_unknown_not_idle() {
        let home = tempfile::TempDir::new().unwrap();
        let root = home.path().join("notifications/task-inquiries");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("broken.json"), "{").unwrap();
        let inv = Inventory::collect(home.path(), &[], &[]);
        assert_eq!(
            inv.status_of("any", true, Goals::default(), false).kind,
            Kind::Unknown
        );
    }
}
