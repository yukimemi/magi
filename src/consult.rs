//! Handing an open question to the chat conversation its task came from.
//!
//! A task filed by the standing chat (`Source::Agent` with
//! [`crate::queue::CHAT_NODE`], whose `run` is the conversation's id) can raise
//! a question the owner would rather talk through where the task started. The
//! owner passes it on; the chat agent either answers it with `magi answer` or
//! puts the decision points to the owner in the conversation.
//!
//! Three rules, each with a reason:
//!
//! - **One place decides.** [`origin_talk`] is the only judge of "does this
//!   question have a chat to ask"; the web view and both entry points use it.
//! - **Not a choice.** The hand-over never goes into `Question::choices` and
//!   never touches `Question::answer`: the question stays `Open`, its
//!   validation and the choice actions stay as they were.
//! - **No new seat, no new waiter.** The text is queued as a draft of the
//!   existing talk, so the chat's own turn machinery (its session, its turn
//!   gate) runs it exactly as if the owner had typed it.

use anyhow::{Context, Result, bail};
use jiff::Timestamp;

use crate::ask::{ChatConsult, Question, Questions};
use crate::config::Config;
use crate::queue::{CHAT_NODE, Source, Task};
use crate::talk::{self, Talk, Talks};

/// The conversation `q` may be handed to, if any.
///
/// Requires an open question whose task was filed from a chat that is still
/// open. A merge approval and a release notice are left out: their answer is
/// gated by the deputy's merge-intent rules and must not be settled by a side
/// door.
pub fn origin_talk(tasks: &[Task], talks: &[Talk], q: &Question) -> Option<Talk> {
    if !q.status.open()
        || q.node == crate::land::APPROVAL_NODE
        || q.node == crate::bump::NOTICE_NODE
    {
        return None;
    }
    let task = crate::daemon::task_of_question(tasks, q)?;
    let Source::Agent { run, .. } = &chat_origin(tasks, task)?.source else {
        return None;
    };
    talks
        .iter()
        .find(|t| &t.id == run && t.status.open())
        .cloned()
}

/// Walk the provenance of `start` to the task a chat filed.
///
/// A follow-up goes to the task its merged run served: `FollowUp::origin_task`
/// when recorded (a missing one ends the walk - guessing another task could
/// hand the question to an unrelated chat), else the task whose `runs` hold
/// `FollowUp::run`. At most `MAX_FOLLOWUP_GENERATION + 1` tasks are looked at,
/// each once, so a cycle ends in `None`.
fn chat_origin<'a>(tasks: &'a [Task], start: &'a Task) -> Option<&'a Task> {
    let mut seen = std::collections::HashSet::new();
    let mut cur = start;
    for _ in 0..=crate::followup::MAX_FOLLOWUP_GENERATION {
        if !seen.insert(cur.id.as_str()) {
            return None;
        }
        if matches!(&cur.source, Source::Agent { node, .. } if node == CHAT_NODE) {
            return Some(cur);
        }
        let f = cur.followup.as_ref()?;
        cur = match &f.origin_task {
            Some(id) => tasks.iter().find(|t| &t.id == id)?,
            None => tasks.iter().find(|t| t.runs.contains(&f.run))?,
        };
    }
    None
}

/// Is any question handed to talk `talk_id` still open?
///
/// Decided from the question store, not from the newest turn's wording: the
/// owner's decision usually arrives in a later turn that carries no hand-over
/// heading, and `magi answer` still has to be able to write then. Answered or
/// abandoned questions stop counting, so the turn goes back to read-only.
pub fn pending_consults(questions: &Questions, talk_id: &str) -> bool {
    questions
        .list()
        .iter()
        .any(|q| q.status.open() && q.consult.as_ref().is_some_and(|c| c.talk == talk_id))
}

