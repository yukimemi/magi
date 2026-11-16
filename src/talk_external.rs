//! Local external-process ingress. Publication, TurnLease and worker ownership
//! are the same as web Chat; an uncertain submission is never replayed.
use super::*;
use serde_json::{Value, json};

/// A durable occurrence, committed atomically with its exact transcript input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Caller-saved occurrence identifier, unique within this conversation.
    pub request: String,
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

pub(crate) fn active_input(talk: &Talk) -> Option<usize> {
    // A historical interrupted request must never hijack a later owner turn.
    // Bind execution to this worker's immutable payload, not a pending flag.
    let home = crate::run::try_home()?;
    let job = crate::worker::current_job(&home).ok()??;
    if job.payload.get("talk")?.as_str()? != talk.id {
        return None;
    }
    let request = job.payload.get("external_request")?.as_str()?;
    talk.external_requests
        .iter()
        .find(|r| {
            r.request == request
                && r.status == "started"
                && r.worker.as_deref() == Some(job.id.as_str())
        })
        .map(|r| r.input_turn)
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
            json!({"id":cursor(&talk,i,j),"sender":sender(turn,message),"body":body,"at":turn.at})
        })
        .collect();
    let more = coordinates.next().is_some();
    let last = messages.last().and_then(|v| v["id"].as_str()).or(after);
    Ok(json!({"talk":talk.id,"messages":messages,"cursor":last,"more":more}))
}

fn view(store: &Talks, talk: &Talk, record: &Request) -> Value {
    let mut status = record.status.as_str();
    if matches!(status, "accepted" | "started") {
        // Absence of completion is not permission to execute again. A worker
        // result may be pruned; that is unknown, not a fresh request.
        if let Some(id) = &record.worker {
            match crate::worker::get(store.home(), id) {
                Ok(job)
                    if matches!(
                        job.status,
                        crate::worker::Status::Failed | crate::worker::Status::Finished
                    ) =>
                {
                    status = "uncertain"
                }
                Err(_) => status = "uncertain",
                _ => {}
            }
        } else {
            status = "uncertain";
        }
    }
    json!({"talk":talk.id,"request":record.request,"status":status,
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
    anyhow::ensure!(
        !request.is_empty()
            && request.len() <= 128
            && request
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "request must contain 1..128 ASCII letters, digits, -, _ or ."
    );
    anyhow::ensure!(
        !label.trim().is_empty() && label.len() <= 128 && !label.chars().any(char::is_control),
        "sender must be a nonempty single-line label of at most 128 bytes"
    );
    anyhow::ensure!(!text.trim().is_empty(), "nothing to send");
    let talk = match resolve(store, selector)? {
        Ok(t) => t,
        Err(v) => return Ok(v),
    };
    if let Some(r) = talk.external_requests.iter().find(|r| r.request == request) {
        return Ok(if r.text == text && r.sender == label {
            view(store, &talk, r)
        } else {
            json!({"talk":talk.id,"request":request,"status":"conflict"})
        });
    }
    let Some(_publication) = crate::chat_worker::try_publication_guard(store, &talk.id)? else {
        return Ok(json!({"talk":talk.id,"request":request,"status":"busy"}));
    };
    // Replays are observational even if the conversation is now busy/closed.
    let fresh = store.get(&talk.id)?;
    if let Some(r) = fresh
        .external_requests
        .iter()
        .find(|r| r.request == request)
    {
        return Ok(if r.text == text && r.sender == label {
            view(store, &fresh, r)
        } else {
            json!({"talk":fresh.id,"request":request,"status":"conflict"})
        });
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
        let mut fresh = store.get(&talk.id)?;
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
        fresh.external_requests.push(Request {
            request: request.to_owned(),
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
        let mut fresh = store.get(&talk.id)?;
        let r = fresh
            .external_requests
            .iter_mut()
            .find(|r| r.request == request)
            .context("external request disappeared")?;
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
    let fresh = store.get(&talk.id)?;
    Ok(view(
        store,
        &fresh,
        fresh
            .external_requests
            .iter()
            .find(|r| r.request == request)
            .context("external request disappeared")?,
    ))
}

/// Observation only: timeout neither cancels nor repeats accepted work.
pub async fn reply(store: &Talks, selector: &str, request: &str, wait: u64) -> Result<Value> {
    let talk = match resolve(store, selector)? {
        Ok(t) => t,
        Err(v) => return Ok(v),
    };
    let deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(wait))
        .context("wait duration exceeds the monotonic clock range")?;
    loop {
        let fresh = store.get(&talk.id)?;
        let Some(record) = fresh
            .external_requests
            .iter()
            .find(|r| r.request == request)
        else {
            return Ok(json!({"talk":talk.id,"request":request,"status":"not_found"}));
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
    loop {
        if let Some(lease) = store.claim_turn(id)? {
            let publication = crate::chat_worker::publication_guard(store, id).await?;
            let mut talk = {
                let _guard = store.guard()?;
                let mut talk = store.get(id)?;
                let r = talk
                    .external_requests
                    .iter_mut()
                    .find(|r| r.request == request)
                    .context("external request missing")?;
                // A second worker or uncertain restart cannot repeat a CLI call.
                if r.status != "accepted" {
                    return Ok(());
                }
                r.status = "started".into();
                r.worker = crate::worker::current_job(store.home())?.map(|j| j.id);
                store.put(&mut talk)?;
                talk
            };
            let index = active_input(&talk).context("external input missing")?;
            let text = talk
                .turns
                .get(index)
                .context("external transcript input missing")?
                .body
                .clone();
            drop(publication);
            let (cfg, _) = Config::discover(&talk.repo, None)?;
            return super::respond(&lease, &mut talk, store, &cfg, &text).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
