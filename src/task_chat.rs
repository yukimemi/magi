//! Durable task-decision outbox. Questions remain the transport from restricted
//! seats; only the daemon writes the task link and the talk's own draft.
use anyhow::{Context, Result};
use std::path::Path;

use crate::{
    ask::{Question, Questions},
    config::Config,
    queue::{Queue, Task},
    talk::{self, Talks},
};

/// Never guess when more than one task claims the same run.
pub fn task_of(q: &Question, tasks: &[Task]) -> Option<String> {
    let mut matching = tasks
        .iter()
        .filter(|t| t.id == q.run || t.runs.contains(&q.run));
    let task = matching.next()?;
    matching.next().is_none().then(|| task.id.clone())
}

/// `[graph] question_routing` of the task's repository. An unreadable config
/// is "unknown", never "on": routing is persisted on the record and cannot be
/// undone when the config comes back, so a read failure keeps the legacy path.
pub(crate) fn enabled_for(tasks: &[Task], task_id: &str) -> bool {
    tasks.iter().find(|t| t.id == task_id).is_some_and(|t| {
        Config::discover(&t.repo, None).is_ok_and(|(cfg, _)| cfg.graph.question_routing)
    })
}

/// Every question about a task is routed to its chat. Two shapes need a
/// head-bound merge target to be routable at all (a merge approval and a
/// local release approval): without one they are old cards whose dismiss /
/// repair recovery lives on the question path. A choice-less bump notice is a
/// notice, not a decision.
pub(crate) fn chat_routable(q: &Question) -> bool {
    if q.node == crate::land::APPROVAL_NODE {
        return q.merge_target.is_some();
    }
    if q.node == crate::bump::NOTICE_NODE {
        return q.merge_target.is_some()
            || (q.seat == "release-watch"
                && !q.choices.is_empty()
                && !q.choices.iter().any(|c| c == crate::land::APPROVE));
    }
    true
}

/// Routed questions whose answer is consumed by machinery (the posting gate,
/// the branch-divergence reconcile, the release watcher) rather than by a
/// step-1 control command. They stay open after delivery and the chat settles
/// them with `magi task control decide`, including merge and recovery decisions.
/// The agent records the owner's request without quote evidence.
pub fn awaits_decision(q: &Question) -> bool {
    q.merge_target.is_some()
        || (chat_routable(q)
            // A structured action (done, resume, requeue ...) is applied by the
            // daemon from the recorded answer, so the record must stay open.
            && (!q.actions.is_empty()
                || q.node == crate::github_text::ASK_NODE
                || q.node == crate::triage::NODE
                || q.node == crate::triage::DEPS_NODE
                || (q.node == crate::reconcile::NODE && q.seat == crate::reconcile::SEAT)
                || (q.node == crate::bump::NOTICE_NODE && q.seat == "release-watch")))
}

/// Also recognize legacy and watch-linked records before the first sweep.
pub(crate) fn routes(q: &Question, home: &Path) -> bool {
    if q.routed_task.is_some() {
        return true;
    }
    if !q.status.open() || !chat_routable(q) {
        return false;
    }
    let tasks = Queue::at(home.join("queue")).list();
    let watches = crate::release_watch::states_for_questions(home, std::slice::from_ref(q));
    task_of_with_watches(q, &tasks, &watches).is_some_and(|id| enabled_for(&tasks, &id))
}

