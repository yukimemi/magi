//! Local CLI ingress. Publication, TurnLease and worker ownership are the same
//! as web Chat; an uncertain submission is never replayed.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Provenance of a CLI admission; old receipts were all external.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Data from an external caller; never grants owner authority.
    #[default]
    External,
    /// An explicit local operator submission.
    Owner,
}

// Separate from the transcript so deleting a talk cannot free an owner
// request ID for use against another destination.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct OwnerBinding {
    talk: String,
    text_sha256: String,
}

fn digest(text: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(text.as_bytes()) {
        write!(encoded, "{byte:02x}").expect("write to String");
    }
    encoded
}

fn binding_path(store: &Talks, request: &str) -> PathBuf {
    store.root.join("owner-requests").join(digest(request))
}

fn read_binding(store: &Talks, request: &str) -> Result<Option<OwnerBinding>> {
    match std::fs::read(binding_path(store, request)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("decode owner request binding")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("read owner request binding"),
    }
}

fn save_binding(store: &Talks, request: &str, binding: &OwnerBinding) -> Result<()> {
    let path = binding_path(store, request);
    std::fs::create_dir_all(path.parent().expect("binding directory"))?;
    write_atomic(
        &path.with_extension("tmp"),
        &path,
        &serde_json::to_string(binding)?,
    )
    .context("persist owner request binding")
}

/// A durable occurrence, committed atomically with its exact transcript input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Caller-saved occurrence identifier.
    pub request: String,
    /// Authority comes from the admission path, not the sender label or text.
    #[serde(default)]
    pub kind: RequestKind,
    /// Self-reported external display label; never an authority credential.
    pub sender: String,
    /// Exact original input, retained to reject conflicting request replays.
    pub text: String,
    /// Exact appended input turn, not a search for the next operator turn.
    pub input_turn: usize,
    /// Exact response turn, committed with that response.
    pub reply_turn: Option<usize>,
    /// Durable execution boundary: accepted, started, replied, failed,
    /// not_started or uncertain. Transport timeouts never change this.
    pub status: String,
    /// Detached worker reservation that owns this occurrence.
    pub worker: Option<String>,
    /// Startup or response diagnostic, when known.
    pub error: Option<String>,
}

fn resolve(store: &Talks, selector: &str) -> Result<std::result::Result<Talk, Value>> {
    let (talks, unreadable) = store.list_counting_unreadable();
    let mut hits: Vec<_> = talks
        .into_iter()
        .filter(|t| t.id == selector || t.id.starts_with(selector))
        .collect();
    if let Some(index) = hits.iter().position(|t| t.id == selector) {
        return Ok(Ok(hits.swap_remove(index)));
    }
    Ok(match hits.len() {
        0 => Err(json!({"status":"not_found", "selector":selector, "unreadable":unreadable})),
        1 => Ok(hits.pop().expect("one match")),
        _ => Err(
            json!({"status":"ambiguous", "selector":selector, "candidates":hits.iter().map(|t| &t.id).collect::<Vec<_>>()}),
        ),
    })
}

/// Discover conversations without opening or reopening one.
pub fn list(store: &Talks) -> Result<Value> {
    let (talks, unreadable) = store.list_counting_unreadable();
    Ok(
        json!({"talks":talks.iter().map(|t| json!({"id":t.id,"repo":t.repo,"agent":t.agent,"status":t.status,"title":t.title})).collect::<Vec<_>>(),"unreadable":unreadable}),
    )
}

fn cursor(talk: &Talk, turn: usize, message: usize) -> String {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    if let Some(row) = talk.turns.get(turn) {
        hash.update(serde_json::to_vec(&row.at).expect("timestamp serialization"));
        let body = row
            .messages
            .get(message)
            .map_or(row.body.as_str(), |m| m.body.as_str());
        hash.update(body.as_bytes());
    }
    // A reverted admission may later reuse a coordinate. Bind the cursor to
    // its contents and occurrence time so it never silently aliases that row.
    use std::fmt::Write as _;
    let mut id = format!("{}:{turn}:{message}:", talk.id);
    id.reserve(64);
    for byte in hash.finalize() {
        write!(id, "{byte:02x}").expect("write to string");
    }
    id
}

