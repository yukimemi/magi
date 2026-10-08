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
/// open. Release notices are left out. Merge approvals may be discussed, but
/// answering from chat requires the owner's latest words.
pub fn origin_talk(tasks: &[Task], talks: &[Talk], q: &Question) -> Option<Talk> {
    if !q.status.open() || q.node == crate::bump::NOTICE_NODE {
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

/// Validate a merge approval answered by a running chat. Terminal answers and
/// other question types retain their existing behavior. Intent is judged by
/// the chat; this gate requires evidence from the correct conversation.
pub fn validate_answer(
    q: &Question,
    talks: &Talks,
    run: &str,
    node: &str,
    reply: &str,
    quote: Option<&str>,
) -> Result<()> {
    if q.node != crate::land::APPROVAL_NODE || node != CHAT_NODE {
        return Ok(());
    }
    let consult = q
        .consult
        .as_ref()
        .context("merge approval was not handed to a chat")?;
    if consult.talk != run {
        bail!("merge approval belongs to a different chat");
    }
    if !q.status.open() {
        bail!("merge approval is no longer open");
    }
    let talk = talks.get(run)?;
    if !talk.status.open() {
        bail!("the consulted chat is closed");
    }
    // Only words the owner wrote after this question was handed over count.
    // The generated hand-over text is stored as an operator turn (possibly
    // coalesced with owner replies), so it is cut out by its own markers.
    let at = talk
        .turns
        .iter()
        .rposition(|t| {
            t.who == talk::Who::Operator
                && t.body.contains(crate::prompt::CHAT_CONSULT_HEADING)
                && t.body.contains(&q.id)
        })
        .context("the question was not delivered to the chat yet")?;
    // A reply still waiting in `pending` is newer than every stored turn.
    let queued = owner_words(&talk.pending, None);
    let stored = talk.turns[at..]
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, t)| t.who == talk::Who::Operator)
        .map(|(i, t)| owner_words(&t.body, (i == 0).then_some(q.id.as_str())))
        .find(|w| !w.is_empty());
    let latest = if queued.is_empty() {
        stored
    } else {
        Some(queued)
    }
    .context("no owner message after the question was handed to the chat")?;
    // Replies sent while a turn runs are joined with a blank line into one
    // turn, so the last paragraph is the only text certain to be the newest.
    let latest = latest
        .rsplit("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or_default()
        .to_owned();
    let quote = quote
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("chat merge approval requires --quote from the owner's latest message")?;
    let valid = match reply {
        crate::land::APPROVE => latest.contains(quote),
        crate::land::HOLD => {
            latest.trim().eq_ignore_ascii_case(crate::land::HOLD)
                && quote.eq_ignore_ascii_case(crate::land::HOLD)
        }
        _ => false,
    };
    if !valid {
        bail!(
            "merge requires a verbatim quote of the latest owner message; hold requires the whole message to be hold"
        );
    }
    Ok(())
}

/// The owner's own words in an operator turn, with magi's generated hand-over
/// text cut out. `drain` stores a queued consult as an operator turn, and
/// `talk::queue` joins an owner reply sent meanwhile onto the same draft, so
/// the turn's origin cannot be told from the turn as a whole. Each generated
/// block runs from its [`CHAT_CONSULT_HEADING`](crate::prompt::CHAT_CONSULT_HEADING)
/// to the last [`CHAT_CONSULT_END`](crate::prompt::CHAT_CONSULT_END) before the
/// next heading (a detail may quote the phrase itself); with no end the block
/// runs to the next heading. Only the blocks are cut: owner replies between
/// them are kept, in order. An owner reply that happens to contain the phrase
/// is cut too, which only ever refuses. With `after_block_of` (the turn
/// carrying the hand-over itself) everything up to the end of the last block
/// naming that question id predates the hand-over and is dropped.
fn owner_words(body: &str, after_block_of: Option<&str>) -> String {
    use crate::prompt::{CHAT_CONSULT_END, CHAT_CONSULT_HEADING};
    let heads: Vec<usize> = body
        .match_indices(CHAT_CONSULT_HEADING)
        .map(|(i, _)| i)
        .collect();
    if heads.is_empty() {
        return body.trim().to_owned();
    }
    // (start, end) of each generated block, the heading's own "# " included.
    let blocks: Vec<(usize, usize)> = heads
        .iter()
        .enumerate()
        .map(|(n, &h)| {
            let limit = heads.get(n + 1).copied().unwrap_or(body.len());
            let end = body[h..limit]
                .rfind(CHAT_CONSULT_END)
                .map_or(limit, |e| h + e + CHAT_CONSULT_END.len());
            let start = if body[..h].ends_with("# ") { h - 2 } else { h };
            (start, end)
        })
        .collect();
    let mut from = 0;
    if let Some(id) = after_block_of {
        if let Some(&(_, end)) = blocks.iter().rfind(|&&(s, e)| body[s..e].contains(id)) {
            from = end;
        } else if let Some(&(_, end)) = blocks.last() {
            from = end;
        }
    }
    let mut parts = Vec::new();
    let mut at = from;
    for &(s, e) in &blocks {
        if s >= at {
            parts.push(body[at..s].trim());
        }
        at = at.max(e);
    }
    parts.push(body[at..].trim());
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
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

    /// A `kind = "command"` agent running `script`; `LEASE` names the turn
    /// lease file so the script can look at it while the turn is in flight.
    fn scripted(tmp: &std::path::Path, script: &str, lease: &std::path::Path) -> Config {
        let path = tmp.join("mock-consult-agent.sh");
        std::fs::write(&path, script).expect("write mock");
        Config {
            agents: vec![AgentSpec {
                id: "mock".to_owned(),
                kind: AgentKind::Command,
                model: None,
                command: vec!["sh".to_owned(), path.to_string_lossy().into_owned()],
                extra_args: Vec::new(),
                env: BTreeMap::from([("LEASE".to_owned(), lease.to_string_lossy().into_owned())]),
                prompt_delivery: None,
            }],
            ..Config::default()
        }
    }

    /// Run `start_turn` with `script` as the agent and hand back its result,
    /// the store and the talk, for the caller to claim the lease afterwards.
    async fn run_turn(
        script: &str,
    ) -> (
        tempfile::TempDir,
        Result<Started>,
        Questions,
        Question,
        Talk,
    ) {
        crate::run::set_home(crate::run::test_home());
        let (tmp, store, talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question("implement");
        questions.put(&mut q).expect("put");
        let cfg = scripted(tmp.path(), script, &store.turn_path(&talk.id));
        let got = start_turn(&questions, &store, &q, &talk, &cfg).await;
        (tmp, got, questions, q, talk)
    }

    #[tokio::test]
    async fn the_lease_is_held_during_the_turn_and_free_once_it_ends() {
        // exit 7 if the lease file is missing while the agent runs.
        let (tmp, got, _questions, _q, talk) =
            run_turn("#!/bin/sh\ncat >/dev/null\n[ -f \"$LEASE\" ] || exit 7\nprintf 'ok\\n'\n")
                .await;
        assert_eq!(got.expect("turn"), Started::Answered);
        let other = Talks::at(tmp.path().join("talks"));
        assert!(
            other.claim_turn(&talk.id).expect("claim").is_some(),
            "free once the turn ended"
        );
    }

    #[tokio::test]
    async fn the_lease_is_free_again_after_a_failed_turn() {
        let (tmp, got, questions, q, talk) = run_turn("#!/bin/sh\ncat >/dev/null\nexit 3\n").await;
        assert!(got.is_err(), "the agent failed");
        assert!(
            questions.get(&q.id).unwrap().consult.is_some(),
            "the turn got as far as running"
        );
        let other = Talks::at(tmp.path().join("talks"));
        assert!(
            other.claim_turn(&talk.id).expect("claim").is_some(),
            "free after the failed turn"
        );
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
        // Merge approvals can be consulted; release notices cannot.
        assert!(
            hit(
                std::slice::from_ref(&chat),
                std::slice::from_ref(&talk),
                &question(crate::land::APPROVAL_NODE)
            )
            .is_some()
        );
        assert!(
            hit(
                std::slice::from_ref(&chat),
                std::slice::from_ref(&talk),
                &question(crate::bump::NOTICE_NODE)
            )
            .is_none()
        );
        // A settled question has nothing left to ask.
        let mut answered = question("implement");
        answered
            .answer(crate::ask::Answer::Choice("Redis".to_owned()))
            .unwrap();
        assert!(hit(&[chat], &[talk], &answered).is_none());
    }

    #[test]
    fn approval_origin_requires_an_open_chat_task_and_question() {
        let (_tmp, _store, talk) = talks();
        let chat = from_chat(&talk);
        let mut q = question(crate::land::APPROVAL_NODE);
        assert!(origin_talk(&[task(Source::Human)], std::slice::from_ref(&talk), &q).is_none());
        assert!(
            origin_talk(
                &[task(Source::Agent {
                    run: talk.id.clone(),
                    node: "implement".into(),
                })],
                std::slice::from_ref(&talk),
                &q
            )
            .is_none()
        );
        assert!(origin_talk(std::slice::from_ref(&chat), &[], &q).is_none());
        let mut closed = talk.clone();
        closed.status = TalkStatus::Closed;
        assert!(origin_talk(std::slice::from_ref(&chat), &[closed], &q).is_none());
        q.abandon("expired");
        assert!(
            origin_talk(std::slice::from_ref(&chat), std::slice::from_ref(&talk), &q).is_none()
        );
        let mut q = question(crate::land::APPROVAL_NODE);
        q.answer(crate::ask::Answer::Choice("Redis".into()))
            .unwrap();
        assert!(origin_talk(&[chat], &[talk], &q).is_none());
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

    fn block(id: &str, detail: &str) -> String {
        format!(
            "# {}\n\nquestion `{id}`\n\n{detail}\n\n{}",
            crate::prompt::CHAT_CONSULT_HEADING,
            crate::prompt::CHAT_CONSULT_END
        )
    }

    #[test]
    fn owner_words_keeps_replies_between_generated_blocks() {
        let body = format!("{}\n\nhold\n\n{}", block("q-bbb", "x"), block("q-ccc", "y"));
        assert_eq!(owner_words(&body, None), "hold");
        let body = format!(
            "{}\n\nmerge it now\n\n{}\n\nhold\n\n{}",
            block("q-aaa", "x"),
            block("q-bbb", "y"),
            block("q-ccc", "z")
        );
        assert_eq!(owner_words(&body, None), "merge it now\n\nhold");
        assert_eq!(owner_words(&body, Some("q-aaa")), "merge it now\n\nhold");
        assert_eq!(owner_words(&body, Some("q-bbb")), "hold");
        assert_eq!(
            owner_words(&format!("early\n\n{}", block("q-aaa", "x")), Some("q-aaa")),
            ""
        );
    }

    #[test]
    fn owner_words_does_not_leak_a_detail_quoting_the_end_phrase() {
        let detail = format!("see: {} --reply merge", crate::prompt::CHAT_CONSULT_END);
        let body = format!("{}\n\nhold", block("q-aaa", &detail));
        assert_eq!(owner_words(&body, Some("q-aaa")), "hold");
        assert_eq!(owner_words(&body, None), "hold");
    }

    #[test]
    fn approval_consult_waits_for_latest_owner_confirmation() {
        let (tmp, store, mut talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question(crate::land::APPROVAL_NODE);
        q.choices = vec![crate::land::APPROVE.into(), crate::land::HOLD.into()];
        questions.put(&mut q).unwrap();
        assert!(begin(&questions, &store, &q, &talk).unwrap());
        assert!(!begin(&questions, &store, &q, &talk).unwrap());
        q = questions.get(&q.id).unwrap();
        assert!(q.status.open());
        assert!(q.answer.is_none());
        assert!(q.thread.is_empty());
        let queued = store.get(&talk.id).unwrap().pending;
        assert!(queued.contains("Never answer it yourself"));
        assert!(queued.contains("Silence holds"));
        assert!(queued.contains("--reply merge --quote"));
        assert!(!queued.contains("answer it yourself with"));

        let owner_turn = |body: &str| talk::Turn {
            who: talk::Who::Operator,
            body: body.into(),
            at: Timestamp::now(),
            attachments: Vec::new(),
            usage: None,
        };
        // An approval the owner gave before the hand-over is not reusable.
        let mut old = talk.clone();
        old.turns
            .insert(0, owner_turn("Please merge PR 12 after review"));
        store.put(&mut old).unwrap();
        talk.turns = old.turns.clone();
        // The drained consult draft is stored as an operator turn; it must not
        // count as the owner's words even though it contains "merge".
        talk.turns.push(owner_turn(&queued));
        store.put(&mut talk).unwrap();
        assert!(validate_answer(&q, &store, &talk.id, CHAT_NODE, "merge", Some("merge")).is_err());
        assert!(
            validate_answer(
                &q,
                &store,
                &talk.id,
                CHAT_NODE,
                "merge",
                Some("Please merge PR 12")
            )
            .is_err()
        );
        // An owner reply coalesced onto the same draft still counts.
        let n = talk.turns.len();
        talk.turns[n - 1].body = format!("{queued}\n\nmerge it now");
        store.put(&mut talk).unwrap();
        assert!(
            validate_answer(
                &q,
                &store,
                &talk.id,
                CHAT_NODE,
                "merge",
                Some("merge it now")
            )
            .is_ok()
        );
        // A later retraction in the same coalesced turn wins.
        talk.turns[n - 1].body = format!("{queued}\n\nmerge it now\n\nhold");
        store.put(&mut talk).unwrap();
        assert!(
            validate_answer(
                &q,
                &store,
                &talk.id,
                CHAT_NODE,
                "merge",
                Some("merge it now")
            )
            .is_err()
        );
        assert!(validate_answer(&q, &store, &talk.id, CHAT_NODE, "hold", Some("hold")).is_ok());
        // So does a reply still waiting in the draft.
        talk.turns[n - 1].body = format!("{queued}\n\nmerge it now");
        talk.pending = "hold".into();
        store.put(&mut talk).unwrap();
        assert!(
            validate_answer(
                &q,
                &store,
                &talk.id,
                CHAT_NODE,
                "merge",
                Some("merge it now")
            )
            .is_err()
        );
        // Another question's hand-over queued after the retraction hides nothing.
        talk.pending = format!("hold\n\n{queued}");
        store.put(&mut talk).unwrap();
        assert!(
            validate_answer(
                &q,
                &store,
                &talk.id,
                CHAT_NODE,
                "merge",
                Some("merge it now")
            )
            .is_err()
        );
        talk.pending.clear();
        talk.turns[n - 1].body = queued.clone();
        store.put(&mut talk).unwrap();
        talk.turns
            .push(owner_turn("Merge this pull request please"));
        store.put(&mut talk).unwrap();
        let check = |q: &Question, run: &str, reply: &str, quote: Option<&str>| {
            validate_answer(q, &store, run, CHAT_NODE, reply, quote)
        };
        assert!(check(&q, "other-talk", "merge", Some("Merge this")).is_err());
        assert!(check(&q, &talk.id, "merge", None).is_err());
        assert!(check(&q, &talk.id, "merge", Some("never said")).is_err());
        assert!(
            check(
                &q,
                &talk.id,
                "merge",
                Some("Merge this pull request please")
            )
            .is_ok()
        );
        assert!(check(&q, &talk.id, "hold", Some("hold")).is_err());
        talk.turns
            .push(owner_turn("Wait, explain the checks first"));
        store.put(&mut talk).unwrap();
        assert!(
            check(
                &q,
                &talk.id,
                "merge",
                Some("Merge this pull request please")
            )
            .is_err()
        );
        talk.turns.push(owner_turn("Please hold"));
        store.put(&mut talk).unwrap();
        assert!(check(&q, &talk.id, "hold", Some("hold")).is_err());
        talk.turns.push(owner_turn("hold"));
        store.put(&mut talk).unwrap();
        assert!(check(&q, &talk.id, "hold", Some("hold")).is_ok());
        talk.turns
            .push(owner_turn("Merge this pull request please"));
        store.put(&mut talk).unwrap();
        q.abandon("approval expired or head changed");
        assert!(
            check(
                &q,
                &talk.id,
                "merge",
                Some("Merge this pull request please")
            )
            .is_err()
        );
        assert!(
            q.answer(crate::ask::Answer::Choice("merge".into()))
                .is_err()
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