/// Record the hand-over and queue the question as the chat's next message.
///
/// Returns `false` (and changes nothing) when the question was already handed
/// over. The question is not otherwise touched: still `Open`, no thread turn.
/// When queueing fails the record is withdrawn, so the owner can try again.
pub fn begin(questions: &Questions, talks: &Talks, q: &Question, talk: &Talk) -> Result<bool> {
    let (q, fresh) = questions.update(&q.id, |r| {
        if !r.status.open() {
            bail!("question {} is already {}", r.short(), r.status.as_str());
        }
        if r.consult.is_some() {
            return Ok(false);
        }
        r.consult = Some(ChatConsult {
            talk: talk.id.clone(),
            at: Timestamp::now(),
        });
        Ok(true)
    })?;
    if !fresh {
        return Ok(false);
    }
    let mut talk = talk.clone();
    if let Err(e) = talk::queue(
        &mut talk,
        talks,
        &crate::prompt::chat_consult(&q),
        Vec::new(),
    ) {
        let _ = questions.update(&q.id, |r| {
            r.consult = None;
            Ok(())
        });
        return Err(e).context("queue the question into the chat");
    }
    Ok(true)
}

/// What [`start_turn`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Started {
    /// The question was handed over and the chat's turn ran to its end.
    Answered,
    /// Another process holds the talk's turn lease; nothing was changed.
    Busy,
    /// The question had already been handed to the chat; nothing was changed.
    Nothing,
}

