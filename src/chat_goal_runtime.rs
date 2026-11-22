//! Finite, evidence-triggered continuation of accepted conversation goals.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::chat_goal::Goal;

/// A current observation is data, never a new owner instruction.
#[derive(Debug)]
struct Observation {
    pub facts: Value,
    repo: PathBuf,
    artifacts: Vec<(usize, String, ArtifactHead)>,
}

impl Observation {
    fn apply(mut self, goal: &mut Goal) {
        let complete = satisfied(goal, &self.facts);
        let evidence = self.facts.to_string();

        // Keep provenance inside the existing string so older builds can read the talk.
        // The first line is the comparison; the optional second line binds its heads.
        let (previous, bindings) = goal
            .fingerprint
            .split_once('\n')
            .map_or((goal.fingerprint.as_str(), None), |(facts, bindings)| {
                (facts, Some(bindings))
            });
        let bound = bindings.and_then(|data| {
            serde_json::from_str::<(PathBuf, BTreeMap<String, String>)>(data).ok()
        });
        // Legacy records lack ref provenance. Use their last comparison only
        // for the same run, then bind it to the first observed target.
        let legacy = bindings.is_none() && previous == goal.evidence;
        let mut prior_heads = BTreeMap::new();
        if self
            .artifacts
            .iter()
            .any(|(_, _, head)| *head == ArtifactHead::Unknown)
            && (legacy || bound.as_ref().is_some_and(|(repo, _)| *repo == self.repo))
            && let Ok(mut facts) = serde_json::from_str::<Value>(previous)
            && let Some(runs) = facts["runs"].as_array_mut()
        {
            for run in runs {
                if let Some(id) = run["id"].as_str()
                    && let Some(head) = run["artifact_head"].as_str()
                    && matches!(head.len(), 40 | 64)
                    && head.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    prior_heads.insert(id.to_owned(), head.to_owned());
                }
            }
        }

        let mut targets = BTreeMap::new();
        let mut substituted = false;
        let runs = self.facts["runs"].as_array_mut().expect("observed runs");
        for (index, branch, observed) in self.artifacts {
            let run = &mut runs[index];
            let id = run["id"].as_str().expect("observed run id").to_owned();
            match observed {
                ArtifactHead::Known(_) => {}
                ArtifactHead::Unknown
                    if legacy
                        || bound.as_ref().is_some_and(|(repo, branches)| {
                            *repo == self.repo && branches.get(&id) == Some(&branch)
                        }) =>
                {
                    let Some(head) = prior_heads.remove(&id) else {
                        continue;
                    };
                    run["artifact_head"] = Value::String(head);
                    substituted = true;
                }
                ArtifactHead::Unknown | ArtifactHead::Absent => continue,
            }
            targets.insert(id, branch);
        }
        let comparison = if substituted {
            self.facts.to_string()
        } else {
            evidence.clone()
        };
        if let Some((facts, _)) = goal.fingerprint.split_once('\n') {
            goal.fingerprint.truncate(facts.len());
        }
        goal.observe(comparison, evidence, complete);
        if !targets.is_empty() {
            goal.fingerprint.push('\n');
            goal.fingerprint
                .push_str(&json!([self.repo, targets]).to_string());
        }
    }
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

const ARTIFACT_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum ArtifactHead {
    Known(String),
    #[default]
    Unknown,
    Absent,
}

