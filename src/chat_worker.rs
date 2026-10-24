//! Independent conversation execution, publication fencing and restart recovery.
//!
//! These workers belong to persisted conversations. They retain their executable,
//! turn lease and submission reservation across web and scheduler restarts.
use crate::{
    config::Config,
    talk::{self, Talk, Talks},
};
use anyhow::{Context, Result, bail};

/// Run the talk's turns under `lease` until no draft is owed.
///
/// Other starters that found the lease held queued drafts and left them to
/// us, so drain until nothing is left (as the web's drain loop does). A failed
/// turn is kept and reported at the end, not returned at once: drafts accepted
/// meanwhile are still owed an answer.
pub async fn drain_turns(
    talks: &Talks,
    talk_id: &str,
    mut lease: talk::TurnLease,
    cfg: &Config,
) -> Result<()> {
    let mut talk = talks.get(talk_id)?;
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
    Ok(())
}

/// Try the short cross-process draft-publication fence for one talk.
pub fn try_publication_guard(talks: &Talks, talk_id: &str) -> Result<Option<crate::worker::Lease>> {
    crate::worker::try_lease(talks.home(), &format!("chat-publication:{talk_id}"))
}

/// Serialize queue/record -> worker acknowledgement against a drainer's
/// generation scan + promotion. Never hold this fence across an agent call.
///
/// Lock order deliberately differs: a drainer owns TurnLease then waits here;
/// ingress owns publication but only *tries* TurnLease and queues when busy.
/// Ingress must never wait for TurnLease while holding publication.
pub async fn publication_guard(talks: &Talks, talk_id: &str) -> Result<crate::worker::Lease> {
    loop {
        if let Some(guard) = try_publication_guard(talks, talk_id)? {
            return Ok(guard);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Reserve and launch a detached drainer for an explicitly submitted draft.
///
/// Every submission has its own durable reservation. A shared key would lose
/// arrivals between the previous drainer's last read and its job settlement.
/// The talk's existing turn lease, not the job key, serializes CLI sessions.
pub async fn spawn_turn(talks: &Talks, talk_id: &str) -> Result<crate::worker::Job> {
    spawn_turn_with_source(talks, talk_id, false, None).await
}

/// Continue an operator turn already persisted by the HTTP admission path.
/// Reserve this job before releasing the caller's turn lease.
pub async fn spawn_recorded_turn(
    talks: &Talks,
    talk_id: &str,
    turn: usize,
) -> Result<crate::worker::Job> {
    spawn_turn_with_source(talks, talk_id, false, Some(turn)).await
}

pub(crate) async fn spawn_notice_turn(talks: &Talks, talk_id: &str) -> Result<crate::worker::Job> {
    spawn_turn_with_source(talks, talk_id, true, None).await
}

async fn spawn_turn_with_source(
    talks: &Talks,
    talk_id: &str,
    automatic: bool,
    recorded_turn: Option<usize>,
) -> Result<crate::worker::Job> {
    let talk = talks.get(talk_id)?;
    let recorded = recorded_turn
        .map(|index| {
            talk.turns
                .get(index)
                .context("recorded chat admission disappeared")
        })
        .transpose()?;
    let key = format!(
        "chat:{}:{}",
        talk.id,
        crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4()
    );
    let result = crate::worker::spawn(
        talks.home(),
        "chat",
        &key,
        serde_json::json!({
            "talk": &talk.id, "automatic": automatic, "recorded_turn": recorded_turn,
        }),
    )
    .await;
    match result {
        Err(error) if recorded.is_some() && crate::worker::not_started(&error) => {
            let index = recorded_turn.expect("recorded snapshot");
            match talk::restore_recorded_draft(
                talks, &talk.id, index, recorded.expect("recorded snapshot"),
            ) {
                Ok(true) => Err(error.context(
                    "the agent never started; your send is saved as a draft — use Resume draft",
                )),
                Ok(false) => Err(error.context(
                    "the agent never started, but the transcript changed before draft restoration; the recorded send was retained",
                )),
                Err(restore) => Err(error.context(format!(
                    "the agent never started and its recorded send was retained; draft restoration failed: {restore:#}",
                ))),
            }
        }
        other => other,
    }
}

/// Includes startup reservations, before the child acquires the turn lease.
pub fn turn_worker_active(talks: &Talks, talk_id: &str) -> bool {
    match crate::worker::active(talks.home()) {
        Ok(jobs) => jobs.iter().any(|job| {
            job.kind == "chat"
                && job
                    .payload
                    .get("talk")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|id| id == talk_id)
        }),
        // An unreadable reservation is not evidence that a previous worker
        // stopped. Hold admission until its ownership can be inspected.
        Err(error) => {
            tracing::warn!("cannot inspect chat workers: {error:#}");
            true
        }
    }
}

/// Task-authored transcript posts do not answer a submitted owner turn.
fn recorded_answered(talk: &Talk, index: usize) -> bool {
    index
        .checked_add(1)
        .and_then(|next| talk.turns.get(next..))
        .is_some_and(|turns| {
            turns
                .iter()
                .any(|turn| turn.who == talk::Who::Agent && turn.posted_by.is_none())
        })
}

/// Hidden worker entry point. The original talk/session and cross-process
/// lease stay authoritative even when an older worker is still responding.
pub async fn run_turn_worker(home: &std::path::Path, payload: serde_json::Value) -> Result<()> {
    let id = payload
        .get("talk")
        .and_then(serde_json::Value::as_str)
        .context("chat worker has no talk id")?;
    let automatic = payload
        .get("automatic")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let recorded_turn = payload
        .get("recorded_turn")
        .and_then(serde_json::Value::as_u64)
        .map(usize::try_from)
        .transpose()?;
    let talks = Talks::at(home.join("talks"));
    let current = crate::worker::current_job(home)?;
    loop {
        let talk = talks.get(id)?;
        if !talk.status.open()
            || (recorded_turn.is_none()
                && talk.pending.is_empty()
                && talk.pending_attachments.is_empty())
        {
            return Ok(());
        }
        if automatic && !crate::chat_notify::pending_is_only_notices(&talk) {
            return Ok(());
        }
        if let Some(lease) = talks.claim_turn(id)? {
            let publication = publication_guard(&talks, id).await?;
            // Reload the draft after ownership is acquired: a notice must not
            // turn an unsent operator draft into an automatic submission.
            if automatic && !crate::chat_notify::pending_is_only_notices(&talks.get(id)?) {
                return Ok(());
            }
            let mut fresh = talks.get(id)?;
            let active = crate::worker::active(home)?;
            if recorded_turn.is_none() && current.as_ref().is_some_and(|current| {
                active.iter().any(|job| {
                    job.kind == "chat" && job.id != current.id
                        && job.generation != current.generation
                        && job.payload.get("talk").and_then(serde_json::Value::as_str) == Some(id)
                        && job.payload.get("recorded_turn").and_then(serde_json::Value::as_u64).is_none()
                        && (!job.payload.get("automatic").and_then(serde_json::Value::as_bool).unwrap_or(false)
                            || crate::chat_notify::pending_is_only_notices(&fresh))
                        // Sequence zero belongs to pre-ordering records. In
                        // that case relinquish rather than infer from time.
                        && job.sequence > 0
                        && (current.sequence == 0 || job.sequence > current.sequence)
                })
            }) {
                return Ok(());
            }
            if recorded_turn.is_none()
                && active.iter().any(|job| {
                    if job.kind != "chat"
                        || job.payload.get("talk").and_then(serde_json::Value::as_str) != Some(id)
                    {
                        return false;
                    }
                    let Some(index) = job
                        .payload
                        .get("recorded_turn")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|index| usize::try_from(index).ok())
                    else {
                        return false;
                    };
                    fresh
                        .turns
                        .get(index)
                        .is_some_and(|turn| turn.who == talk::Who::Operator)
                        && !recorded_answered(&fresh, index)
                })
            {
                // A submitted immediate turn is already in the transcript,
                // but its child has not acquired the lease yet. Do not let a
                // queued follow-up overtake it during process startup.
                drop(publication);
                drop(lease);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            let (cfg, _) = Config::discover(&fresh.repo, None)?;
            let text = if let Some(index) = recorded_turn {
                let turn = fresh
                    .turns
                    .get(index)
                    .context("recorded chat turn disappeared")?;
                anyhow::ensure!(
                    turn.who == talk::Who::Operator,
                    "recorded turn is not operator input"
                );
                if recorded_answered(&fresh, index) {
                    return Ok(());
                }
                turn.body.clone()
            } else {
                let Some(text) = talk::drain(&mut fresh, &talks)? else {
                    return Ok(());
                };
                text
            };
            drop(publication);
            // Exactly one invocation per job. A submission accepted by the
            // successor UI already owns a successor-generation reservation;
            // never pull it into this old worker after the current reply.
            return talk::respond(&lease, &mut fresh, &talks, &cfg, &text).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentKind, AgentSpec};
    use std::collections::BTreeMap;

    #[test]
    fn unreadable_worker_ownership_never_looks_idle() {
        let (_home, talks, talk) = talks();
        std::fs::write(talks.home().join("workers"), "not a directory")
            .expect("make reservation inspection fail");
        assert!(crate::worker::active(talks.home()).is_err());
        assert!(turn_worker_active(&talks, &talk.id));
    }

    #[test]
    fn a_task_post_does_not_answer_an_admitted_owner_send() {
        let (_home, talks, mut talk) = talks();
        talk::record(&mut talk, &talks, "owner send", Vec::new()).expect("owner turn");
        let index = talk.turns.len() - 1;
        talk::post(&talks, &talk.id, "unrelated task report", "task-seat").expect("task post");
        let mut fresh = talks.get(&talk.id).expect("persisted transcript");
        assert!(!recorded_answered(&fresh, index));
        let mut response = fresh.turns.last().expect("post").clone();
        response.posted_by = None;
        response.body = "actual persona response".to_owned();
        fresh.turns.push(response);
        assert!(recorded_answered(&fresh, index));
    }

    #[tokio::test]
    async fn publication_waiting_drainer_does_not_block_busy_queue_admission() {
        let (_home, talks, mut talk) = talks();
        let ingress = publication_guard(&talks, &talk.id)
            .await
            .expect("ingress publication");
        let turn = talks
            .claim_turn(&talk.id)
            .expect("turn claim")
            .expect("drainer turn");
        let id = talk.id.clone();
        let waiting = publication_guard(&talks, &id);
        tokio::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(
                waiting.as_mut().poll(cx).is_pending(),
                "worker waits for publication"
            );
            std::task::Poll::Ready(())
        })
        .await;
        assert!(
            talks
                .claim_turn(&talk.id)
                .expect("nonblocking ingress claim")
                .is_none()
        );
        talk::queue(&mut talk, &talks, "accepted while busy", Vec::new())
            .expect("queue without owning turn");
        assert_eq!(
            talks.get(&talk.id).expect("durable pending").pending,
            "accepted while busy"
        );
        drop(ingress);
        let publication = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("publication fence released")
            .expect("drainer publication");
        assert_eq!(
            talk::drain(&mut talk, &talks).expect("drain").as_deref(),
            Some("accepted while busy")
        );
        drop(publication);
        assert!(
            try_publication_guard(&talks, &talk.id)
                .expect("released fence")
                .is_some()
        );
        drop(turn);
    }

    #[tokio::test]
    async fn a_pinning_failure_restores_the_recorded_send_for_manual_resume() {
        let (_home, talks, mut talk) = talks();
        let _publication = publication_guard(&talks, &talk.id)
            .await
            .expect("publication");
        let _turn = talks
            .claim_turn(&talk.id)
            .expect("turn")
            .expect("free turn");
        talk::record(&mut talk, &talks, "keep this accepted send", Vec::new()).expect("record");
        let workers = talks.home().join("workers");
        std::fs::create_dir_all(&workers).expect("worker directory");
        std::fs::write(workers.join("generations"), "block executable publication")
            .expect("controlled pre-reservation failure");
        let error = spawn_recorded_turn(&talks, &talk.id, talk.turns.len() - 1)
            .await
            .expect_err("pinning must fail");
        assert!(crate::worker::not_started(&error));
        assert!(error.to_string().contains("saved as a draft"));
        let mut recovered = talks.get(&talk.id).expect("recovered draft");
        assert!(
            recovered.turns.is_empty(),
            "no unanswered transcript orphan"
        );
        assert_eq!(recovered.pending, "keep this accepted send");
        assert!(
            crate::chat_notify::talks_to_start(&talks).is_empty(),
            "recovery does not become an automatic retry"
        );
        assert_eq!(
            talk::drain(&mut recovered, &talks)
                .expect("manual resume")
                .as_deref(),
            Some("keep this accepted send")
        );
    }

    #[test]
    fn restoring_an_unstarted_send_retains_reply_attachments_and_later_drafts() {
        let (_home, talks, mut talk) = talks();
        talk::post(&talks, &talk.id, "original reply source", "source-task").expect("source");
        let attachment = talk::Attachment {
            id: "image".to_owned(),
            name: "photo.png".to_owned(),
            mime: "image/png".to_owned(),
            bytes: 12,
        };
        talk::record_reply(
            &mut talk,
            &talks,
            "  preserve formatting\n\nowner send",
            vec![attachment.clone()],
            Some(talk::ReplyTarget {
                turn: 0,
                message: 0,
            }),
        )
        .expect("record reply");
        let index = talk.turns.len() - 1;
        let recorded = talk.turns[index].clone();
        talk::post(&talks, &talk.id, "unrelated concurrent post", "later-task").expect("post");
        talk::queue(&mut talk, &talks, "later draft", Vec::new()).expect("later draft");
        assert!(talk::restore_recorded_draft(&talks, &talk.id, index, &recorded).expect("restore"));
        let mut recovered = talks.get(&talk.id).expect("recovered");
        assert_eq!(recovered.turns.len(), 2, "task posts are retained");
        assert_eq!(recovered.pending_messages[0], recorded.messages[0]);
        assert_eq!(recovered.pending_messages[1].body, "later draft");
        assert_eq!(recovered.pending_attachments, vec![attachment]);
        assert_eq!(
            recovered.pending,
            "  preserve formatting\n\nowner send\n\nlater draft"
        );
        assert!(
            !talk::restore_recorded_draft(&talks, &talk.id, index, &recorded).expect("idempotent")
        );
        talk::drain(&mut recovered, &talks)
            .expect("manual resume")
            .expect("draft");
        assert_eq!(
            recovered.turns.last().expect("resumed").messages[0],
            recorded.messages[0]
        );
    }

    #[test]
    fn a_started_response_is_never_rolled_back_into_a_draft() {
        let (_home, talks, mut talk) = talks();
        talk::record(&mut talk, &talks, "owner send", Vec::new()).expect("record");
        let recorded = talk.turns[0].clone();
        let mut response = recorded.clone();
        response.who = talk::Who::Agent;
        response.sender = None;
        response.body = "already answered".to_owned();
        talk.turns.push(response);
        talks.put(&mut talk).expect("response");
        assert!(
            !talk::restore_recorded_draft(&talks, &talk.id, 0, &recorded).expect("refuse rollback")
        );
        let fresh = talks.get(&talk.id).expect("unchanged");
        assert_eq!(fresh.turns.len(), 2);
        assert!(fresh.pending.is_empty());
    }

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
                max_turn_secs: None,
            }],
            ..Config::default()
        };
        let talk = talk::begin(&store, &cfg, tmp.path().to_path_buf(), Some("mock")).expect("talk");
        (tmp, store, talk)
    }
}