/// [`begin`], then run the chat's turn here, under the talk's own lease.
///
/// The lease is taken *before* anything is written, so a talk somebody else is
/// running leaves no consult record and no draft behind (`Started::Busy`) and
/// the caller can simply try again later. A turn that fails after `begin`
/// leaves the record and the draft, so the chat can resume it.
pub async fn start_turn(
    questions: &Questions,
    talks: &Talks,
    q: &Question,
    talk: &Talk,
    cfg: &Config,
) -> Result<Started> {
    if q.consult.is_some() {
        return Ok(Started::Nothing);
    }
    let Some(mut lease) = talks.claim_turn(&talk.id)? else {
        return Ok(Started::Busy);
    };
    if !begin(questions, talks, q, talk)? {
        return Ok(Started::Nothing);
    }
    let mut talk = talks.get(&talk.id)?;
    // Other starters that found the lease held queued drafts and left them to
    // us, so drain until nothing is left (as the web's drain loop does).
    // A failed turn is kept and reported at the end, not returned at once:
    // drafts accepted meanwhile are still owed an answer.
    let mut failed = None;
    loop {
        while let Some(text) = talk::drain(&mut talk, talks)? {
            if let Err(e) = talk::respond(&lease, &mut talk, talks, cfg, &text).await {
                failed.get_or_insert(e);
            }
            if !lease.beat()? {
                bail!("the turn lease for chat {} was lost", talk.short());
            }
        }
        // A draft queued after the last drain, before the lease is gone, was
        // left to us by a starter that found the lease held. Release first,
        // then look again: whoever queues later either sees no lease (and
        // starts its own turn) or is seen by this re-check.
        drop(lease);
        talk = talks.get(&talk.id)?;
        let owed = talk.status.open()
            && (!talk.pending.is_empty() || !talk.pending_attachments.is_empty());
        if !owed {
            break;
        }
        // Somebody else took the lease meanwhile: they drain it.
        let Some(again) = talks.claim_turn(&talk.id)? else {
            break;
        };
        lease = again;
    }
    if let Some(e) = failed {
        return Err(e);
    }
    Ok(Started::Answered)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::config::{AgentKind, AgentSpec, Config};
    use crate::queue::Source;
    use crate::talk::TalkStatus;

    const RUN: &str = "20260902-000000-beef";

    fn talks() -> (tempfile::TempDir, Talks, Talk) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Talks::at(tmp.path().join("talks"));
        let cfg = Config {
            agents: vec![AgentSpec {
                id: "mock".to_owned(),
                kind: AgentKind::Command,
                model: None,
                command: vec!["true".to_owned()],
                extra_args: Vec::new(),
                env: BTreeMap::new(),
                prompt_delivery: None,
            }],
            ..Config::default()
        };
        let talk = talk::begin(&store, &cfg, tmp.path().to_path_buf(), Some("mock")).expect("talk");
        (tmp, store, talk)
    }

    fn task(source: Source) -> Task {
        let mut t = Task::new(
            "t".to_owned(),
            "Do it".to_owned(),
            PathBuf::from("/repo"),
            source,
        );
        t.start(RUN.to_owned());
        t
    }

    fn from_chat(talk: &Talk) -> Task {
        task(Source::Agent {
            run: talk.id.clone(),
            node: CHAT_NODE.to_owned(),
        })
    }

    fn question(node: &str) -> Question {
        Question::new(
            RUN.to_owned(),
            node.to_owned(),
            "impl-A".to_owned(),
            "Which backend?".to_owned(),
            "SQLite is simpler.".to_owned(),
            vec!["SQLite".to_owned(), "Redis".to_owned()],
        )
    }

    #[tokio::test]
    async fn start_turn_is_busy_and_writes_nothing_while_the_lease_is_held() {
        let (tmp, store, talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question("implement");
        questions.put(&mut q).expect("put");
        let held = store.claim_turn(&talk.id).expect("claim").expect("first");

        let got = start_turn(&questions, &store, &q, &talk, &Config::default())
            .await
            .expect("start");
        assert_eq!(got, Started::Busy);
        assert!(questions.get(&q.id).unwrap().consult.is_none());
        assert!(store.get(&talk.id).unwrap().pending.is_empty());
        drop(held);
    }

    #[test]
    fn origin_talk_is_decided_in_one_table() {
        let (_tmp, _store, talk) = talks();
        let mut closed = talk.clone();
        closed.status = TalkStatus::Closed;
        let chat = from_chat(&talk);
        let q = question("implement");

        let hit = |tasks: &[Task], talks: &[Talk], q: &Question| origin_talk(tasks, talks, q);
        assert_eq!(
            hit(std::slice::from_ref(&chat), std::slice::from_ref(&talk), &q).map(|t| t.id),
            Some(talk.id.clone())
        );
        // Not filed from a chat.
        assert!(hit(&[task(Source::Human)], std::slice::from_ref(&talk), &q).is_none());
        let other = task(Source::Agent {
            run: talk.id.clone(),
            node: "implement".to_owned(),
        });
        assert!(hit(&[other], std::slice::from_ref(&talk), &q).is_none());
        // The conversation is gone, or no longer takes turns.
        assert!(hit(std::slice::from_ref(&chat), &[], &q).is_none());
        assert!(hit(std::slice::from_ref(&chat), &[closed], &q).is_none());
        // No task owns the question.
        assert!(hit(&[], std::slice::from_ref(&talk), &q).is_none());
        // Approvals and release notices are never handed over.
        for node in [crate::land::APPROVAL_NODE, crate::bump::NOTICE_NODE] {
            assert!(
                hit(
                    std::slice::from_ref(&chat),
                    std::slice::from_ref(&talk),
                    &question(node)
                )
                .is_none()
            );
        }
        // A settled question has nothing left to ask.
        let mut answered = question("implement");
        answered
            .answer(crate::ask::Answer::Choice("Redis".to_owned()))
            .unwrap();
        assert!(hit(&[chat], &[talk], &answered).is_none());
    }

    #[test]
    fn a_conductor_question_is_found_through_the_task_id() {
        let (_tmp, _store, talk) = talks();
        let chat = from_chat(&talk);
        let mut q = question(crate::conduct::NODE);
        q.run = chat.id.clone();
        assert!(origin_talk(&[chat], &[talk], &q).is_some());
    }

    fn followup_of(parent: &Task, n: u32) -> Task {
        let mut t = Task::new(
            "f".to_owned(),
            "Fix".to_owned(),
            PathBuf::from("/repo"),
            Source::Agent {
                run: format!("merged-{n}"),
                node: "followup".to_owned(),
            },
        );
        t.start(format!("run-f{n}"));
        t.followup = Some(crate::queue::FollowUp {
            run: format!("merged-{n}"),
            origin_task: Some(parent.id.clone()),
            pr: "https://example.invalid/pr/1".to_owned(),
            findings: Vec::new(),
            generation: n,
        });
        t
    }

    fn q_for(t: &Task) -> Question {
        let mut q = question("implement");
        q.run = t.runs[0].clone();
        q
    }

    #[test]
    fn a_followup_traces_back_to_the_chat() {
        let (_tmp, _store, talk) = talks();
        let mut chat = from_chat(&talk);
        chat.runs = vec!["chat-run".to_owned()];
        let f1 = followup_of(&chat, 1);
        let f2 = followup_of(&f1, 2);
        let ts = [chat, f1.clone(), f2.clone()];
        let id = Some(talk.id.clone());
        let tk = std::slice::from_ref(&talk);
        assert_eq!(origin_talk(&ts, tk, &q_for(&f1)).map(|t| t.id), id);
        assert_eq!(origin_talk(&ts, tk, &q_for(&f2)).map(|t| t.id), id);
    }

    #[test]
    fn a_followup_without_origin_task_is_found_through_runs() {
        let (_tmp, _store, talk) = talks();
        let mut chat = from_chat(&talk);
        chat.runs = vec!["merged-1".to_owned()];
        let mut f1 = followup_of(&chat, 1);
        f1.followup.as_mut().unwrap().origin_task = None;
        let q = q_for(&f1);
        assert!(origin_talk(&[chat, f1], &[talk], &q).is_some());
    }

    #[test]
    fn a_dangling_origin_task_has_no_chat() {
        let (_tmp, _store, talk) = talks();
        let mut chat = from_chat(&talk);
        chat.runs = vec!["chat-run".to_owned()];
        let mut f1 = followup_of(&chat, 1);
        f1.followup.as_mut().unwrap().origin_task = Some("gone".to_owned());
        let q = q_for(&f1);
        assert!(origin_talk(&[chat, f1], &[talk], &q).is_none());
    }

    #[test]
    fn a_followup_cycle_ends_without_a_chat() {
        let (_tmp, _store, talk) = talks();
        let mut a = followup_of(&from_chat(&talk), 1);
        let mut b = followup_of(&a, 2);
        a.followup.as_mut().unwrap().origin_task = Some(b.id.clone());
        b.followup.as_mut().unwrap().origin_task = Some(a.id.clone());
        let q = q_for(&a);
        assert!(origin_talk(&[a, b], &[talk], &q).is_none());
    }

    #[test]
    fn pending_consults_follow_the_store_not_the_turn_text() {
        let (tmp, store, talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        assert!(!pending_consults(&questions, &talk.id), "empty store");

        let mut plain = question("implement");
        questions.put(&mut plain).unwrap();
        assert!(!pending_consults(&questions, &talk.id), "no consult record");

        let mut q = question("implement");
        questions.put(&mut q).unwrap();
        begin(&questions, &store, &q, &talk).unwrap();
        assert!(pending_consults(&questions, &talk.id));
        assert!(!pending_consults(&questions, "other-talk"));

        questions
            .update(&q.id, |q| {
                q.abandon("test");
                Ok(())
            })
            .unwrap();
        assert!(!pending_consults(&questions, &talk.id), "closed question");
    }

    #[test]
    fn begin_queues_once_and_leaves_the_question_open() {
        let (tmp, store, talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question("implement");
        questions.put(&mut q).unwrap();

        assert!(begin(&questions, &store, &q, &talk).unwrap());
        assert!(!begin(&questions, &store, &q, &talk).unwrap(), "idempotent");

        let after = questions.get(&q.id).unwrap();
        assert!(after.status.open());
        assert!(after.thread.is_empty());
        assert_eq!(after.choices, q.choices, "choices are not touched");
        assert_eq!(
            after.consult.as_ref().map(|c| c.talk.as_str()),
            Some(talk.id.as_str())
        );

        let queued = store.get(&talk.id).unwrap().pending;
        assert_eq!(
            queued.matches(&q.id).count(),
            2 + 1,
            "id once per use: {queued}"
        );
        assert!(queued.contains("Which backend?"));
        assert!(queued.contains("SQLite is simpler."));
        assert!(queued.contains("- Redis"));
        // Queued once, so the first-operator-turn title is not involved.
        assert_eq!(
            queued.matches(crate::prompt::CHAT_CONSULT_HEADING).count(),
            1
        );
    }

    #[test]
    fn begin_withdraws_its_record_when_the_talk_cannot_take_it() {
        let (tmp, store, talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question("implement");
        questions.put(&mut q).unwrap();
        std::fs::remove_dir_all(tmp.path().join("talks")).unwrap();
        assert!(begin(&questions, &store, &q, &talk).is_err());
        assert!(questions.get(&q.id).unwrap().consult.is_none());
    }

    #[test]
    fn an_older_question_file_reads_without_a_consult() {
        let mut q = question("implement");
        q.schema = 5;
        let mut v = serde_json::to_value(&q).unwrap();
        v.as_object_mut().unwrap().remove("consult");
        let back: Question = serde_json::from_value(v).unwrap();
        assert!(back.consult.is_none());
    }
}