async fn git_artifact_head(repo: &Path, branch: &str) -> ArtifactHead {
    if branch.starts_with('-') {
        return ArtifactHead::Unknown;
    }
    let candidate = format!("refs/heads/{branch}");
    if let Ok(reference) =
        crate::git::git_raw(repo, &["show-ref", "--verify", "--hash", "--", &candidate]).await
        && reference.ok()
    {
        let revision = format!("{}^{{commit}}", reference.stdout);
        return match crate::git::git_raw(
            repo,
            &["rev-parse", "--verify", "--end-of-options", &revision],
        )
        .await
        {
            Ok(output) if output.ok() => ArtifactHead::Known(output.stdout),
            _ => ArtifactHead::Unknown,
        };
    }
    // --exists distinguishes a missing ref (2) from an unreadable one (1).
    // Git can also return 2 for a malformed name, so validate before clearing a baseline.
    if crate::git::git_raw(repo, &["show-ref", "--exists", &candidate])
        .await
        .is_ok_and(|out| out.code == Some(2))
        && crate::git::git_raw(repo, &["check-ref-format", &candidate])
            .await
            .is_ok_and(|out| out.ok())
    {
        ArtifactHead::Absent
    } else {
        ArtifactHead::Unknown
    }
}

async fn bounded_artifact_head(
    timeout: std::time::Duration,
    probe: impl Future<Output = ArtifactHead>,
) -> ArtifactHead {
    tokio::time::timeout(timeout, probe)
        .await
        .unwrap_or_default()
}

trait EvidenceObserver: Sync {
    fn current_pr(&self, repo: &Path, url: &str) -> impl Future<Output = Result<Value>> + Send;
    fn artifact_head(&self, repo: &Path, branch: &str)
    -> impl Future<Output = ArtifactHead> + Send;
}

struct LiveObserver;

impl EvidenceObserver for LiveObserver {
    async fn current_pr(&self, repo: &Path, url: &str) -> Result<Value> {
        crate::chat_notify::current_pr(repo, url).await
    }

    async fn artifact_head(&self, repo: &Path, branch: &str) -> ArtifactHead {
        bounded_artifact_head(ARTIFACT_HEAD_TIMEOUT, git_artifact_head(repo, branch)).await
    }
}

/// Local task/run evidence plus one timed forge lookup per distinct associated PR.
/// Timestamps and transcript prose are excluded from the wake fingerprint.
#[cfg(test)]
async fn observe(
    home: &Path,
    repo: &Path,
    tasks: &[String],
    runs: &[String],
    watches: &[String],
) -> Observation {
    observe_with(home, repo, tasks, runs, watches, &LiveObserver).await
}