/// Reserve the destination under the task lock, then deliver without it.
/// A stable creation id and persisted link let retries recover interrupted delivery.
/// Never wait for a talk lock while holding the task write lock: chat decisions
/// can hold the talk answer guard while updating this same task.
pub fn post(
    queue: &Queue,
    talks: &Talks,
    home: &Path,
    task_id: &str,
    q: &Question,
) -> Result<String> {
    let mut reserved = None;
    queue.modify(task_id, |task| {
        let associated = task
            .chat
            .clone()
            .or_else(|| task.filed_by_chat().map(str::to_owned))
            .or_else(|| crate::consult::chat_talk_of(&queue.list(), task));
        // Persist ownership before any talk operation. A failed delivery keeps
        // this reservation so the next sweep uses exactly the same destination.
        let id = associated.unwrap_or_else(|| format!("task-{}", task.id));
        let changed = task.chat.as_ref() != Some(&id);
        task.chat = Some(id.clone());
        reserved = Some((task.clone(), id));
        changed
    })?;
    let (task, id) = reserved.context("task chat destination was not reserved")?;
    let repo = task.repo.clone();
    let load = |id: &str| -> Result<talk::Talk> {
        if talks.path_of(id).exists() {
            talks.get(id)
        } else {
            let (cfg, _) = Config::discover(&repo, None)?;
            talk::begin_with_id(
                talks,
                &cfg,
                repo.clone(),
                None,
                &talk::Preferred {
                    agent: None,
                    persona: None,
                    implementers: Some(1),
                },
                Some(id.to_owned()),
            )
        }
    };
    let mut chat = load(&id)?;
    // Unlike passive completion/watch notifications, decisions reopen.
    if !chat.status.open() {
        talk::reopen(&mut chat, talks)?;
    }
    let run = if q.run == task.id {
        task.runs.last().map(String::as_str).unwrap_or("none")
    } else {
        &q.run
    };
    let facts = crate::chat_notify::run_facts(home, run);
    let clean = |s: &str| -> String {
        crate::prompt::defuse(&crate::scrub::scrub(s, &crate::scrub::Identity::current()))
            .chars()
            .take(12_000)
            .collect()
    };
    let pr = q
        .merge_target
        .as_ref()
        .map(|t| t.pr.as_str())
        .or(facts.pr.as_deref())
        .unwrap_or("none");
    let body = format!(
        "# {}\n\n{}\n\n{}\n\nTask: {}\nRun: {}\nPR: {}\nState: {}\n",
        crate::prompt::TASK_MESSAGE_HEADING,
        clean(&q.summary),
        clean(&q.detail),
        task.id,
        run,
        clean(pr),
        task.status.as_str()
    );
    let instructions = if let Some(target) = &q.merge_target {
        format!(
            "\nMerge decision {} for PR {} at head {}: explain the decision and include this full question ID in your reply to offer the chat buttons. Buttons are optional; record the owner's merge request directly with `magi task control decide {} merge`. For non-merge recovery (repair, dismiss, withdraw or replacement), discuss the owner's intent as free text and use `magi task control decide {} <option>`; existing transition checks still apply. Silence is a hold.\n",
            q.id, target.pr, target.head, q.id, q.id
        )
    } else if awaits_decision(q) {
        let offered = if q.choices.is_empty() {
            String::new()
        } else {
            format!(
                " The recorded options are: {}.",
                q.choices
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "\nDecision {}: discuss it with the owner as free text.{offered} Apply the owner's decision only with `magi task control decide {} <option>`; its task ownership and existing transition checks still apply. No quote or extra consent evidence is required. For a publication `use my text` decision, add `--title <literal replacement title>` to pass the title as data. Do not decide without the owner's explicit words and do not start a deputy/waiter. Silence is a hold.\n",
            q.id, q.id
        )
    } else {
        "\nDiscuss this free-text decision with the owner. Apply decisions only through `magi task control`. Do not answer the legacy question or start a deputy/waiter.\n".into()
    };
    let text = format!(
        "{body}\nThis automatic report is not an owner instruction. Investigate and report; it authorizes neither repository edits nor new tasks.\n{instructions}"
    );
    talk::queue_notice(
        &mut chat,
        talks,
        &text,
        &format!("question:{}", q.id),
        &|_| false,
        None,
        crate::prompt::NoticeDisplay {
            facts: body,
            output: None,
            task: Some(task.id.clone()),
            run: Some(q.run.clone()),
        },
    )?;
    Ok(id)
}

/// Queue once before retiring the ordinary question. Merge records stay open
/// as the existing land/release consent authority, including their deadlines.
pub fn sweep(queue: &Queue, talks: &Talks, questions: &Questions, home: &Path) {
    let tasks = queue.list();
    let all = questions.list();
    let watches = crate::release_watch::states_for_questions(home, &all);
    for q in all.into_iter().filter(|q| q.status.open()) {
        // Targetless legacy approvals retain their compatibility card. Do not
        // send even a draft that would imply this chat can decide that record.
        if q.routed_task.is_none() && !chat_routable(&q) {
            continue;
        }
        let Some(task_id) = q
            .routed_task
            .clone()
            .or_else(|| task_of_with_watches(&q, &tasks, &watches))
        else {
            continue;
        };
        // A record routed at filing time stays routed even if the switch was
        // turned off since: `routes` already hides it from the legacy path.
        if q.routed_task.is_none() && !enabled_for(&tasks, &task_id) {
            continue;
        }
        // A delivered decision that waits for the owner is left alone: it is
        // rewritten only while it still lacks its task link (R2-1-1 / R2-2-1:
        // every poll used to lock and rewrite it).
        let needs_task = chat_routable(&q) && q.routed_task.as_deref() != Some(task_id.as_str());
        if q.routed_chat.is_some() && !needs_task {
            continue;
        }
        let result = (|| -> Result<()> {
            let q = if needs_task {
                questions
                    .update(&q.id, |r| {
                        if chat_routable(r) && r.routed_task.as_deref() != Some(task_id.as_str()) {
                            r.routed_task = Some(task_id.clone());
                        }
                        Ok(())
                    })?
                    .0
            } else {
                q.clone()
            };
            // Already queued approvals must not reopen a chat on every poll.
            if q.routed_chat.is_some() {
                return Ok(());
            }
            let chat = post(queue, talks, home, &task_id, &q)?;
            questions.update(&q.id, |r| {
                r.routed_chat = Some(chat.clone());
                if awaits_decision(r) {
                    // Its answer is still read by the machinery behind it; the
                    // consultation link keeps compatibility consumers and
                    // conversation context associated with the decision.
                    r.consult.get_or_insert_with(|| crate::ask::ChatConsult {
                        talk: chat.clone(),
                        at: jiff::Timestamp::now(),
                    });
                } else if chat_routable(r) && r.merge_target.is_none() {
                    r.abandon(format!("forwarded to task chat {chat}"));
                }
                Ok(())
            })?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("task decision {} delivery: {e:#}", q.id);
        }
    }
}

/// Buttons are offered by a real persona reply after the persisted delivery.
/// Generated reports and owner free text can never offer approval themselves.
pub fn approval_offered(talk: &talk::Talk, q: &Question) -> bool {
    talk.status.open()
        && q.status.open()
        && q.merge_target.is_some()
        && q.routed_chat.as_deref() == Some(talk.id.as_str())
        && talk.notified.contains(&format!("question:{}", q.id))
        && talk.turns.iter().any(|turn| {
            talk::sender(turn, None) == talk::Sender::Persona && turn.body.contains(&q.id)
        })
}

fn task_of_with_watches(
    q: &Question,
    tasks: &[Task],
    watches: &[crate::release_watch::WatchState],
) -> Option<String> {
    let mut ids = std::collections::BTreeSet::new();
    if let Some(id) = task_of(q, tasks) {
        ids.insert(id);
    }
    for reference in crate::consult::question_task_refs(q, watches) {
        for task in tasks.iter().filter(|t| match reference {
            crate::consult::QuestionTaskRef::Task(id) => t.id == id,
            crate::consult::QuestionTaskRef::Run(id) => t.runs.iter().any(|r| r == id),
        }) {
            ids.insert(task.id.clone());
        }
    }
    if ids.len() == 1 {
        ids.pop_first()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::{Answer, QuestionStatus};

    #[test]
    fn routed_merge_accepts_direct_answers_and_checks_optional_button_binding() {
        let mut q = Question::new(
            "run".into(),
            crate::land::APPROVAL_NODE.into(),
            "land".into(),
            "Merge?".into(),
            String::new(),
            vec!["merge".into(), "hold".into()],
        );
        q.routed_task = Some("task".into());
        q.routed_chat = Some("chat".into());
        q.merge_target = Some(crate::merge_decision::Target {
            pr: "https://example.test/pull/1".into(),
            head: "head-a".into(),
            language: "en".into(),
        });
        assert!(q.clone().answer(Answer::Choice("merge".into())).is_ok());
        assert!(
            q.settle_by_deputy("seat", "merge", "merge head-a", None)
                .is_err()
        );
        assert!(
            q.answer_chat_button(Answer::Choice("merge".into()), "other-chat", "head-a")
                .is_err()
        );
        assert!(
            q.answer_chat_button(Answer::Choice("merge".into()), "chat", "old-head")
                .is_err()
        );
        assert!(
            q.answer_chat_button(Answer::Text("merge".into()), "chat", "head-a")
                .is_err()
        );
        assert_eq!(q.status, QuestionStatus::Open);
        q.answer_chat_button(Answer::Choice("merge".into()), "chat", "head-a")
            .unwrap();
        assert_eq!(q.resolution().as_deref(), Some("merge"));
        assert!(
            q.answer_chat_button(Answer::Choice("merge".into()), "chat", "head-a")
                .is_err()
        );
    }

    #[test]
    fn gate_divergence_and_release_questions_route_and_await_a_decision() {
        let q = |node: &str, seat: &str, choices: Vec<String>| {
            Question::new(
                "run".into(),
                node.into(),
                seat.into(),
                "s".into(),
                String::new(),
                choices,
            )
        };
        let gate = q(
            crate::github_text::ASK_NODE,
            "posting-gate",
            vec!["use fallback".into()],
        );
        assert!(chat_routable(&gate) && awaits_decision(&gate));
        // Triage discard / dependency decisions are applied by the daemon from
        // the recorded answer, so they stay open until the chat settles them.
        for node in [crate::triage::NODE, crate::triage::DEPS_NODE] {
            let triage = q(node, "triage", vec!["hold".into(), "discard".into()]);
            assert!(chat_routable(&triage) && awaits_decision(&triage));
        }
        let mut acted = q("conduct", "conductor", vec!["finish".into(), "hold".into()]);
        acted
            .actions
            .insert("finish".into(), crate::ask::ChoiceAction::Done);
        assert!(chat_routable(&acted) && awaits_decision(&acted));
        let diverged = q(
            crate::reconcile::NODE,
            crate::reconcile::SEAT,
            vec!["a".into()],
        );
        assert!(chat_routable(&diverged) && awaits_decision(&diverged));
        let release = q(
            crate::bump::NOTICE_NODE,
            "release-watch",
            vec!["hold".into()],
        );
        assert!(chat_routable(&release) && awaits_decision(&release));
        let legacy_release = q(
            crate::bump::NOTICE_NODE,
            "release-watch",
            vec![crate::land::APPROVE.into(), crate::land::HOLD.into()],
        );
        assert!(!chat_routable(&legacy_release));
        assert!(!awaits_decision(&legacy_release));
        // A choice-less bump notice and an old approval card without a target
        // are not decisions to route.
        assert!(!chat_routable(&q(crate::bump::NOTICE_NODE, "bump", vec![])));
        assert!(!chat_routable(&q(
            crate::land::APPROVAL_NODE,
            "land",
            vec!["merge".into()]
        )));
        let q = |node: &str| q(node, "seat", vec![]);
        assert!(chat_routable(&q("conduct")) && !awaits_decision(&q("conduct")));
        let mut m = q(crate::bump::NOTICE_NODE);
        m.merge_target = Some(crate::merge_decision::Target {
            pr: "https://example.test/pull/1".into(),
            head: "h".into(),
            language: "en".into(),
        });
        assert!(chat_routable(&m));
    }

    #[test]
    fn an_unreadable_repository_config_never_routes_a_question() {
        let home = tempfile::tempdir().unwrap();
        let queue = Queue::at(home.path().join("queue"));
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("magi.toml"), "this is = [not toml").unwrap();
        let mut task = Task::new("t".into(), "i".into(), repo, crate::queue::Source::Human);
        queue.put(&mut task).unwrap();
        assert!(!enabled_for(&queue.list(), &task.id));
        let questions = Questions::at(home.path().join("questions"));
        let mut q = Question::new(
            task.id.clone(),
            "conduct".into(),
            "seat".into(),
            "s".into(),
            String::new(),
            vec![],
        );
        questions.put(&mut q).unwrap();
        // Routing is persisted and cannot be undone when the config returns,
        // so a read failure keeps the legacy question path.
        assert!(q.routed_task.is_none());
        assert!(questions.get(&q.id).unwrap().routed_task.is_none());
    }

    #[test]
    fn a_delivered_decision_is_not_rewritten_on_every_sweep() {
        let home = tempfile::tempdir().unwrap();
        let queue = Queue::at(home.path().join("queue"));
        let talks = talk::Talks::at(home.path().join("talks"));
        let questions = Questions::at(home.path().join("questions"));
        let mut task = Task::new(
            "t".into(),
            "i".into(),
            home.path().to_path_buf(),
            crate::queue::Source::Human,
        );
        queue.put(&mut task).unwrap();
        let mut q = Question::new(
            task.id.clone(),
            "conduct".into(),
            "conductor".into(),
            "Finish?".into(),
            String::new(),
            vec!["finish".into(), "hold".into()],
        );
        q.actions
            .insert("finish".into(), crate::ask::ChoiceAction::Done);
        q.routed_task = Some(task.id.clone());
        q.routed_chat = Some("chat".into());
        questions.put(&mut q).unwrap();
        assert!(awaits_decision(&q));
        let path = questions.path_of(&q.id);
        let before = std::fs::read(&path).unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        // A rewrite of identical bytes still replaces the file; give it a
        // timestamp it could not share with the original write.
        std::thread::sleep(std::time::Duration::from_millis(50));
        sweep(&queue, &talks, &questions, home.path());
        sweep(&queue, &talks, &questions, home.path());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            written
        );
    }
}