/// Conversation-bound coordinates and occurrence fingerprints form stable IDs.
pub fn history(store: &Talks, selector: &str, after: Option<&str>, limit: usize) -> Result<Value> {
    anyhow::ensure!(
        (1..=500).contains(&limit),
        "limit must be between 1 and 500"
    );
    let talk = match resolve(store, selector)? {
        Ok(t) => t,
        Err(v) => return Ok(v),
    };
    let mut coordinates = talk
        .turns
        .iter()
        .enumerate()
        .flat_map(|(i, turn)| (0..turn.messages.len().max(1)).map(move |j| (i, j)));
    if let Some(after) = after {
        coordinates
            .find(|&(i, j)| cursor(&talk, i, j) == after)
            .context("cursor is not a message in this conversation")?;
    } else {
        let count: usize = talk.turns.iter().map(|t| t.messages.len().max(1)).sum();
        for _ in 0..count.saturating_sub(limit) {
            coordinates.next();
        }
    }
    let messages: Vec<_> = coordinates
        .by_ref()
        .take(limit)
        .map(|(i, j)| {
            let turn = &talk.turns[i];
            let message = turn.messages.get(j);
            let body = message.map_or(turn.body.as_str(), |m| m.body.as_str());
            let ingress = talk.external_requests.iter().find(|r| r.input_turn == i);
            json!({"id":cursor(&talk,i,j),"sender":sender(turn,message),"body":body,"at":turn.at,
                "request":ingress.map(|r| &r.request),"ingress":ingress.map(|r| r.kind)})
        })
        .collect();
    let more = coordinates.next().is_some();
    let last = messages.last().and_then(|v| v["id"].as_str()).or(after);
    Ok(json!({"talk":talk.id,"messages":messages,"cursor":last,"more":more}))
}

fn view(store: &Talks, talk: &Talk, record: &Request) -> Value {
    use crate::worker::Status;
    let mut status = record.status.as_str();
    let owner = record.kind == RequestKind::Owner;
    if matches!(status, "accepted" | "started") {
        // A missing or ended worker without a settled reply is not permission
        // to execute again. The owner path also probes the live execution lease.
        let live = record
            .worker
            .as_deref()
            .and_then(|id| crate::worker::get(store.home(), id).ok())
            .is_some_and(|job| {
                matches!(job.status, Status::Starting | Status::Running)
                    && (!owner
                        || crate::worker::observe_job_active(store.home(), &job).unwrap_or(false))
            });
        status = if !live {
            "uncertain"
        } else if owner && status == "accepted" {
            "queued"
        } else {
            status
        };
    }
    // A saved authorization (even a superseded one) is evidence of the
    // effect. Persona prose, task status and an unrelated goal are not.
    let mut goal_authorizations = Vec::new();
    if owner {
        for goal in &talk.goals {
            if goal.source_turn == record.input_turn && goal.owner_input == record.text {
                goal_authorizations.push(json!({"id":goal.id,"purpose":goal.purpose,
                    "scope":goal.scope,"condition":goal.condition,"current":true,"state":goal.state}));
            }
            for revision in &goal.history {
                if revision.source_turn == record.input_turn && revision.owner_input == record.text
                {
                    goal_authorizations.push(json!({"id":goal.id,"purpose":revision.purpose,
                        "scope":revision.scope,"condition":revision.condition,
                        "current":false,"state":revision.state}));
                }
            }
        }
    }
    json!({"talk":talk.id,"request":record.request,"status":status,"accepted":true,
        "ingress":record.kind,"goal_authorizations":goal_authorizations,
        "input_id":cursor(talk,record.input_turn,0),
        "reply_id":record.reply_turn.map(|i| cursor(talk,i,0)),
        "reply":record.reply_turn.and_then(|i| talk.turns.get(i)).map(|t| &t.body),
        "worker":record.worker,"error":record.error})
}

/// Accept once, or return the saved occurrence. Duplicate calls never dispatch.
pub async fn send(
    store: &Talks,
    selector: &str,
    request: &str,
    label: &str,
    text: &str,
) -> Result<Value> {
    send_input(store, selector, request, Some(label), text).await
}

/// Submit an explicit local operator request, independently of external ingress.
pub async fn send_owner(store: &Talks, id: &str, request: &str, text: &str) -> Result<Value> {
    let seat_environment = ["MAGI_RUN", "MAGI_NODE", "MAGI_SEAT"];
    anyhow::ensure!(
        !seat_environment
            .iter()
            .chain(crate::worker::INVOCATION_ENV.iter())
            .chain(crate::chat_control::bridge::ENVIRONMENT)
            .any(|key| std::env::var_os(key).is_some()),
        "send-owner is unavailable inside a magi seat or worker; run it from the owner's trusted local terminal using their private MAGI_HOME"
    );
    send_input(store, id, request, None, text).await
}

