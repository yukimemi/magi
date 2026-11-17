//! Finite, evidence-triggered continuation of accepted conversation goals.
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// A current observation is data, never a new owner instruction.
#[derive(Debug)]
pub(crate) struct Observation {
    pub facts: Value,
    pub fingerprint: String,
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .context("decode goal evidence")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn unavailable(error: &anyhow::Error) -> Value {
    json!({"unavailable": format!("{error:#}")})
}

trait PrObserver: Sync {
    fn current_pr(&self, repo: &Path, url: &str) -> impl Future<Output = Result<Value>> + Send;
}

struct CurrentPr;

impl PrObserver for CurrentPr {
    async fn current_pr(&self, repo: &Path, url: &str) -> Result<Value> {
        crate::chat_notify::current_pr(repo, url).await
    }
}

/// Local task/run evidence plus one timed forge lookup per distinct associated PR.
/// Timestamps and transcript prose are excluded from the wake fingerprint.
pub(crate) async fn observe(
    home: &Path,
    repo: &Path,
    tasks: &[String],
    runs: &[String],
    watches: &[String],
) -> Observation {
    observe_with_pr(home, repo, tasks, runs, watches, &CurrentPr).await
}

async fn observe_with_pr(
    home: &Path,
    repo: &Path,
    tasks: &[String],
    runs: &[String],
    watches: &[String],
    observer: &impl PrObserver,
) -> Observation {
    let mut task_facts = Vec::new();
    let mut run_ids = Vec::new();
    let mut historical_runs = std::collections::BTreeSet::new();
    let queue = crate::queue::Queue::at(home.join("queue"));
    for id in tasks {
        match queue.get(id) {
            Ok(task) => {
                if task.repo != repo {
                    task_facts.push(json!({"id":id,"evidence":{"unavailable":"task repository changed; goal scope requires review"}}));
                    continue;
                }
                historical_runs.extend(task.runs.iter().cloned());
                if let Some(run) = task.execution_runs().last()
                    && !run_ids.contains(run)
                {
                    run_ids.push(run.clone());
                }
                task_facts.push(json!({"id": task.id, "status":task.status,
                    "reason":task.hold_reason, "runs":task.execution_runs(),"current_run":task.execution_runs().last()}));
            }
            Err(error) => task_facts.push(json!({"id":id,"evidence":unavailable(&error)})),
        }
    }
    for run in runs {
        if !historical_runs.contains(run) && !run_ids.contains(run) {
            run_ids.push(run.clone());
        }
    }
    let mut run_facts = Vec::new();
    let mut pr_observations = std::collections::BTreeMap::new();
    let mut prs = Vec::new();
    for id in &run_ids {
        if !valid_id(id) {
            run_facts.push(json!({"id":id,"evidence":{"unavailable":"invalid run identity"}}));
            continue;
        }
        match read_json(&home.join("runs").join(id).join("run.json")) {
            Ok(run) => {
                let winner = run["candidates"].as_array().and_then(|candidates| {
                    candidates
                        .iter()
                        .find(|candidate| candidate["label"] == run["tally"]["winner"])
                });
                let branch = winner.and_then(|candidate| candidate["branch"].as_str());
                let artifact_head = if let Some(branch) = branch {
                    let revision = format!("{branch}^{{commit}}");
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        crate::git::git_raw(
                            repo,
                            &["rev-parse", "--verify", "--end-of-options", &revision],
                        ),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .filter(|output| output.ok())
                    .map(|output| output.stdout)
                } else {
                    None
                };
                if let Some(url) = run.pointer("/pr/url").and_then(Value::as_str) {
                    if !pr_observations.contains_key(url) {
                        let observed = observer
                            .current_pr(repo, url)
                            .await
                            .unwrap_or_else(|error| unavailable(&error));
                        pr_observations.insert(url.to_owned(), observed);
                    }
                    prs.push(json!({"run":id,"observation":pr_observations[url]}));
                }
                run_facts.push(json!({"id":id,"status":run["status"],
                    "pr":run["pr"],"head":run["pr_pushed_head"],"merge":run["merge"],
                    "resource_block":run["resource_block"],"base_sync":run["base_sync"],
                    "gate_head":run["gate_head"],"gate":run["gate"],
                    "gate_ran":run["gate_ran"], "artifact_head":artifact_head,
                    "github_text":run["github_text"],"land_approval":run["land_approval"]}));
            }
            Err(error) => run_facts.push(json!({"id":id,"evidence":unavailable(&error)})),
        }
    }
    let mut watch_facts = Vec::new();
    for id in watches {
        if !valid_id(id) {
            watch_facts
                .push(json!({"id":id,"observation":{"unavailable":"invalid watch identity"}}));
            continue;
        }
        let value = read_json(&home.join("chat-watches").join(id).join("job.json"))
            .map(|job| json!({"result":job["result"]}))
            .unwrap_or_else(|error| unavailable(&error));
        watch_facts.push(json!({"id":id,"observation":value}));
    }
    let decisions = match crate::task_inquiry::Outbox::at(home).list() {
        Ok(inquiries) => Value::Array(
            inquiries
                .into_iter()
                .filter(|inquiry| {
                    tasks.contains(&inquiry.task)
                        || inquiry.run.as_ref().is_some_and(|id| run_ids.contains(id))
                })
                .filter_map(|inquiry| {
                    inquiry.decision.map(|decision| json!({
                "id":inquiry.id,"task":inquiry.task,"run":inquiry.run,
                "status":decision.status,"request":decision.request,"error":decision.error,
                "retry_safe":decision.retry_safe
            }))
                })
                .collect(),
        ),
        Err(error) => unavailable(&error),
    };
    let facts = json!({"tasks":task_facts,"runs":run_facts,"prs":prs,"watches":watch_facts,"decisions":decisions});
    let fingerprint = facts.to_string();
    Observation { facts, fingerprint }
}

fn satisfied(goal: &crate::chat_goal::Goal, facts: &Value) -> bool {
    use crate::chat_goal::Completion;
    let runs = facts["runs"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if matches!(
        goal.condition.kind,
        Completion::TasksDone | Completion::RunsMerged
    ) && facts["tasks"].as_array().is_some_and(|tasks| {
        tasks.iter().any(|task| {
            task["current_run"].is_null()
                || !runs.iter().any(|run| run["id"] == task["current_run"])
        })
    }) {
        return false;
    }
    match goal.condition.kind {
        Completion::OwnerReview => false,
        Completion::RunsMerged => {
            !runs.is_empty()
                && runs.iter().all(|run| {
                    let Some(head) = run["head"].as_str().filter(|head| !head.is_empty()) else {
                        return false;
                    };
                    facts["prs"].as_array().is_some_and(|prs| {
                        prs.iter().any(|pr| {
                            pr["run"] == run["id"]
                                && pr["observation"]["state"] == "MERGED"
                                && pr["observation"]["head"] == head
                                && !pr["observation"]["merged_at"].is_null()
                        })
                    })
                })
        }
        Completion::TasksDone => {
            !runs.is_empty()
                && runs.iter().all(|run| {
                    let gate = run["gate"].as_array();
                    run["gate_ran"] == true
                        && run["gate_head"]
                            .as_str()
                            .is_some_and(|head| !head.is_empty())
                        && gate.is_some_and(|commands| {
                            !commands.is_empty()
                                && commands
                                    .iter()
                                    .all(|c| c["code"] == 0 && c["resource_blocked"] != true)
                        })
                        && run["artifact_head"] == run["gate_head"]
                })
        }
        Completion::WatchesSatisfied => facts["watches"].as_array().is_some_and(|watches| {
            !watches.is_empty()
                && watches.iter().all(|watch| {
                    watch["observation"]["result"]
                        .as_str()
                        .is_some_and(|result| result == "exited with status 0")
                })
        }),
    }
}

/// Bounded restart reconciliation. Unchanged evidence never spends another turn.
pub(crate) async fn sweep(talks: &crate::talk::Talks, parking: bool) {
    if parking {
        return;
    }
    use crate::chat_goal::State;
    let mut pending = Vec::new();
    for talk in talks.list() {
        if !talk.status.open() || !talk.pending.is_empty() || !talk.pending_attachments.is_empty() {
            continue;
        }
        if talks.turn_held(&talk.id) || crate::chat_worker::turn_worker_active(talks, &talk.id) {
            continue;
        }
        for goal in &talk.goals {
            if matches!(goal.state, State::Runnable | State::Waiting) {
                pending.push((goal.last_checked, talk.id.clone(), goal.id.clone()));
            }
        }
    }
    pending.sort_by_key(|entry| entry.0);
    for (_, chat, id) in pending.into_iter().take(8) {
        if let Err(error) = reconcile(talks, &chat, &id).await {
            tracing::warn!("chat goal {id}: reconciliation deferred: {error:#}");
        }
    }
}

async fn reconcile(talks: &crate::talk::Talks, chat: &str, id: &str) -> Result<()> {
    use crate::chat_goal::State;
    let talk = talks.get(chat)?;
    let Some(goal) = talk.goals.iter().find(|goal| goal.id == id) else {
        return Ok(());
    };
    if crate::chat_worker::turn_worker_active(talks, chat) || talks.turn_held(chat) {
        return Ok(());
    }
    if goal.started.is_some() {
        talks.mutate(chat, |talk| {
            if let Some(goal) = talk.goals.iter_mut().find(|goal| goal.id == id) {
                goal.state = State::Paused;
                goal.step =
                    "Interrupted continuation; reconcile accepted effects before retrying".into();
            }
            Ok(())
        })?;
        return Ok(());
    }
    if goal.rechecks >= 240 {
        talks.mutate(chat, |talk| {
            if let Some(goal) = talk.goals.iter_mut().find(|goal| goal.id == id)
                && goal.rechecks >= 240 && goal.active() {
                goal.state = State::Paused;
                goal.step = "Evidence recheck budget exhausted; owner input is required to continue observing".into();
            }
            Ok(())
        })?;
        return Ok(());
    }
    let now = jiff::Timestamp::now();
    if goal
        .last_checked
        .is_some_and(|last| now.as_second() - last.as_second() < 60)
    {
        return Ok(());
    }
    let observation = observe(
        talks.home(),
        &talk.repo,
        &goal.tasks,
        &goal.runs,
        &goal.watches,
    )
    .await;
    let complete = satisfied(goal, &observation.facts);
    let previous = goal.fingerprint.clone();
    let goal_snapshot = goal.clone();
    let generation = talks.mutate(chat, |fresh| {
        let Some(goal) = fresh.goals.iter_mut().find(|goal| goal.id == id) else {
            return Ok(None);
        };
        if !fresh.status.open()
            || goal.fingerprint != previous
            || goal.tasks != goal_snapshot.tasks
            || goal.runs != goal_snapshot.runs
            || goal.watches != goal_snapshot.watches
            || goal.wake != goal_snapshot.wake
            || !matches!(goal.state, State::Runnable | State::Waiting)
        {
            return Ok(None);
        }
        goal.last_checked = Some(now);
        goal.rechecks += 1;
        goal.observe(
            observation.fingerprint,
            observation.facts.to_string(),
            complete,
        );
        if goal.wake > goal.handled && goal.started.is_none() && goal.state == State::Runnable {
            Ok(Some(goal.wake))
        } else {
            Ok(None)
        }
    })?;
    if let Some(generation) = generation {
        crate::worker::spawn(
            talks.home(),
            "chat",
            &format!("goal:{chat}:{id}:{generation}"),
            json!({"talk":chat,"goal_wake":{"id":id,"generation":generation}}),
        )
        .await?;
    }
    Ok(())
}

/// Separate reserved input: no draft is promoted, cleared, or implicitly sent.
pub(crate) async fn run_wake(
    talks: &crate::talk::Talks,
    chat: &str,
    id: &str,
    generation: u64,
) -> Result<()> {
    let talk = talks.get(chat)?;
    if !talk.status.open()
        || !talk.pending.is_empty()
        || !talk.pending_attachments.is_empty()
        || !talk.goals.iter().any(|goal| {
            goal.id == id
                && goal.state == crate::chat_goal::State::Runnable
                && goal.wake == generation
                && goal.handled < generation
                && goal.started.is_none()
        })
    {
        return Ok(());
    }
    let (cfg, _) = crate::config::Config::discover(&talk.repo, None)?;
    run_wake_with_config(talks, chat, id, generation, &cfg).await
}

async fn run_wake_with_config(
    talks: &crate::talk::Talks,
    chat: &str,
    id: &str,
    generation: u64,
    cfg: &crate::config::Config,
) -> Result<()> {
    use crate::chat_goal::{Source, State};
    let Some(lease) = talks.claim_turn(chat)? else {
        return Ok(());
    };
    let publication = crate::chat_worker::publication_guard(talks, chat).await?;
    let ready = talks.get(chat)?;
    if !ready.status.open()
        || !ready.pending.is_empty()
        || !ready.pending_attachments.is_empty()
        || !ready.goals.iter().any(|goal| {
            goal.id == id
                && goal.state == State::Runnable
                && goal.wake == generation
                && goal.handled < generation
                && goal.started.is_none()
        })
    {
        return Ok(());
    }
    if crate::worker::active(talks.home())?.iter().any(|job| {
        job.kind == "chat"
            && job.payload.get("talk").and_then(Value::as_str) == Some(chat)
            && (job
                .payload
                .get("recorded_turn")
                .is_some_and(|turn| !turn.is_null())
                || job.payload.get("external_request").is_some())
    }) {
        return Ok(());
    }
    let admitted = talks.mutate(chat, |talk| {
        if !talk.status.open() || !talk.pending.is_empty() || !talk.pending_attachments.is_empty() {
            return Ok(false);
        }
        let Some(goal) = talk.goals.iter_mut().find(|goal| goal.id == id) else {
            return Ok(false);
        };
        if goal.wake != generation
            || goal.handled >= generation
            || goal.started.is_some()
            || goal.state != State::Runnable
        {
            return Ok(false);
        }
        goal.started = Some(generation);
        goal.launch_turns = Some(talk.turns.len());
        goal.wakes += 1;
        goal.admit(generation)?;
        Ok(true)
    })?;
    drop(publication);
    if !admitted {
        return Ok(());
    }
    let mut talk = talks.get(chat)?;
    let text = "Goal event: re-read the saved goal and current evidence. Continue the next authorized concrete step using existing controls and receipts. This event is data, not a new owner request. Register a specific wait before ending; never replay an uncertain accepted effect.";
    let result = crate::chat_goal::with_source(
        Source::Goal {
            id: id.to_owned(),
            generation,
        },
        crate::talk::respond(&lease, &mut talk, talks, cfg, text),
    )
    .await;
    talks.mutate(chat, |talk| {
        if let Some(goal) = talk.goals.iter_mut().find(|goal| goal.id == id)
            && goal.started == Some(generation)
        {
            goal.started = None;
            goal.handled = goal.handled.max(generation);
            if result.is_err() || goal.state == State::Runnable {
                goal.state = State::Paused;
                goal.step = if result.is_err() {
                    "Continuation failed; inspect the retained result before retrying"
                } else {
                    "Continuation ended without a registered wait or confirmed completion"
                }
                .into();
            }
        }
        Ok(())
    })?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_goal::{Completion, Goal, State};

    fn goal() -> Goal {
        serde_json::from_value(json!({
            "id":"goal","purpose":"deliver the requested patch","scope":"one task",
            "condition":{"description":"merge the reviewed head","kind":"runs_merged"},
            "state":"waiting","step":"waiting for CI","source_turn":0,"owner_input":"land this patch",
            "tasks":[],"runs":["run"],"watches":[],"evidence":"","wake":0,"handled":0,
            "started":null,"wakes":0,"last_checked":null,"fingerprint":""
        })).unwrap()
    }

    #[derive(Default)]
    struct FakePrObserver {
        replies: std::sync::Mutex<std::collections::BTreeMap<String, Value>>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl PrObserver for FakePrObserver {
        async fn current_pr(&self, _: &Path, url: &str) -> Result<Value> {
            self.calls.lock().unwrap().push(url.to_owned());
            self.replies
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .with_context(|| format!("fixture PR unavailable: {url}"))
        }
    }

    impl FakePrObserver {
        fn assert_one_lookup_per_pr(&self, urls: &[String]) {
            let calls = std::mem::take(&mut *self.calls.lock().unwrap());
            for url in urls {
                assert_eq!(
                    calls.iter().filter(|called| *called == url).count(),
                    1,
                    "every distinct PR must be refreshed once per observation"
                );
            }
            assert_eq!(
                calls.len(),
                urls.len(),
                "shared run associations must not duplicate forge calls"
            );
        }
    }

    fn pr_goal_fixture(home: &Path) -> (Goal, FakePrObserver, Vec<String>) {
        let mut goal = goal();
        goal.runs.clear();
        let observer = FakePrObserver::default();
        let urls: Vec<_> = (1..=5)
            .map(|index| format!("https://example.invalid/pull/{index}"))
            .collect();
        for (index, url) in urls.iter().enumerate() {
            let run = format!("run-{index}");
            let head = format!("{:040x}", index + 1);
            let path = home.join("runs").join(&run);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("run.json"),
                json!({"pr":{"url":url},"pr_pushed_head":head}).to_string(),
            )
            .unwrap();
            goal.runs.push(run);
            observer.replies.lock().unwrap().insert(
                url.clone(),
                json!({"state":"MERGED","head":head,"merged_at":"2026-01-01T00:00:00Z"}),
            );
        }
        // A second run sharing the fifth PR still needs a corresponding fact.
        let alias = home.join("runs").join("run-shared");
        std::fs::create_dir_all(&alias).unwrap();
        std::fs::write(
            alias.join("run.json"),
            json!({"pr":{"url":urls[4]},"pr_pushed_head":format!("{:040x}",5)}).to_string(),
        )
        .unwrap();
        goal.runs.push("run-shared".into());
        (goal, observer, urls)
    }

    #[tokio::test]
    async fn fifth_pull_request_changes_wake_and_complete_all_related_runs() {
        let temp = tempfile::tempdir().unwrap();
        let (mut goal, observer, urls) = pr_goal_fixture(temp.path());
        let head = format!("{:040x}", 5);
        observer.replies.lock().unwrap().insert(
            urls[4].clone(),
            json!({"state":"OPEN","head":head,"merged_at":null,"checks":[{"verdict":"pending"}]}),
        );
        let pending =
            observe_with_pr(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(!satisfied(&goal, &pending.facts));
        goal.observe(pending.fingerprint, pending.facts.to_string(), false);
        observer.assert_one_lookup_per_pr(&urls);

        observer.replies.lock().unwrap().insert(
            urls[4].clone(),
            json!({"state":"OPEN","head":head,"merged_at":null,"checks":[{"verdict":"failed"}]}),
        );
        let failed =
            observe_with_pr(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert_ne!(
            failed.fingerprint, goal.fingerprint,
            "CI changes beyond the first four PRs must wake the goal"
        );
        assert!(!satisfied(&goal, &failed.facts));
        goal.observe(failed.fingerprint, failed.facts.to_string(), false);
        assert_eq!(goal.wake, 2);
        observer.assert_one_lookup_per_pr(&urls);

        observer.replies.lock().unwrap().insert(urls[4].clone(),
            json!({"state":"MERGED","head":format!("{:040x}",99),"merged_at":"2026-01-01T00:00:00Z"}));
        let moved =
            observe_with_pr(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(
            !satisfied(&goal, &moved.facts),
            "the fifth PR must match its saved head too"
        );
        observer.assert_one_lookup_per_pr(&urls);

        observer.replies.lock().unwrap().insert(
            urls[4].clone(),
            json!({"state":"MERGED","head":head,"merged_at":"2026-01-01T00:00:00Z"}),
        );
        let merged =
            observe_with_pr(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(
            satisfied(&goal, &merged.facts),
            "all six run associations must have exact-head merge evidence"
        );
        goal.observe(merged.fingerprint, merged.facts.to_string(), true);
        assert_eq!(goal.state, State::Completed);
        observer.assert_one_lookup_per_pr(&urls);
    }

    #[tokio::test]
    async fn unavailable_pull_request_does_not_hide_later_ci_or_reuse_old_merge_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let (goal, observer, urls) = pr_goal_fixture(temp.path());
        let merged =
            observe_with_pr(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(satisfied(&goal, &merged.facts));
        observer.assert_one_lookup_per_pr(&urls);
        observer.replies.lock().unwrap().remove(&urls[0]);
        observer.replies.lock().unwrap().insert(urls[4].clone(),
            json!({"state":"OPEN","head":format!("{:040x}",5),"merged_at":null,"checks":[{"verdict":"failed"}]}));
        let current =
            observe_with_pr(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(
            !satisfied(&goal, &current.facts),
            "previously merged evidence cannot replace a failed fresh lookup"
        );
        assert_ne!(merged.fingerprint, current.fingerprint);
        let prs = current.facts["prs"].as_array().unwrap();
        assert!(
            prs.iter()
                .any(|pr| pr["run"] == "run-0" && pr["observation"]["unavailable"].is_string())
        );
        assert!(prs.iter().any(
            |pr| pr["run"] == "run-4" && pr["observation"]["checks"][0]["verdict"] == "failed"
        ));
        observer.assert_one_lookup_per_pr(&urls);
    }

    #[test]
    fn merge_completion_requires_observed_exact_head_not_task_done_or_pr_creation() {
        let goal = goal();
        let mut facts = json!({"tasks":[{"status":"done","current_run":"run"}],
            "runs":[{"id":"run","head":"abc","pr":{"url":"https://example.invalid/pull/1"}}],
            "prs":[{"run":"run","observation":{"state":"OPEN","head":"abc","merged_at":null}}]});
        assert!(!satisfied(&goal, &facts));
        facts["prs"][0]["observation"] = json!({"state":"MERGED","head":"other","merged_at":"now"});
        assert!(!satisfied(&goal, &facts));
        facts["prs"][0]["observation"]["head"] = json!("abc");
        assert!(satisfied(&goal, &facts));
        facts["prs"][0]["observation"] = json!({"unavailable":"forge offline"});
        assert!(!satisfied(&goal, &facts));
    }

    #[test]
    fn implementation_completion_requires_a_verified_delivered_commit() {
        let mut goal = goal();
        goal.condition.kind = Completion::TasksDone;
        let mut facts = json!({"runs":[{"id":"run","gate_ran":true,"gate_head":"abc",
            "gate":[{"code":0}], "artifact_head":"old"}]});
        assert!(!satisfied(&goal, &facts));
        facts["runs"][0]["artifact_head"] = json!("abc");
        assert!(satisfied(&goal, &facts));
        facts["runs"][0]["gate"][0]["resource_blocked"] = json!(true);
        assert!(!satisfied(&goal, &facts));
    }

    #[tokio::test]
    async fn restart_retains_uncertain_started_work_without_replaying_it() {
        let temp = tempfile::tempdir().unwrap();
        let talks = crate::talk::Talks::at(temp.path().join("talks"));
        let mut talk = crate::talk::seeded_for_test("chat", temp.path().into());
        let mut goal = goal();
        goal.state = State::Runnable;
        goal.wake = 1;
        goal.started = Some(1);
        talk.goals.push(goal);
        talks.put(&mut talk).unwrap();
        reconcile(&talks, "chat", "goal").await.unwrap();
        let saved = talks.get("chat").unwrap();
        assert_eq!(saved.goals[0].state, State::Paused);
        assert_eq!(saved.goals[0].started, Some(1));
        assert!(saved.turns.is_empty());
        assert!(crate::worker::active(temp.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn owner_draft_and_stopped_goal_are_never_consumed_by_a_wake() {
        let temp = tempfile::tempdir().unwrap();
        let talks = crate::talk::Talks::at(temp.path().join("talks"));
        let mut talk = crate::talk::seeded_for_test("chat", temp.path().into());
        let mut goal = goal();
        goal.state = State::Stopped;
        goal.wake = 1;
        talk.goals.push(goal);
        talk.pending = "keep my unsent question".into();
        talks.put(&mut talk).unwrap();
        run_wake(&talks, "chat", "goal", 1).await.unwrap();
        let saved = talks.get("chat").unwrap();
        assert_eq!(saved.pending, "keep my unsent question");
        assert_eq!(saved.goals[0].state, State::Stopped);
        assert!(saved.goals[0].started.is_none());
        assert!(saved.turns.is_empty());
    }

    #[tokio::test]
    async fn completion_checks_the_entire_task_and_run_scope() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "Fixture artifact",
            ],
        ] {
            let result = crate::git::git_raw(&repo, &args).await.unwrap();
            assert!(result.ok(), "{}", result.stderr);
        }
        let head = crate::git::git_raw(&repo, &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .stdout;
        let queue = crate::queue::Queue::at(home.join("queue"));
        let mut goal = goal();
        goal.condition.kind = Completion::TasksDone;
        goal.tasks.clear();
        goal.runs.clear();
        for index in 0..32 {
            let run = format!("run-{index}");
            let path = home.join("runs").join(&run);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("run.json"),
                json!({
                    "gate_ran":true,"gate_head":head,"gate":[{"code":0}],
                    "tally":{"winner":"A"},"candidates":[{"label":"A","branch":"HEAD"}]
                })
                .to_string(),
            )
            .unwrap();
            let mut task = crate::queue::Task::new(
                "Verified patch".into(),
                "deliver the patch".into(),
                repo.clone(),
                crate::queue::Source::Human,
            );
            task.runs.push(run.clone());
            queue.put(&mut task).unwrap();
            goal.tasks.push(task.id);
            goal.runs.push(run);
        }
        let mut unstarted = crate::queue::Task::new(
            "Unstarted patch".into(),
            "deliver this patch too".into(),
            repo.clone(),
            crate::queue::Source::Human,
        );
        queue.put(&mut unstarted).unwrap();
        goal.tasks.push(unstarted.id.clone());
        goal.runs.push("missing-run".into());
        let observation = observe(&home, &repo, &goal.tasks, &goal.runs, &[]).await;
        let mut facts = observation.facts;
        assert!(
            !satisfied(&goal, &facts),
            "unobserved work must never be declared complete"
        );
        assert!(
            facts["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|task| task["id"] == unstarted.id)
        );
        assert!(
            facts["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|run| run["id"] == "missing-run")
        );

        // Each tail independently prevents completion; the fully verified
        // prefix is a delivered artifact rather than a vacuous failure.
        facts["runs"].as_array_mut().unwrap().pop();
        assert!(
            !satisfied(&goal, &facts),
            "the thirty-third task is still unstarted"
        );
        facts["tasks"].as_array_mut().unwrap().pop();
        assert!(satisfied(&goal, &facts));
        facts["runs"].as_array_mut().unwrap().push(json!({
            "id":"missing-run","evidence":{"unavailable":"not started"}
        }));
        assert!(
            !satisfied(&goal, &facts),
            "the thirty-third direct run is still unavailable"
        );
    }

    #[tokio::test]
    async fn watch_completion_requires_the_tail_and_invalid_identities() {
        let temp = tempfile::tempdir().unwrap();
        let mut goal = goal();
        goal.condition.kind = Completion::WatchesSatisfied;
        goal.watches = (0..33).map(|index| format!("watch-{index}")).collect();
        for id in &goal.watches[..32] {
            let path = temp.path().join("chat-watches").join(id);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("job.json"),
                r#"{"result":"exited with status 0"}"#,
            )
            .unwrap();
        }
        let observation = observe(temp.path(), temp.path(), &[], &[], &goal.watches).await;
        assert!(
            !satisfied(&goal, &observation.facts),
            "the thirty-third watch has no result"
        );
        let path = temp.path().join("chat-watches").join(&goal.watches[32]);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("job.json"),
            r#"{"result":"exited with status 0"}"#,
        )
        .unwrap();
        let observation = observe(temp.path(), temp.path(), &[], &[], &goal.watches).await;
        assert!(satisfied(&goal, &observation.facts));
        goal.watches.push("../invalid".into());
        let observation = observe(temp.path(), temp.path(), &[], &[], &goal.watches).await;
        assert!(
            !satisfied(&goal, &observation.facts),
            "invalid identities are unknown, not omitted"
        );
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use crate::config::{AgentKind, AgentSpec, Config};

    #[tokio::test]
    async fn changed_event_resumes_same_goal_once_across_reopened_store_and_concurrent_starters() {
        let temp = tempfile::tempdir().unwrap();
        let talks = crate::talk::Talks::at(temp.path().join("talks"));
        let script = temp.path().join("finite-agent.sh");
        std::fs::write(&script,
            "cat > \"$GOAL_PROMPT\"\nprintf 'step\\n' >> \"$GOAL_CALLS\"\nprintf 'Inspected the retained work; waiting for the registered event.\\n'\n"
        ).unwrap();
        let calls = temp.path().join("calls");
        let prompt = temp.path().join("prompt");
        let cfg = Config {
            agents: vec![AgentSpec {
                id: "worker".into(),
                kind: AgentKind::Command,
                model: None,
                command: vec!["sh".into(), script.to_string_lossy().into_owned()],
                extra_args: Vec::new(),
                env: std::collections::BTreeMap::from([
                    ("GOAL_CALLS".into(), calls.to_string_lossy().into_owned()),
                    ("GOAL_PROMPT".into(), prompt.to_string_lossy().into_owned()),
                ]),
                prompt_delivery: None,
                max_turn_secs: None,
            }],
            ..Config::default()
        };
        let mut talk = crate::talk::seeded_for_test("finite-goal", temp.path().into());
        talk.agent = "worker".into();
        talk.persona = "asuka".into();
        talk.title = Some(crate::talk::ChatTitle {
            text: "Retained work".into(),
            source: crate::talk::TitleSource::Manual,
        });
        talks.put(&mut talk).unwrap();
        crate::talk::record(&mut talk, &talks, "repair and land this task", Vec::new()).unwrap();
        let queue = crate::queue::Queue::at(temp.path().join("queue"));
        let mut task = crate::queue::Task::new(
            "Retained patch".into(),
            "repair the existing patch".into(),
            talk.repo.clone(),
            crate::queue::Source::Agent {
                run: talk.id.clone(),
                node: crate::queue::CHAT_NODE.into(),
            },
        );
        task.status = crate::queue::TaskStatus::Held;
        task.hold_reason = Some("existing verification failed".into());
        queue.put(&mut task).unwrap();
        let mut goal: crate::chat_goal::Goal = serde_json::from_value(json!({
            "id":"deliver","purpose":"Repair the existing patch without a second owner message",
            "scope":"the retained task only",
            "condition":{"description":"merge the approved head","kind":"runs_merged"},
            "state":"waiting","step":"waiting for task result","source_turn":0,
            "owner_input":"repair and land this task","tasks":[task.id],"runs":[],"watches":[],
            "evidence":"","wake":0,"handled":0,"started":null,"wakes":0,
            "last_checked":null,"fingerprint":""
        }))
        .unwrap();
        let event = observe(
            temp.path(),
            &talk.repo,
            &goal.tasks,
            &goal.runs,
            &goal.watches,
        )
        .await;
        goal.observe(event.fingerprint, event.facts.to_string(), false);
        talk.goals.push(goal);
        talks.put(&mut talk).unwrap();
        let (first, duplicate) = tokio::join!(
            Box::pin(run_wake_with_config(&talks, &talk.id, "deliver", 1, &cfg)),
            Box::pin(run_wake_with_config(&talks, &talk.id, "deliver", 1, &cfg))
        );
        first.unwrap();
        duplicate.unwrap();
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        let saved = talks.get(&talk.id).unwrap();
        assert_eq!(saved.goals[0].handled, 1);
        assert!(saved.goals[0].started.is_none());
        assert!(!talks.turn_held(&talk.id));
        // A process/store restart retains receipts. Replaying the old wake
        // cannot invoke the backend again, even with no live in-memory owner.
        let reopened = crate::talk::Talks::at(temp.path().join("talks"));
        run_wake_with_config(&reopened, &talk.id, "deliver", 1, &cfg)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        task.hold_reason = Some("current head checks failed".into());
        queue.put(&mut task).unwrap();
        let event = observe(
            temp.path(),
            &talk.repo,
            std::slice::from_ref(&task.id),
            &[],
            &[],
        )
        .await;
        reopened
            .mutate(&talk.id, |talk| {
                let goal = &mut talk.goals[0];
                goal.state = crate::chat_goal::State::Waiting;
                goal.observe(event.fingerprint, event.facts.to_string(), false);
                Ok(())
            })
            .unwrap();
        run_wake_with_config(&reopened, &talk.id, "deliver", 2, &cfg)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
        let input = std::fs::read_to_string(prompt).unwrap();
        assert!(input.contains("Repair the existing patch without a second owner message"));
        assert!(input.contains("current head checks failed"));
        assert_eq!(reopened.get(&talk.id).unwrap().persona, "asuka");
        assert_eq!(reopened.get(&talk.id).unwrap().goals[0].handled, 2);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[tokio::test]
    async fn unchanged_wait_has_a_finite_probe_budget_without_agent_calls() {
        let temp = tempfile::tempdir().unwrap();
        let talks = crate::talk::Talks::at(temp.path().join("talks"));
        let mut talk = crate::talk::seeded_for_test("bounded-wait", temp.path().into());
        let goal = serde_json::from_value(json!({
            "id":"wait","purpose":"observe the existing job","scope":"one job",
            "condition":{"description":"owner reviews result","kind":"owner_review"},
            "state":"waiting","step":"waiting for evidence","source_turn":0,"owner_input":"follow this job",
            "tasks":[],"runs":[],"watches":[],"evidence":"","wake":0,"handled":0,
            "started":null,"wakes":0,"rechecks":240,"last_checked":null,"fingerprint":""
        })).unwrap();
        talk.goals.push(goal);
        talks.put(&mut talk).unwrap();
        reconcile(&talks, &talk.id, "wait").await.unwrap();
        let saved = talks.get(&talk.id).unwrap();
        assert_eq!(saved.goals[0].state, crate::chat_goal::State::Paused);
        assert_eq!(saved.goals[0].rechecks, 240);
        assert_eq!(saved.goals[0].wakes, 0);
        assert!(saved.turns.is_empty());
        assert!(crate::worker::active(temp.path()).unwrap().is_empty());
    }
}
