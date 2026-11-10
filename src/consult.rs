//! Handing an open question to the chat conversation its task came from.
//!
//! A task filed by the standing chat (`Source::Agent` with
//! [`crate::queue::CHAT_NODE`], whose `run` is the conversation's id) can raise
//! a question the owner would rather talk through where the task started. The
//! owner passes it on; the chat agent carries out the owner's decision with
//! `magi answer`, or puts the decision points to the owner in the conversation.
//!
//! Three rules, each with a reason:
//!
//! - **One place decides.** [`origin_talk`] is the only judge of "does this
//!   question have a chat to ask"; the web view and both entry points use it.
//!   [`associated_talk`] resolves typed ownership links for navigation,
//!   independently of consultation eligibility.
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
use crate::queue::Task;
use crate::talk::{self, Talk, Talks};

/// The conversation `q` may be handed to, if any.
///
/// Requires an open question whose task was filed from a chat. The chat may
/// be closed: [`begin`] reopens it. Release notices are left out.
pub fn origin_talk(tasks: &[Task], talks: &[Talk], q: &Question) -> Option<Talk> {
    if !q.status.open() || q.node == crate::bump::NOTICE_NODE {
        return None;
    }
    let task = crate::daemon::task_of_question(tasks, q)?;
    let chat = chat_talk_of(tasks, task)?;
    talks.iter().find(|talk| talk.id == chat).cloned()
}

/// The existing conversation associated with a question, regardless of whether
/// the question is still eligible for consultation.
pub fn associated_talk(tasks: &[Task], talks: &[Talk], q: &Question) -> Option<Talk> {
    associated_talk_with_watches(tasks, talks, q, &[])
}

/// A typed ownership link, shared by association and the web's selective loader.
#[derive(Clone, Copy)]
pub(crate) enum QuestionTaskRef<'a> {
    Task(&'a str),
    Run(&'a str),
}

pub(crate) fn question_task_refs<'a>(
    q: &'a Question,
    watches: &'a [crate::release_watch::WatchState],
) -> Vec<QuestionTaskRef<'a>> {
    // Review-start reconciliation is filed under a task id, unlike normal
    // review questions; both the node and seat identify that producer.
    let names_task = q.run_names_task()
        || (q.node == crate::reconcile::NODE && q.seat == crate::reconcile::SEAT);
    let mut refs = vec![if names_task {
        QuestionTaskRef::Task(&q.run)
    } else {
        QuestionTaskRef::Run(&q.run)
    }];
    let mut matches = watches
        .iter()
        .filter(|w| w.question.as_deref() == Some(&q.id));
    // Duplicate records are ambiguous even if they currently name one owner.
    if let Some(watch) = matches.next()
        && matches.next().is_none()
    {
        refs.extend(watch.run.as_deref().map(QuestionTaskRef::Run));
        refs.extend(watch.held_task.as_deref().map(QuestionTaskRef::Task));
    }
    refs
}

/// Explicit links may fill a missing owner, but conflicting owners fail closed.
/// This navigation-only resolution never changes consultation eligibility.
pub(crate) fn associated_talk_with_watches(
    tasks: &[Task],
    talks: &[Talk],
    q: &Question,
    watches: &[crate::release_watch::WatchState],
) -> Option<Talk> {
    let mut owner: Option<&Task> = None;
    for reference in question_task_refs(q, watches) {
        for task in tasks.iter().filter(|task| match reference {
            QuestionTaskRef::Task(id) => task.id == id,
            QuestionTaskRef::Run(id) => task.runs.iter().any(|run| run == id),
        }) {
            if owner.is_some_and(|previous| previous.id != task.id) {
                return None;
            }
            owner = Some(task);
        }
    }
    let chat = chat_talk_of(tasks, owner?)?;
    talks.iter().find(|talk| talk.id == chat).cloned()
}