async fn send_input(
    store: &Talks,
    selector: &str,
    request: &str,
    label: Option<&str>,
    text: &str,
) -> Result<Value> {
    let kind = if label.is_some() {
        RequestKind::External
    } else {
        RequestKind::Owner
    };
    let label = label.unwrap_or_default();
    let owner = kind == RequestKind::Owner;
    anyhow::ensure!(
        !request.is_empty()
            && request.len() <= 128
            && request
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "request must contain 1..128 ASCII letters, digits, -, _ or ."
    );
    anyhow::ensure!(
        owner
            || (!label.trim().is_empty()
                && label.len() <= 128
                && !label.chars().any(char::is_control)),
        "sender must be a nonempty single-line label of at most 128 bytes"
    );
    anyhow::ensure!(!text.trim().is_empty(), "nothing to send");
    let expected_binding = owner.then(|| OwnerBinding {
        talk: selector.to_owned(),
        text_sha256: digest(text),
    });
    let binding_status = |binding: &OwnerBinding| {
        let status = if Some(binding) == expected_binding.as_ref() {
            "uncertain"
        } else {
            "conflict"
        };
        json!({"talk":selector,"request":request,"status":status})
    };
    let talk = match resolve(store, selector)? {
        Ok(t) => t,
        Err(v) => {
            return Ok(if owner {
                read_binding(store, request)?
                    .as_ref()
                    .map_or(v, binding_status)
            } else {
                v
            });
        }
    };
    anyhow::ensure!(
        !owner || talk.id == selector,
        "send-owner requires the full Chat ID from magi talk list"
    );
    let replay = |talk: &Talk, r: &Request| {
        if r.kind == kind && r.text == text && r.sender == label {
            view(store, talk, r)
        } else {
            json!({"talk":talk.id,"request":request,"status":"conflict"})
        }
    };
    if let Some(r) = talk.external_requests.iter().find(|r| r.request == request) {
        return Ok(replay(&talk, r));
    }
    if let Some(binding) = read_binding(store, request)? {
        // Re-read: the binding can become visible just before the atomic
        // transcript write. Never dispatch again in that interval.
        let fresh = read_path(&store.path_of(&talk.id))?;
        return Ok(fresh
            .external_requests
            .iter()
            .find(|r| r.request == request)
            .map_or_else(|| binding_status(&binding), |r| replay(&fresh, r)));
    }
    let Some(_publication) = crate::chat_worker::try_publication_guard(store, &talk.id)? else {
        return Ok(json!({"talk":talk.id,"request":request,"status":"busy"}));
    };
    // Replays are observational even if the conversation is now busy/closed.
    let fresh = read_path(&store.path_of(&talk.id))?;
    if let Some(r) = fresh
        .external_requests
        .iter()
        .find(|r| r.request == request)
    {
        return Ok(replay(&fresh, r));
    }
    if !fresh.status.open() {
        return Ok(json!({"talk":fresh.id,"request":request,"status":"closed"}));
    }
    // Refuse every live generation, including older executables unable to
    // decode external provenance, before publishing a new-format record.
    if crate::chat_worker::turn_worker_active(store, &fresh.id) {
        return Ok(json!({"talk":fresh.id,"request":request,"status":"busy"}));
    }
    let Some(_turn) = store.claim_turn(&fresh.id)? else {
        return Ok(json!({"talk":fresh.id,"request":request,"status":"busy"}));
    };
    {
        let _guard = store.guard()?;
        let mut fresh = read_path(&store.path_of(&talk.id))?;
        if let Some(binding) = read_binding(store, request)? {
            return Ok(binding_status(&binding));
        }
        if !fresh.status.open() {
            return Ok(json!({"talk":fresh.id,"request":request,"status":"closed"}));
        }
        if !fresh.pending.is_empty()
            || !fresh.pending_messages.is_empty()
            || !fresh.pending_attachments.is_empty()
        {
            return Ok(json!({"talk":fresh.id,"request":request,"status":"busy"}));
        }
        let index = fresh.turns.len();
        if let Some(binding) = &expected_binding {
            save_binding(store, request, binding)?;
            fresh.turns.push(operator_turn(text, Vec::new(), None));
        } else {
            fresh.turns.push(Turn {
                sender: Some(Sender::External),
                who: Who::Operator,
                body: format!("External agent [{label}]:\n{text}"),
                at: Timestamp::now(),
                attachments: Vec::new(),
                usage: None,
                breaks: Some(Vec::new()),
                messages: Vec::new(),
                posted_by: None,
            });
        }
        fresh.external_requests.push(Request {
            request: request.to_owned(),
            kind,
            sender: label.to_owned(),
            text: text.to_owned(),
            input_turn: index,
            reply_turn: None,
            status: "accepted".into(),
            worker: None,
            error: None,
        });
        store.put(&mut fresh)?;
    }
    // The ledger is already durable. A crash at any subsequent boundary is an
    // accepted but uncertain send, never an invitation to append or run again.
    let result = crate::worker::spawn(
        store.home(),
        "chat",
        &format!("chat-external:{}:{request}", talk.id),
        json!({"talk":talk.id,"external_request":request}),
    )
    .await;
    {
        let _guard = store.guard()?;
        let mut fresh = read_path(&store.path_of(&talk.id))?;
        let r = fresh
            .external_requests
            .iter_mut()
            .find(|r| r.request == request)
            .context("CLI request disappeared")?;
        match result {
            Ok(job) => {
                r.worker = Some(job.id);
            }
            Err(error) => {
                if r.status == "accepted" {
                    r.status = if crate::worker::not_started(&error) {
                        "not_started"
                    } else {
                        "uncertain"
                    }
                    .into();
                }
                r.error = Some(format!("{error:#}"));
            }
        }
        store.put(&mut fresh)?;
    }
    let fresh = read_path(&store.path_of(&talk.id))?;
    Ok(view(
        store,
        &fresh,
        fresh
            .external_requests
            .iter()
            .find(|r| r.request == request)
            .context("CLI request disappeared")?,
    ))
}