async fn observe_with(
    home: &Path,
    repo: &Path,
    tasks: &[String],
    runs: &[String],
    watches: &[String],
    observer: &impl EvidenceObserver,
) -> Observation {
    let mut task_facts = Vec::new();
    let mut run_ids = Vec::new();
    let mut historical_runs = std::collections::BTreeSet::new();
    let queue = crate::queue::Queue::at(home.join("queue"));
    for id in tasks {
        match crate::chat_goal::task_record(&queue, id, repo) {
            Ok(task) => {
                if let Some(run) = task.execution_runs().last()
                    && !run_ids.contains(run)
                {
                    run_ids.push(run.clone());
                }
                task_facts.push(json!({"id": task.id, "status":task.status,
                    "reason":task.hold_reason, "runs":task.execution_runs(),"current_run":task.execution_runs().last()}));
                historical_runs.extend(task.runs);
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
    let mut artifacts = Vec::new();
    let mut pr_observations = std::collections::BTreeMap::new();
    let mut prs = Vec::new();
    for id in &run_ids {
        if !crate::chat_goal::valid_reference(id) {
            run_facts.push(json!({"id":id,"evidence":{"unavailable":"invalid run identity"}}));
            continue;
        }
        let record = read_json(&home.join("runs").join(id).join("run.json")).and_then(|run| {
            anyhow::ensure!(
                run["id"].as_str() == Some(id),
                "goal run record identity does not match {id}"
            );
            let path = run["repo"]
                .as_str()
                .with_context(|| format!("goal run {id} has no repository"))?;
            crate::chat_goal::require_repo("run", id, Path::new(path), repo)?;
            Ok(run)
        });
        match record {
            Ok(run) => {
                let winner = run["candidates"].as_array().and_then(|candidates| {
                    candidates
                        .iter()
                        .find(|candidate| candidate["label"] == run["tally"]["winner"])
                });
                let branch = winner.and_then(|candidate| candidate["branch"].as_str());
                let artifact_head = match branch {
                    Some(branch) => {
                        let observed = observer.artifact_head(repo, branch).await;
                        let head = match &observed {
                            ArtifactHead::Known(head) => json!(head),
                            ArtifactHead::Unknown | ArtifactHead::Absent => Value::Null,
                        };
                        artifacts.push((run_facts.len(), branch.to_owned(), observed));
                        head
                    }
                    None => Value::Null,
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
                    (task_facts
                        .iter()
                        .any(|task| task["id"] == inquiry.task && task.get("status").is_some())
                        || inquiry.run.as_ref().is_some_and(|id| {
                            run_facts
                                .iter()
                                .any(|run| run["id"] == *id && run.get("status").is_some())
                        }))
                        && crate::chat_goal::require_repo(
                            "inquiry",
                            &inquiry.id,
                            &inquiry.repo,
                            repo,
                        )
                        .is_ok()
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
    Observation {
        facts,
        repo: repo.to_owned(),
        artifacts,
    }
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
    if let Some(generation) = reconcile_with(talks, chat, id, &LiveObserver).await? {
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

async fn reconcile_with(
    talks: &crate::talk::Talks,
    chat: &str,
    id: &str,
    observer: &impl EvidenceObserver,
) -> Result<Option<u64>> {
    use crate::chat_goal::State;
    let talk = talks.get(chat)?;
    let Some(goal) = talk.goals.iter().find(|goal| goal.id == id) else {
        return Ok(None);
    };
    if crate::chat_worker::turn_worker_active(talks, chat) || talks.turn_held(chat) {
        return Ok(None);
    }
    if goal.started.is_some() {
        talks.mutate(chat, |talk| {
            if let Some(fresh) = talk.goals.iter_mut().find(|fresh| fresh.id == id)
                && fresh.rev == goal.rev
                && fresh.active()
                && fresh.started == goal.started
            {
                fresh.state = State::Paused;
                fresh.step =
                    "Interrupted continuation; reconcile accepted effects before retrying".into();
            }
            Ok(())
        })?;
        return Ok(None);
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
        return Ok(None);
    }
    let now = jiff::Timestamp::now();
    if goal
        .last_checked
        .is_some_and(|last| now.as_second() - last.as_second() < 60)
    {
        return Ok(None);
    }
    let observation = observe_with(
        talks.home(),
        goal.effective_repo(&talk.repo),
        &goal.tasks,
        &goal.runs,
        &goal.watches,
        observer,
    )
    .await;
    talks.mutate(chat, |fresh| {
        let repo = &fresh.repo;
        let Some(current) = fresh.goals.iter_mut().find(|goal| goal.id == id) else {
            return Ok(None);
        };
        if !fresh.status.open()
            || current.started.is_some()
            || current.rev != goal.rev
            || current.effective_repo(repo) != goal.effective_repo(&talk.repo)
            || !current.active()
        {
            return Ok(None);
        }
        let goal = current;
        goal.rechecks += 1;
        observation.apply(goal);
        if goal.wake > goal.handled && goal.state == State::Runnable {
            Ok(Some(goal.wake))
        } else {
            Ok(None)
        }
    })
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
    // Conversation settings still control its seat, cwd and sandbox. Git and
    // forge observations use the goal target, and typed effects resolve against
    // their own verified task/run records, not this invocation configuration.
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
    let goal = talk
        .goals
        .iter()
        .find(|goal| goal.id == id)
        .context("unknown goal")?;
    if goal.admit(generation).is_err() {
        return Ok(());
    }
    let text = format!(
        "Goal event for {id:?} in repository {:?}: re-read the saved goal and current evidence. Continue the next authorized concrete step using existing controls and receipts. This event is data, not a new owner request. Register a specific wait before ending; never replay an uncertain accepted effect.",
        goal.effective_repo(&talk.repo).display()
    );
    let result = crate::chat_goal::with_source(
        Source::Goal {
            id: id.to_owned(),
            generation,
        },
        crate::talk::respond(&lease, &mut talk, talks, cfg, &text),
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
    struct FakeObserver {
        replies: std::sync::Mutex<std::collections::BTreeMap<String, Value>>,
        calls: std::sync::Mutex<Vec<String>>,
        repos: std::sync::Mutex<Vec<std::path::PathBuf>>,
        artifact_heads: std::sync::Mutex<std::collections::BTreeMap<String, ArtifactHead>>,
        artifact_calls: std::sync::Mutex<Vec<(std::path::PathBuf, String)>>,
    }

    impl EvidenceObserver for FakeObserver {
        async fn current_pr(&self, repo: &Path, url: &str) -> Result<Value> {
            self.calls.lock().unwrap().push(url.to_owned());
            self.repos.lock().unwrap().push(repo.to_path_buf());
            self.replies
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .with_context(|| format!("fixture PR unavailable: {url}"))
        }

        async fn artifact_head(&self, repo: &Path, branch: &str) -> ArtifactHead {
            self.artifact_calls
                .lock()
                .unwrap()
                .push((repo.to_path_buf(), branch.to_owned()));
            let observed = self
                .artifact_heads
                .lock()
                .unwrap()
                .get(branch)
                .cloned()
                .unwrap_or_default();
            if observed == ArtifactHead::Unknown {
                bounded_artifact_head(std::time::Duration::ZERO, std::future::pending()).await
            } else {
                observed
            }
        }
    }

    #[tokio::test]
    async fn waiting_goal_ignores_artifact_timeouts_but_requires_fresh_head_for_completion() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let talks_path = home.join("talks");
        let talks = crate::talk::Talks::at(talks_path.clone());
        let chat = "artifact-wait";
        let mut talk = crate::talk::seeded_for_test(chat, home.into());
        let mut task = crate::queue::Task::new(
            "Resume the retained run".into(),
            "Use the retained patch".into(),
            home.into(),
            crate::queue::Source::Human,
        );
        task.runs.push("run".into());
        let queue = crate::queue::Queue::at(home.join("queue"));
        queue.put(&mut task).unwrap();

        let mut goal = goal();
        goal.tasks.push(task.id.clone());
        goal.condition.kind = Completion::TasksDone;
        let run_dir = home.join("runs/run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let run_path = run_dir.join("run.json");
        let branch = "magi/retained";
        let mut run = json!({
            "id":"run","repo":home,"status":"blocked",
            "tally":{"winner":"A"},"candidates":[{"label":"A","branch":branch}]
        });
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let known_a = ArtifactHead::Known(a.clone());
        let known_b = ArtifactHead::Known(b.clone());
        let observer = FakeObserver::default();
        std::fs::write(&run_path, run.to_string()).unwrap();
        observer
            .artifact_heads
            .lock()
            .unwrap()
            .insert(branch.into(), known_a.clone());
        let legacy = observe_with(home, home, &goal.tasks, &goal.runs, &[], &observer).await;
        // The previous build persisted exactly the raw facts as both strings.
        goal.evidence = legacy.facts.to_string();
        goal.fingerprint = goal.evidence.clone();
        goal.wake = 1;
        goal.handled = 1;
        goal.wakes = 1;
        let legacy_goal = serde_json::to_value(&goal).unwrap();
        let legacy_fields = legacy_goal.as_object().unwrap();
        talk.goals.push(goal);
        talks.put(&mut talk).unwrap();

        let mut wake = 1;
        for (head, gate_passed, expected, state) in [
            (&ArtifactHead::Unknown, false, None, State::Waiting),
            (&ArtifactHead::Unknown, false, None, State::Waiting),
            (&known_a, false, None, State::Waiting),
            (&known_b, false, Some(2), State::Runnable),
            (&known_b, false, None, State::Waiting),
            (&ArtifactHead::Absent, false, Some(3), State::Runnable),
            (&ArtifactHead::Unknown, false, None, State::Waiting),
            (&known_b, false, Some(4), State::Runnable),
            (&ArtifactHead::Unknown, true, Some(5), State::Runnable),
            (&ArtifactHead::Unknown, true, None, State::Waiting),
            (&known_b, true, None, State::Completed),
        ] {
            if gate_passed {
                run["gate_ran"] = json!(true);
                run["gate_head"] = json!(b);
                run["gate"] = json!([{"code":0}]);
            }
            std::fs::write(&run_path, run.to_string()).unwrap();
            observer
                .artifact_heads
                .lock()
                .unwrap()
                .insert(branch.into(), head.clone());
            let reopened = crate::talk::Talks::at(talks_path.clone());
            assert_eq!(
                reconcile_with(&reopened, chat, "goal", &observer)
                    .await
                    .unwrap(),
                expected,
                "{head:?}, gate_passed={gate_passed}"
            );
            let saved = reopened.get(chat).unwrap();
            let observed = &saved.goals[0];
            let encoded = serde_json::to_value(observed).unwrap();
            assert_eq!(
                encoded.as_object().unwrap().keys().collect::<Vec<_>>(),
                legacy_fields.keys().collect::<Vec<_>>(),
                "a strict legacy Goal decoder must not encounter new fields"
            );
            assert!(encoded["fingerprint"].is_string());
            assert_eq!(observed.state, state);
            let evidence: Value = serde_json::from_str(&observed.evidence).unwrap();
            assert_eq!(evidence["tasks"][0]["status"], "queued");
            assert_eq!(evidence["runs"][0]["status"], "blocked");
            match head {
                ArtifactHead::Known(head) => {
                    assert_eq!(evidence["runs"][0]["artifact_head"], *head);
                }
                _ => assert!(evidence["runs"][0]["artifact_head"].is_null()),
            }
            if *head == ArtifactHead::Unknown && !gate_passed {
                let prompt = crate::chat_goal::prompt(&saved);
                assert!(!prompt.contains(&a));
                assert!(!prompt.contains(&b));
            }
            assert_eq!(observed.wakes, wake);
            assert!(saved.turns.is_empty());
            if gate_passed && state == State::Completed {
                assert_eq!(observed.wake, u64::from(wake) + 1);
                break;
            }
            assert_eq!(observed.wake, expected.unwrap_or(u64::from(wake)));
            reopened
                .mutate(chat, |talk| {
                    let goal = &mut talk.goals[0];
                    goal.last_checked = None;
                    if let Some(generation) = expected {
                        // Settle the scheduled turn without invoking an agent.
                        wake += 1;
                        goal.handled = generation;
                        goal.wakes = wake;
                        goal.state = State::Waiting;
                    }
                    Ok(())
                })
                .unwrap();
        }
    }

    impl FakeObserver {
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

    fn pr_goal_fixture(home: &Path) -> (Goal, FakeObserver, Vec<String>) {
        let mut goal = goal();
        goal.runs.clear();
        let observer = FakeObserver::default();
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
                json!({"id":run,"repo":home,"pr":{"url":url},"pr_pushed_head":head}).to_string(),
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
            json!({"id":"run-shared","repo":home,"pr":{"url":urls[4]},"pr_pushed_head":format!("{:040x}",5)}).to_string(),
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
        let pending = observe_with(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(!satisfied(&goal, &pending.facts));
        pending.apply(&mut goal);
        observer.assert_one_lookup_per_pr(&urls);

        observer.replies.lock().unwrap().insert(
            urls[4].clone(),
            json!({"state":"OPEN","head":head,"merged_at":null,"checks":[{"verdict":"failed"}]}),
        );
        let failed = observe_with(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(!satisfied(&goal, &failed.facts));
        failed.apply(&mut goal);
        assert_eq!(
            goal.wake, 2,
            "CI changes beyond the first four PRs must wake the goal"
        );
        observer.assert_one_lookup_per_pr(&urls);

        observer.replies.lock().unwrap().insert(urls[4].clone(),
            json!({"state":"MERGED","head":format!("{:040x}",99),"merged_at":"2026-01-01T00:00:00Z"}));
        let moved = observe_with(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(
            !satisfied(&goal, &moved.facts),
            "the fifth PR must match its saved head too"
        );
        observer.assert_one_lookup_per_pr(&urls);

        observer.replies.lock().unwrap().insert(
            urls[4].clone(),
            json!({"state":"MERGED","head":head,"merged_at":"2026-01-01T00:00:00Z"}),
        );
        let merged = observe_with(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(
            satisfied(&goal, &merged.facts),
            "all six run associations must have exact-head merge evidence"
        );
        merged.apply(&mut goal);
        assert_eq!(goal.state, State::Completed);
        observer.assert_one_lookup_per_pr(&urls);
    }

    #[tokio::test]
    async fn unavailable_pull_request_does_not_hide_later_ci_or_reuse_old_merge_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let (goal, observer, urls) = pr_goal_fixture(temp.path());
        let merged = observe_with(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(satisfied(&goal, &merged.facts));
        observer.assert_one_lookup_per_pr(&urls);
        observer.replies.lock().unwrap().remove(&urls[0]);
        observer.replies.lock().unwrap().insert(urls[4].clone(),
            json!({"state":"OPEN","head":format!("{:040x}",5),"merged_at":null,"checks":[{"verdict":"failed"}]}));
        let current = observe_with(temp.path(), temp.path(), &[], &goal.runs, &[], &observer).await;
        assert!(
            !satisfied(&goal, &current.facts),
            "previously merged evidence cannot replace a failed fresh lookup"
        );
        assert_ne!(merged.facts, current.facts);
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

    #[tokio::test]
    async fn restarted_observer_uses_the_saved_checkout_and_rejects_foreign_runs() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let checkout = home.join("target");
        let other = home.join("other");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::create_dir(&other).unwrap();
        crate::git::git(&checkout, &["init", "-q"]).await.unwrap();
        crate::git::git(&other, &["init", "-q"]).await.unwrap();
        let subdir = checkout.join("src");
        std::fs::create_dir(&subdir).unwrap();
        let queue = crate::queue::Queue::at(home.join("queue"));
        let talks = crate::talk::Talks::at(home.join("talks"));
        let mut talk = crate::talk::seeded_for_test("chat", home.into());
        let make_run = |repo: &Path, number: u64| {
            let mut run = crate::run::RunState::new(
                repo.into(),
                "main".into(),
                "head".into(),
                "verify".into(),
                crate::config::Config::default(),
            );
            let url = format!("https://example.invalid/pull/{number}");
            run.pr_pushed_head = Some("head".into());
            run.pr = Some(crate::run::PrRecord {
                url,
                number,
                state: "open".into(),
                checks: "green".into(),
                round: 1,
                rounds: 1,
                red_at_merge: vec![],
            });
            run.reserve_under(home).unwrap();
            run
        };
        let previous = make_run(&checkout, 3);
        let target = make_run(&checkout, 1);
        let foreign = make_run(&other, 2);
        let mut task = crate::queue::Task::new(
            "Work".into(),
            "verify".into(),
            subdir.clone(),
            crate::queue::Source::Human,
        );
        task.runs.push(previous.id.clone());
        task.runs.push(target.id.clone());
        queue.put(&mut task).unwrap();
        let mut retained = goal();
        retained.repo = Some(checkout.canonicalize().unwrap());
        retained.tasks = vec![task.id];
        retained.runs = vec![previous.id];
        let observer = FakeObserver::default();
        let url = &target.pr.as_ref().unwrap().url;
        observer
            .replies
            .lock()
            .unwrap()
            .insert(url.clone(), json!({"state":"OPEN","head":"head"}));
        let repo = retained.effective_repo(&talk.repo);
        let mixed = observe_with(
            home,
            repo,
            &retained.tasks,
            &[retained.runs[0].clone(), foreign.id.clone()],
            &[],
            &observer,
        )
        .await;
        assert!(
            mixed.facts["runs"][1]["evidence"]["unavailable"]
                .as_str()
                .unwrap()
                .contains(&foreign.id)
        );
        assert_one_target_lookup(&observer, repo, url);
        // Legacy conversations may have stored the filing subdirectory as cwd.
        let pending = observe_with(
            home,
            &subdir,
            &retained.tasks,
            &retained.runs,
            &[],
            &observer,
        )
        .await;
        assert_eq!(pending.facts["prs"][0]["run"], target.id);
        assert!(!satisfied(&retained, &pending.facts));
        assert_one_target_lookup(&observer, &subdir, url);
        retained.evidence = pending.facts.to_string();
        retained.fingerprint = retained.evidence.clone();
        talk.goals.push(retained);
        talks.put(&mut talk).unwrap();

        observer.replies.lock().unwrap().insert(
            url.clone(),
            json!({"state":"MERGED","head":"head","merged_at":"now"}),
        );
        let reopened = crate::talk::Talks::at(home.join("talks"));
        assert_eq!(
            reconcile_with(&reopened, &talk.id, "goal", &observer)
                .await
                .unwrap(),
            None
        );
        let again = reopened.get(&talk.id).unwrap();
        assert_eq!(again.goals[0].state, State::Completed);
        assert_one_target_lookup(&observer, again.goals[0].effective_repo(&again.repo), url);
        assert_eq!(again.repo, home);
    }

    fn assert_one_target_lookup(observer: &FakeObserver, repo: &Path, url: &str) {
        observer.assert_one_lookup_per_pr(&[url.into()]);
        let repos = std::mem::take(&mut *observer.repos.lock().unwrap());
        assert_eq!(repos, [repo]);
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
        let queue = crate::queue::Queue::at(home.join("queue"));
        let observer = FakeObserver::default();
        let mut goal = goal();
        goal.condition.kind = Completion::TasksDone;
        goal.tasks.clear();
        goal.runs.clear();
        let mut expected_calls = Vec::new();
        for index in 0..32 {
            let run = format!("run-{index}");
            let branch = format!("magi/task-{index}");
            let head = format!("{:040x}", index + 1);
            let path = home.join("runs").join(&run);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("run.json"),
                json!({
                    "id":run,"repo":repo,
                    "gate_ran":true,"gate_head":head,"gate":[{"code":0}],
                    "tally":{"winner":"A"},"candidates":[{"label":"A","branch":branch}]
                })
                .to_string(),
            )
            .unwrap();
            observer
                .artifact_heads
                .lock()
                .unwrap()
                .insert(branch.clone(), ArtifactHead::Known(head));
            expected_calls.push((repo.clone(), branch));
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
        let observation = observe_with(&home, &repo, &goal.tasks, &goal.runs, &[], &observer).await;
        assert_eq!(
            std::mem::take(&mut *observer.artifact_calls.lock().unwrap()),
            expected_calls
        );
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

        // With only the 32 valid tasks and runs, a single unavailable artifact
        // head at the tail must still reject completion.
        goal.tasks.pop();
        goal.runs.pop();
        let verified = observe_with(&home, &repo, &goal.tasks, &goal.runs, &[], &observer).await;
        assert!(satisfied(&goal, &verified.facts));
        observer
            .artifact_heads
            .lock()
            .unwrap()
            .insert("magi/task-31".into(), ArtifactHead::Unknown);
        let unavailable_tail =
            observe_with(&home, &repo, &goal.tasks, &goal.runs, &[], &observer).await;
        assert!(unavailable_tail.facts["runs"][31]["artifact_head"].is_null());
        assert!(
            !satisfied(&goal, &unavailable_tail.facts),
            "an unavailable artifact head must reject completion even without unstarted tasks or missing runs"
        );
    }

    #[tokio::test]
    async fn timed_out_artifact_head_probe_becomes_null_and_blocks_completion() {
        assert_eq!(
            bounded_artifact_head(
                ARTIFACT_HEAD_TIMEOUT,
                std::future::ready(ArtifactHead::Known("abc".into())),
            )
            .await,
            ArtifactHead::Known("abc".into())
        );
        assert_eq!(
            bounded_artifact_head(std::time::Duration::ZERO, std::future::pending()).await,
            ArtifactHead::Unknown
        );

        struct TimedArtifactObserver {
            fast_branch: String,
            fast_head: String,
        }

        impl EvidenceObserver for TimedArtifactObserver {
            async fn current_pr(&self, _repo: &Path, url: &str) -> Result<Value> {
                anyhow::bail!("unexpected PR lookup: {url}")
            }

            async fn artifact_head(&self, _repo: &Path, branch: &str) -> ArtifactHead {
                if branch == self.fast_branch {
                    bounded_artifact_head(
                        ARTIFACT_HEAD_TIMEOUT,
                        std::future::ready(ArtifactHead::Known(self.fast_head.clone())),
                    )
                    .await
                } else {
                    bounded_artifact_head(std::time::Duration::ZERO, std::future::pending()).await
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let queue = crate::queue::Queue::at(home.join("queue"));
        let mut goal = goal();
        goal.condition.kind = Completion::TasksDone;
        goal.tasks.clear();
        goal.runs.clear();
        for (index, branch) in ["magi/fast", "magi/timed-out"].into_iter().enumerate() {
            let run = format!("run-{index}");
            let head = format!("{:040x}", index + 1);
            let path = home.join("runs").join(&run);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("run.json"),
                json!({
                    "id":run,"repo":repo,
                    "gate_ran":true,"gate_head":head,"gate":[{"code":0}],
                    "tally":{"winner":"A"},"candidates":[{"label":"A","branch":branch}]
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
        let observer = TimedArtifactObserver {
            fast_branch: "magi/fast".into(),
            fast_head: format!("{:040x}", 1),
        };
        let observation = observe_with(&home, &repo, &goal.tasks, &goal.runs, &[], &observer).await;
        assert_eq!(
            observation.facts["runs"][0]["artifact_head"],
            format!("{:040x}", 1)
        );
        assert!(observation.facts["runs"][1]["artifact_head"].is_null());
        assert!(
            !satisfied(&goal, &observation.facts),
            "a timed-out artifact probe must become null and prevent completion"
        );
    }

    #[tokio::test]
    async fn git_artifact_head_resolves_existing_ref_and_rejects_missing_or_option_like_ref() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "--initial-branch=magi/delivered"],
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
            crate::git::git(&repo, &args).await.unwrap();
        }
        let head = crate::git::rev_parse(&repo, "HEAD").await.unwrap();
        assert_eq!(
            git_artifact_head(&repo, "magi/delivered").await,
            ArtifactHead::Known(head)
        );
        crate::git::git(&repo, &["-c", "tag.gpgSign=false", "tag", "magi/missing"])
            .await
            .unwrap();
        assert_eq!(
            git_artifact_head(&repo, "magi/missing").await,
            ArtifactHead::Absent
        );
        for (repo, branch) in [
            (repo.as_path(), "--help"),
            (repo.as_path(), "invalid..name"),
            (temp.path(), "magi/missing"),
        ] {
            assert_eq!(git_artifact_head(repo, branch).await, ArtifactHead::Unknown);
        }
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
        event.apply(&mut goal);
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
                event.apply(goal);
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