/// The talk `start` descends from: the id recorded on a task
/// ([`Task::chat_talk`]) wins, so deleted ancestors do not matter; tasks
/// written before it existed are resolved by walking the ancestry.
///
/// Walk the provenance of `start` to the task a chat filed.
///
/// A follow-up goes to the task its merged run served: `FollowUp::origin_task`
/// when recorded (a missing one ends the walk - guessing another task could
/// hand the question to an unrelated chat), else the task whose `runs` hold
/// `FollowUp::run`. At most `MAX_FOLLOWUP_GENERATION + 1` tasks are looked at,
/// each once, so a cycle ends in `None`.
pub fn chat_talk_of(tasks: &[Task], start: &Task) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let mut cur = start;
    for _ in 0..=crate::followup::MAX_FOLLOWUP_GENERATION {
        if !seen.insert(cur.id.as_str()) {
            return None;
        }
        if let Some(id) = cur.chat_talk() {
            return Some(id.to_owned());
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
/// heading. Answered or abandoned questions and ended consultations stop
/// counting.
pub fn pending_consults(questions: &Questions, talk_id: &str) -> bool {
    questions.list().iter().any(|q| active_consult(q, talk_id))
}

/// Consultation activity is separate from the question's waiting state.
pub fn active_consult(q: &Question, talk_id: &str) -> bool {
    q.routed_task.is_none()
        && q.status.open()
        && q.consult_outcome.is_none()
        && q.consult.as_ref().is_some_and(|c| c.talk == talk_id)
}

/// A non-answer never wakes a waiter or executes a choice action.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsultOutcome {
    /// The owner's outcome, with no execution effect.
    pub text: String,
    /// Originating conversation.
    pub talk: String,
    /// Time consultation ended.
    pub at: Timestamp,
}

/// Store the owner's alternative as a note and end only the consultation:
/// the question stays open and its waiter stays held.
pub fn record_other(q: &mut Question, text: &str) -> Result<()> {
    if text.trim().is_empty() || q.free_text() || q.choices.iter().any(|c| c == text) {
        bail!("--reply-other requires a nonempty outcome outside the offered choices");
    }
    let talk = match &q.consult {
        Some(consult) if active_consult(q, &consult.talk) => consult.talk.clone(),
        _ => bail!("question {} has no active consultation to end", q.short()),
    };
    q.schema = crate::ask::SCHEMA;
    q.consult_outcome = Some(ConsultOutcome {
        text: text.to_owned(),
        talk,
        at: Timestamp::now(),
    });
    Ok(())
}

/// `magi answer --reply-other` on question `id` (a full id): [`record_other`]
/// under the store's lock, then what to print. Shared by the terminal's direct
/// path and a chat turn's control bridge.
pub fn reply_other(store: &Questions, id: &str, text: &str) -> Result<String> {
    let (q, ()) = store.update(id, |r| record_other(r, text))?;
    Ok(format!(
        "consultation ended for {}; decision recorded as a note. Question remains open and its waiter remains held; no choice action executed.",
        q.short()
    ))
}

/// Record the hand-over and queue the question as the chat's next message.
///
/// Returns `false` (and changes nothing) when the question was already handed
/// over. The question is not otherwise touched: still `Open`, no thread turn.
/// When queueing fails the record is withdrawn, so the owner can try again.
pub fn begin(questions: &Questions, talks: &Talks, q: &Question, talk: &Talk) -> Result<bool> {
    let (q, fresh) = questions.update(&q.id, |r| {
        anyhow::ensure!(
            r.routed_task.is_none(),
            "task decision is delivered by its own chat turn"
        );
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
    // Serialize dispatch with answers, but never wait for conversation locks
    // under the expiring question lock. Each retry reloads the latest question.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let dispatch = loop {
        let mut busy = false;
        let updated = questions.update(&q.id, |r| {
            if !r.status.open() || r.consult != q.consult {
                return Ok(false);
            }
            if talk::try_queue_consult(
                talks,
                &talk.id,
                &crate::prompt::chat_consult(r),
                Some(crate::prompt::chat_consult_display(r)),
            )?
            .is_none()
            {
                busy = true;
                bail!("the conversation store is locked by another process");
            }
            Ok(true)
        });
        if busy && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        break updated;
    };
    if let Err(e) = dispatch {
        let _ = questions.update(&q.id, |r| {
            if r.status.open() && r.consult == q.consult {
                r.consult = None;
            }
            Ok(())
        });
        return Err(e).context("queue the question into the chat");
    }
    Ok(dispatch?.1)
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
    let Some(lease) = talks.claim_turn(&talk.id)? else {
        return Ok(Started::Busy);
    };
    if !begin(questions, talks, q, talk)? {
        return Ok(Started::Nothing);
    }
    crate::chat_worker::drain_turns(talks, &talk.id, lease, cfg).await?;
    Ok(Started::Answered)
}

/// 表示専用の生存確認。ワーカーストアの排他ロックやレコード修復を行わない。
/// 送信可否の判断には引き続き [`crate::chat_worker::turn_worker_active`] を使う。
pub fn turn_worker_observed(talks: &Talks, talk_id: &str) -> bool {
    crate::worker::observe_active(talks.home()).is_ok_and(|jobs| {
        jobs.iter().any(|job| {
            job.kind == "chat"
                && job.payload.get("talk").and_then(serde_json::Value::as_str) == Some(talk_id)
        })
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ServiceRequest {
    service: String,
    repo: std::path::PathBuf,
    cfg: Option<Config>,
    #[serde(default)]
    explicit_config: Option<std::path::PathBuf>,
}

fn service_due(home: &std::path::Path, service: &str, now: Timestamp) -> bool {
    let interval = match service {
        "waiter" => crate::waiter::TICK,
        "deputies" => crate::deputy::TICK,
        "release-watch" => crate::release_watch::LAP,
        _ => return false,
    };
    std::fs::read_to_string(home.join("service-ticks").join(service))
        .ok()
        .and_then(|text| text.parse::<i64>().ok())
        .is_none_or(|last| now.as_second().saturating_sub(last) >= interval.as_secs() as i64)
}

fn service_has_work(
    home: &std::path::Path,
    repo: &std::path::Path,
    cfg: Option<&Config>,
    service: &str,
    now: Timestamp,
) -> bool {
    match service {
        "waiter" => crate::waiter::Waiter::new(
            Questions::at(home.join("questions")),
            home.to_path_buf(),
            cfg.cloned(),
        )
        .has_work(now),
        "deputies" => crate::deputy::Deputies::new(
            Questions::at(home.join("questions")),
            home.to_path_buf(),
            cfg.cloned(),
            repo.to_path_buf(),
            cfg.map_or(0, |cfg| cfg.daemon.max_deputies),
            std::sync::Arc::new(|| false),
        )
        .has_work(now),
        "release-watch" => cfg.is_some_and(|cfg| cfg.daemon.release_stall_minutes > 0),
        _ => false,
    }
}

/// Finite service work outlives the scheduler, but never pins a service host
/// to an obsolete executable. Its next tick is started by the new scheduler.
pub async fn start_services(
    home: &std::path::Path,
    repo: &std::path::Path,
    cfg: Option<&Config>,
    explicit_config: Option<&std::path::Path>,
) -> Result<()> {
    let active = crate::worker::active(home)?;
    for service in ["waiter", "deputies", "release-watch"] {
        let key = format!("service:{service}");
        // Adoption is by operation key, not by the new scheduler's config
        // payload/schema. An old invocation owns its started request.
        if active.iter().any(|job| job.key == key) {
            continue;
        }
        if !service_due(home, service, Timestamp::now()) {
            continue;
        }
        let service_cfg = if service == "release-watch" {
            // The original watcher reloaded settings each lap. Do this cheap
            // admission read here so disabled/idle services never fork a CLI.
            Config::discover(repo, explicit_config)
                .ok()
                .map(|(cfg, _)| cfg)
        } else {
            cfg.cloned()
        };
        if !service_has_work(home, repo, service_cfg.as_ref(), service, Timestamp::now()) {
            let stamps = home.join("service-ticks");
            std::fs::create_dir_all(&stamps)?;
            std::fs::write(
                stamps.join(service),
                Timestamp::now().as_second().to_string(),
            )?;
            continue;
        }
        let payload = serde_json::to_value(ServiceRequest {
            service: service.to_owned(),
            repo: repo.to_path_buf(),
            cfg: service_cfg,
            explicit_config: explicit_config.map(std::path::Path::to_path_buf),
        })?;
        if let Err(error) = crate::worker::spawn(home, "service", &key, payload).await {
            tracing::warn!("service {service} worker could not start: {error:#}");
        }
    }
    Ok(())
}

/// One leased service tick. Existing question leases, seat records, and release
/// receipts remain the only authorities for delivery and once-only effects.
pub async fn run_service_worker(home: &std::path::Path, payload: serde_json::Value) -> Result<()> {
    let request: ServiceRequest = serde_json::from_value(payload)?;
    anyhow::ensure!(
        matches!(
            request.service.as_str(),
            "waiter" | "deputies" | "release-watch"
        ),
        "unknown conversation service"
    );
    let stamps = home.join("service-ticks");
    std::fs::create_dir_all(&stamps)?;
    std::fs::write(
        stamps.join(&request.service),
        Timestamp::now().as_second().to_string(),
    )?;
    let questions = Questions::at(home.join("questions"));
    match request.service.as_str() {
        "waiter" => {
            crate::waiter::Waiter::new(questions, home.to_path_buf(), request.cfg)
                .tick(Timestamp::now(), &|| false)
                .await;
        }
        "deputies" => {
            let max = request.cfg.as_ref().map_or_else(
                || Config::default().daemon.max_deputies,
                |cfg| cfg.daemon.max_deputies,
            );
            let deputies = crate::deputy::Deputies::new(
                questions,
                home.to_path_buf(),
                request.cfg,
                request.repo,
                max,
                std::sync::Arc::new(|| false),
            );
            deputies.dispatch(Timestamp::now()).await?;
        }
        "release-watch" => {
            if let Ok((cfg, _)) =
                Config::discover(&request.repo, request.explicit_config.as_deref())
            {
                let (repos, minutes) =
                    crate::daemon::release_watch_settings_from_config(home, &request.repo, &cfg);
                if minutes > 0 {
                    crate::release_watch::Watcher::new(
                        Box::new(crate::release_watch::GhForge),
                        home.to_path_buf(),
                    )
                    .lap(
                        &repos,
                        (minutes as i64).saturating_mul(60),
                        Timestamp::now().as_second(),
                        &|| false,
                    )
                    .await;
                }
            }
        }
        _ => unreachable!("validated service"),
    }
    // Preserve the delay after completion even when the scheduler changes
    // executable or restarts while this tick is still running.
    std::fs::write(
        stamps.join(&request.service),
        Timestamp::now().as_second().to_string(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::config::{AgentKind, AgentSpec, Config};
    use crate::queue::{CHAT_NODE, Source};
    use crate::talk::TalkStatus;

    #[test]
    fn service_cadence_is_durable_and_keeps_the_watchers_own_interval() {
        let home = tempfile::tempdir().expect("home");
        let stamps = home.path().join("service-ticks");
        std::fs::create_dir_all(&stamps).expect("stamps");
        for service in ["waiter", "deputies", "release-watch"] {
            std::fs::write(stamps.join(service), "100").expect("completion timestamp");
        }
        let at = |seconds| Timestamp::from_second(seconds).unwrap();
        assert!(!service_due(home.path(), "waiter", at(104)));
        assert!(service_due(home.path(), "waiter", at(105)));
        assert!(!service_due(home.path(), "deputies", at(104)));
        assert!(service_due(home.path(), "deputies", at(105)));
        assert!(!service_due(home.path(), "release-watch", at(399)));
        assert!(service_due(home.path(), "release-watch", at(400)));
    }

    #[test]
    fn idle_services_do_not_need_detached_processes() {
        let home = tempfile::tempdir().expect("home");
        for service in ["waiter", "deputies", "release-watch"] {
            assert!(!service_has_work(
                home.path(),
                home.path(),
                None,
                service,
                Timestamp::now(),
            ));
        }
        let mut disabled = Config::default();
        disabled.daemon.release_stall_minutes = 0;
        assert!(!service_has_work(
            home.path(),
            home.path(),
            Some(&disabled),
            "release-watch",
            Timestamp::now(),
        ));
    }

    #[tokio::test]
    async fn explicit_release_config_overrides_the_repository_without_launching_disabled_watchers()
    {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let explicit = home.path().join("serve-config.toml");
        std::fs::write(
            repo.path().join("magi.toml"),
            "[daemon]\nrelease_stall_minutes = 60\n",
        )
        .expect("repository watcher enabled");
        std::fs::write(&explicit, "[daemon]\nrelease_stall_minutes = 0\n")
            .expect("explicit watcher disabled");
        start_services(home.path(), repo.path(), None, Some(&explicit))
            .await
            .expect("idle service admission");
        let jobs = home.path().join("workers").join("jobs");
        assert!(
            std::fs::read_dir(jobs)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "serve --config must not launch the repository-configured watcher"
        );
    }

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
                max_turn_secs: None,
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
                max_turn_secs: None,
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
        // A closed conversation is reopened by `begin`, so it still qualifies.
        assert!(hit(std::slice::from_ref(&chat), &[closed], &q).is_some());
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
    fn associated_chat_remains_navigable_after_consultation_and_settlement() {
        let (tmp, store, mut talk) = talks();
        let chat = from_chat(&talk);
        let tasks = std::slice::from_ref(&chat);
        let mut q = question("implement");
        let questions = Questions::at(tmp.path().join("questions"));
        questions.put(&mut q).unwrap();
        assert!(begin(&questions, &store, &q, &talk).unwrap());
        q = questions.get(&q.id).unwrap();
        let associated = |q: &Question, talk: &Talk| {
            associated_talk(tasks, std::slice::from_ref(talk), q).map(|t| t.id)
        };
        assert_eq!(associated(&q, &talk), Some(talk.id.clone()));
        assert!(q.status.open());
        assert!(q.answer.is_none());
        assert!(origin_talk(tasks, std::slice::from_ref(&talk), &q).is_some());
        q.answer(crate::ask::Answer::Choice("Redis".into()))
            .unwrap();
        assert_eq!(associated(&q, &talk), Some(talk.id.clone()));
        assert!(origin_talk(tasks, std::slice::from_ref(&talk), &q).is_none());
        talk.status = TalkStatus::Closed;
        assert_eq!(associated(&q, &talk), Some(talk.id.clone()));
        let mut q = question("implement");
        q.abandon("expired");
        assert_eq!(associated(&q, &talk), Some(talk.id.clone()));
        assert!(origin_talk(tasks, std::slice::from_ref(&talk), &q).is_none());
        assert!(associated_talk(tasks, &[], &q).is_none());
        assert!(associated_talk(&[], std::slice::from_ref(&talk), &q).is_none());
        let notice = question(crate::bump::NOTICE_NODE);
        assert_eq!(associated(&notice, &talk), Some(talk.id.clone()));
        assert!(origin_talk(tasks, &[talk], &notice).is_none());
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
        assert!(origin_talk(std::slice::from_ref(&chat), &[closed], &q).is_some());
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
    fn a_recorded_chat_survives_deleted_ancestors() {
        let (_tmp, _store, talk) = talks();
        let chat = from_chat(&talk);
        assert_eq!(chat.origin_chat.as_deref(), Some(talk.id.as_str()));
        let mut f1 = followup_of(&chat, 1);
        f1.origin_chat = chat.origin_chat.clone();
        let mut f2 = followup_of(&f1, 2);
        f2.origin_chat = f1.origin_chat.clone();
        // Neither the chat task nor f1 is in the queue any more.
        let q = q_for(&f2);
        let got = origin_talk(std::slice::from_ref(&f2), std::slice::from_ref(&talk), &q);
        assert_eq!(got.map(|t| t.id), Some(talk.id.clone()));
        // The recorded id still has to name an open chat.
        assert!(origin_talk(&[f2], &[], &q).is_none());
    }

    #[test]
    fn a_task_without_the_field_still_walks_the_ancestry() {
        let (_tmp, _store, talk) = talks();
        let mut chat = from_chat(&talk);
        chat.origin_chat = None;
        let mut f1 = followup_of(&chat, 1);
        f1.origin_chat = None;
        let q = q_for(&f1);
        assert!(origin_talk(&[chat, f1.clone()], std::slice::from_ref(&talk), &q).is_some());
        // And the old JSON shape reads with the field absent.
        let mut v = serde_json::to_value(&f1).unwrap();
        v.as_object_mut().unwrap().remove("origin_chat");
        let back: Task = serde_json::from_value(v).unwrap();
        assert!(back.origin_chat.is_none());
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
    fn begin_reopens_a_closed_chat_before_queueing() {
        let (tmp, store, mut talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question("implement");
        questions.put(&mut q).unwrap();
        talk::close(&mut talk, &store).unwrap();
        assert!(!store.get(&talk.id).unwrap().status.open());

        assert!(begin(&questions, &store, &q, &talk).unwrap());

        let after = store.get(&talk.id).unwrap();
        assert!(after.status.open(), "the chat was reopened");
        assert!(after.pending.contains(&q.id), "{}", after.pending);
        assert!(questions.get(&q.id).unwrap().consult.is_some());
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

        let destination = store.get(&talk.id).unwrap();
        assert_eq!(destination.pending_messages.len(), 1);
        assert_eq!(
            destination.pending_messages[0].display,
            Some(crate::prompt::chat_consult_display(&q))
        );
        let queued = destination.pending;
        assert_eq!(
            queued.matches(&q.id).count(),
            1 + 3,
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
    fn chat_consult_has_each_marker_once() {
        use crate::prompt::{CHAT_CONSULT_END as E, CHAT_CONSULT_HEADING as H};
        for node in ["implement", crate::land::APPROVAL_NODE] {
            let mut q = question(node);
            let quoted = format!("{H} {E}");
            q.summary = quoted.clone();
            q.detail = quoted.clone();
            q.choices = vec![quoted.clone(), "b".into()];
            let s = crate::prompt::chat_consult(&q);
            assert_eq!(s.matches(H).count(), 1, "{s}");
            assert_eq!(s.matches(E).count(), 1, "{s}");
        }
    }

    /// What `magi answer --reply` does under the question lock.
    fn record(questions: &Questions, id: &str, reply: &str) -> Result<Question> {
        questions
            .update(id, |r| {
                let label = r.resolve_reply(reply)?.to_owned();
                r.answer(crate::ask::Answer::Choice(label))
            })
            .map(|(q, ())| q)
    }

    #[test]
    fn a_consulted_posting_question_records_the_chats_choice_without_a_quote() {
        use crate::github_text::{ASK_NODE, POST_ORIGINAL, USE_FALLBACK, USE_REWRITE};
        for choice in [POST_ORIGINAL, USE_REWRITE] {
            let (tmp, store, talk) = talks();
            let questions = Questions::at(tmp.path().join("questions"));
            let mut q = question(ASK_NODE);
            q.choices = vec![USE_FALLBACK.into(), choice.into()];
            questions.put(&mut q).unwrap();
            assert!(begin(&questions, &store, &q, &talk).unwrap());
            let queued = store.get(&talk.id).unwrap().pending;
            assert!(queued.contains("magi answer"), "{queued}");
            assert!(!queued.contains("--quote"), "{queued}");
            let other = if choice == POST_ORIGINAL {
                USE_REWRITE
            } else {
                POST_ORIGINAL
            };
            assert!(record(&questions, &q.id, other).is_err(), "not offered");
            let answered = record(&questions, &q.id, choice).unwrap();
            assert_eq!(answered.resolution().as_deref(), Some(choice));
            assert!(
                record(&questions, &q.id, USE_FALLBACK).is_err(),
                "a question is answered once"
            );
        }
    }

    #[test]
    fn merge_repair_consult_records_only_an_offered_choice() {
        use crate::land::{APPROVAL_NODE, APPROVE, HOLD, REPAIR};
        let (tmp, store, talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question(APPROVAL_NODE);
        q.choices = vec![APPROVE.into(), HOLD.into()];
        questions.put(&mut q).unwrap();
        begin(&questions, &store, &q, &talk).unwrap();
        assert!(record(&questions, &q.id, REPAIR).is_err(), "not offered");
        questions
            .update(&q.id, |r| {
                r.choices.push(REPAIR.into());
                Ok(())
            })
            .unwrap();
        let answered = record(&questions, &q.id, REPAIR).unwrap();
        assert_eq!(answered.resolution().as_deref(), Some(REPAIR));
    }

    #[test]
    fn approval_consult_records_the_chats_merge_answer() {
        let (tmp, store, talk) = talks();
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
        assert!(queued.contains("Silence holds"), "{queued}");
        assert!(queued.contains("--reply merge"), "{queued}");
        assert!(!queued.contains("--quote"), "{queued}");

        // The chat records the owner's "merge it" as the approval; nothing
        // else is needed. The land approval waiter consumes it unchanged.
        let answered = record(&questions, &q.id, crate::land::APPROVE).unwrap();
        assert_eq!(answered.resolution().as_deref(), Some(crate::land::APPROVE));
        assert!(record(&questions, &q.id, crate::land::HOLD).is_err());

        // An expired approval cannot be answered.
        let mut expired = question(crate::land::APPROVAL_NODE);
        expired.choices = q.choices.clone();
        expired.abandon("approval expired or head changed");
        assert!(
            expired
                .answer(crate::ask::Answer::Choice(crate::land::APPROVE.into()))
                .is_err()
        );
    }

    #[test]
    fn reply_other_is_a_note_that_ends_only_the_consultation() {
        let (tmp, store, talk) = talks();
        let questions = Questions::at(tmp.path().join("questions"));
        let mut q = question("implement");
        questions.put(&mut q).unwrap();
        assert!(
            questions
                .update(&q.id, |r| record_other(r, "Use Postgres"))
                .is_err(),
            "nothing to end before consultation"
        );
        begin(&questions, &store, &q, &talk).unwrap();
        assert!(pending_consults(&questions, &talk.id));
        for refused in ["", "  ", "SQLite"] {
            assert!(
                questions
                    .update(&q.id, |r| record_other(r, refused))
                    .is_err(),
                "{refused:?}"
            );
        }
        questions
            .update(&q.id, |r| record_other(r, "Use Postgres"))
            .unwrap();
        let after = questions.get(&q.id).unwrap();
        assert!(after.status.open());
        assert!(after.answer.is_none());
        let outcome = after.consult_outcome.unwrap();
        assert_eq!(outcome.text, "Use Postgres");
        assert_eq!(outcome.talk, talk.id);
        assert!(!pending_consults(&questions, &talk.id));
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