/// Observation only: timeout neither cancels nor repeats accepted work.
pub async fn reply(store: &Talks, selector: &str, request: &str, wait: u64) -> Result<Value> {
    let binding = read_binding(store, request)?;
    let talk = match resolve(store, selector)? {
        Ok(t) => t,
        Err(v) => {
            return Ok(if binding.as_ref().is_some_and(|b| b.talk == selector) {
                json!({"talk":selector,"request":request,"status":"uncertain"})
            } else {
                v
            });
        }
    };
    let deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(wait))
        .context("wait duration exceeds the monotonic clock range")?;
    loop {
        let fresh = read_path(&store.path_of(&talk.id))?;
        let Some(record) = fresh
            .external_requests
            .iter()
            .find(|r| r.request == request)
        else {
            let status = match &binding {
                Some(b) if b.talk == fresh.id => "uncertain",
                Some(_) => "conflict",
                None => "not_found",
            };
            return Ok(json!({"talk":talk.id,"request":request,"status":status}));
        };
        let mut value = view(store, &fresh, record);
        if wait == 0 || matches!(record.status.as_str(), "replied" | "failed" | "not_started") {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            value["state"] = value["status"].clone();
            value["status"] = json!("timeout");
            return Ok(value);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Called only by the ordinary detached chat worker, under its execution lease.
pub(crate) async fn run_worker(store: &Talks, id: &str, request: &str) -> Result<()> {
    let job = crate::worker::current_job(store.home())?.context("CLI submission has no worker")?;
    anyhow::ensure!(
        job.kind == "chat"
            && job.payload["talk"].as_str() == Some(id)
            && job.payload["external_request"].as_str() == Some(request),
        "CLI submission does not belong to this worker"
    );
    loop {
        if let Some(lease) = store.claim_turn(id)? {
            let publication = crate::chat_worker::publication_guard(store, id).await?;
            let (mut talk, index) = {
                let _guard = store.guard()?;
                let mut talk = read_path(&store.path_of(id))?;
                let r = talk
                    .external_requests
                    .iter_mut()
                    .find(|r| r.request == request)
                    .context("external request missing")?;
                // A second worker or uncertain restart cannot repeat a CLI call.
                if r.status != "accepted" {
                    return Ok(());
                }
                if r.kind == RequestKind::Owner && !talk.status.open() {
                    r.status = "not_started".into();
                    r.error = Some("conversation closed before execution".into());
                    store.put(&mut talk)?;
                    return Ok(());
                }
                anyhow::ensure!(
                    r.worker.as_deref().is_none_or(|worker| worker == job.id),
                    "CLI submission belongs to a different worker"
                );
                if r.kind == RequestKind::Owner {
                    let turn = talk
                        .turns
                        .get(r.input_turn)
                        .context("owner input is missing")?;
                    let binding = OwnerBinding {
                        talk: id.to_owned(),
                        text_sha256: digest(&r.text),
                    };
                    anyhow::ensure!(
                        read_binding(store, request)?.as_ref() == Some(&binding)
                            && turn.who == Who::Operator
                            && turn.sender == Some(Sender::Operator)
                            && turn.body == r.text
                            && turn.messages.iter().all(|message| {
                                message.sender == Some(Sender::Operator) && message.body == r.text
                            }),
                        "saved owner input does not match its admission"
                    );
                } else {
                    anyhow::ensure!(
                        talk.turns
                            .get(r.input_turn)
                            .is_some_and(|turn| turn.sender == Some(Sender::External)),
                        "saved external input has no external provenance"
                    );
                }
                r.status = "started".into();
                r.worker = Some(job.id.clone());
                let index = r.input_turn;
                store.put(&mut talk)?;
                (talk, index)
            };
            let text = talk
                .turns
                .get(index)
                .context("recorded CLI input is missing")?
                .body
                .clone();
            drop(publication);
            let (cfg, _) = Config::discover(&talk.repo, None)?;
            return super::respond_at(&lease, &mut talk, store, &cfg, &text, Some(index)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
