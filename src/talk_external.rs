//! Local CLI ingress. Publication, TurnLease and worker ownership are the same
//! as web Chat; an uncertain submission is never replayed.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

fn reserve_binding(store: &Talks, request: &str, binding: &OwnerBinding) -> Result<bool> {
    let path = binding_path(store, request);
    std::fs::create_dir_all(path.parent().expect("binding directory"))?;
    publish_exclusive(
        &path,
        &path.with_extension(format!("tmp.{}", fresh_token())),
        &serde_json::to_string(binding)?,
    )
    .context("reserve owner request binding")
}

/// A durable occurrence, committed atomically with its exact transcript input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Caller-saved occurrence identifier.
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
    /// not_started. Uncertain observations do not change this boundary.
    pub status: String,
    /// Detached worker reservation that owns this occurrence.
    pub worker: Option<String>,
    /// Startup or response diagnostic, when known.
    pub error: Option<String>,
}

// Older scheduler generations rewrite Talk through their own schema. They
// preserve these receipt fields and the transcript, but not new fields.
// The owner binding therefore lives outside Talk and is the authority.
fn owner_input(
    talk_id: &str,
    input: Option<&Turn>,
    record: &Request,
    binding: Option<&OwnerBinding>,
) -> Result<bool> {
    let turn = input.context("recorded CLI input is missing")?;
    let owner = binding.filter(|binding| binding.talk == talk_id);
    if let Some(binding) = owner {
        anyhow::ensure!(
            binding.text_sha256 == digest(&record.text)
                && record.sender.is_empty()
                && turn.who == Who::Operator
                && turn.sender == Some(Sender::Operator)
                && turn.body == record.text
                && turn.messages.iter().all(|message| {
                    message.sender == Some(Sender::Operator) && message.body == record.text
                }),
            "saved owner input does not match its admission"
        );
    } else {
        anyhow::ensure!(
            turn.sender == Some(Sender::External),
            "saved CLI input has no matching owner binding or external provenance"
        );
    }
    Ok(owner.is_some())
}

fn ingress(owner: bool) -> &'static str {
    if owner { "owner" } else { "external" }
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
            let request = talk.external_requests.iter().find(|r| r.input_turn == i);
            let kind = request
                .map(|r| {
                    let binding = read_binding(store, &r.request)?;
                    owner_input(&talk.id, Some(turn), r, binding.as_ref()).map(ingress)
                })
                .transpose()?;
            Ok(json!({"id":cursor(&talk,i,j),"sender":sender(turn,message),"body":body,"at":turn.at,
                "request":request.map(|r| &r.request),"ingress":kind}))
        })
        .collect::<Result<_>>()?;
    let more = coordinates.next().is_some();
    let last = messages.last().and_then(|v| v["id"].as_str()).or(after);
    Ok(json!({"talk":talk.id,"messages":messages,"cursor":last,"more":more}))
}

fn view(store: &Talks, talk: &Talk, record: &Request, owner: bool) -> Value {
    use crate::worker::Status;
    let mut status = record.status.as_str();
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
        "ingress":ingress(owner),"goal_authorizations":goal_authorizations,
        "input_id":cursor(talk,record.input_turn,0),
        "reply_id":record.reply_turn.map(|i| cursor(talk,i,0)),
        "reply":record.reply_turn.and_then(|i| talk.turns.get(i)).map(|t| &t.body),
        "worker":record.worker,"error":record.error})
}

enum InputKind<'a> {
    External(&'a str),
    Owner,
}

/// Accept once, or return the saved occurrence. Duplicate calls never dispatch.
pub async fn send(
    store: &Talks,
    selector: &str,
    request: &str,
    label: &str,
    text: &str,
) -> Result<Value> {
    submit(store, selector, request, InputKind::External(label), text).await
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
    submit(store, id, request, InputKind::Owner, text).await
}

async fn submit(
    store: &Talks,
    selector: &str,
    request: &str,
    input: InputKind<'_>,
    text: &str,
) -> Result<Value> {
    let label = match input {
        InputKind::External(label) => label,
        InputKind::Owner => "",
    };
    let owner = matches!(input, InputKind::Owner);
    anyhow::ensure!(
        !request.is_empty()
            && request.len() <= 128
            && request
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "request must contain 1..128 ASCII letters, digits, -, _ or ."
    );
    if let InputKind::External(label) = input {
        anyhow::ensure!(
            !label.trim().is_empty() && label.len() <= 128 && !label.chars().any(char::is_control),
            "sender must be a nonempty single-line label of at most 128 bytes"
        );
    }
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
    // Owner bindings are home-wide for owner submissions only. Within the
    // bound conversation, an incomplete owner receipt still fences both kinds.
    let scoped_binding = || -> Result<Option<OwnerBinding>> {
        Ok(read_binding(store, request)?.filter(|binding| owner || binding.talk == talk.id))
    };
    let replay = |talk: &Talk, r: &Request| -> Result<Value> {
        let binding = scoped_binding()?;
        let saved_owner = owner_input(&talk.id, talk.turns.get(r.input_turn), r, binding.as_ref())?;
        Ok(
            if saved_owner == owner && r.text == text && r.sender == label {
                view(store, talk, r, saved_owner)
            } else {
                json!({"talk":talk.id,"request":request,"status":"conflict"})
            },
        )
    };
    if let Some(r) = talk.external_requests.iter().find(|r| r.request == request) {
        return replay(&talk, r);
    }
    if let Some(binding) = scoped_binding()? {
        // Re-read: the binding can become visible just before the atomic
        // transcript write. Never dispatch again in that interval.
        let fresh = read_path(&store.path_of(&talk.id))?;
        return match fresh
            .external_requests
            .iter()
            .find(|r| r.request == request)
        {
            Some(r) => replay(&fresh, r),
            None => Ok(binding_status(&binding)),
        };
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
        return replay(&fresh, r);
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
        if let Some(binding) = scoped_binding()? {
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
        if let Some(binding) = &expected_binding
            && !reserve_binding(store, request, binding)?
        {
            let winner =
                read_binding(store, request)?.context("owner request binding disappeared")?;
            return Ok(binding_status(&winner));
        }
        fresh.turns.push(match input {
            InputKind::Owner => operator_turn(text, Vec::new(), None),
            InputKind::External(label) => Turn {
                sender: Some(Sender::External),
                who: Who::Operator,
                body: format!("External agent [{label}]:\n{text}"),
                at: Timestamp::now(),
                attachments: Vec::new(),
                usage: None,
                breaks: Some(Vec::new()),
                messages: Vec::new(),
                posted_by: None,
            },
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
        let mut fresh = read_path(&store.path_of(&talk.id))?;
        let r = fresh
            .external_requests
            .iter_mut()
            .find(|r| r.request == request)
            .context("CLI request disappeared")?;
        record_launch(r, result);
        store.put(&mut fresh)?;
    }
    let fresh = read_path(&store.path_of(&talk.id))?;
    replay(
        &fresh,
        fresh
            .external_requests
            .iter()
            .find(|r| r.request == request)
            .context("CLI request disappeared")?,
    )
}

fn record_launch(r: &mut Request, result: Result<crate::worker::Job>) {
    match result {
        Ok(job) => r.worker = Some(job.id),
        Err(error) => {
            // Acknowledgement is observation, not revocation. Only a proven
            // non-start can close admission before the reserved worker runs.
            if r.status == "accepted" {
                if let Some(pending) = error.downcast_ref::<crate::worker::StartupUnacknowledged>()
                {
                    r.worker = Some(pending.id.clone());
                } else if crate::worker::not_started(&error) {
                    r.status = "not_started".into();
                }
            }
            r.error = Some(format!("{error:#}"));
        }
    }
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
        let binding = read_binding(store, request)?;
        let owner = owner_input(
            &fresh.id,
            fresh.turns.get(record.input_turn),
            record,
            binding.as_ref(),
        )?;
        let mut value = view(store, &fresh, record, owner);
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
                let pos = talk
                    .external_requests
                    .iter()
                    .position(|r| r.request == request)
                    .context("CLI request missing")?;
                let r = &talk.external_requests[pos];
                // A second worker or uncertain restart cannot repeat a CLI call.
                if r.status != "accepted" {
                    return Ok(());
                }
                let binding = read_binding(store, request)?;
                let owner = owner_input(id, talk.turns.get(r.input_turn), r, binding.as_ref())?;
                let r = &mut talk.external_requests[pos];
                if owner && !talk.status.open() {
                    r.status = "not_started".into();
                    r.error = Some("conversation closed before execution".into());
                    store.put(&mut talk)?;
                    return Ok(());
                }
                anyhow::ensure!(
                    r.worker.as_deref().is_none_or(|worker| worker == job.id),
                    "CLI submission belongs to a different worker"
                );
                r.status = "started".into();
                r.worker = Some(job.id.clone());
                r.error = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_owner_admission_cannot_replace_a_request_binding() {
        let home = tempfile::tempdir().unwrap();
        let store = Talks::at(home.path().join("talks"));
        let request = "follow";
        let first = OwnerBinding {
            talk: "first".into(),
            text_sha256: digest("Observe the release"),
        };
        let second = OwnerBinding {
            talk: "second".into(),
            text_sha256: digest("Observe the release"),
        };

        // Both admissions observed an absent binding before the second won.
        assert!(read_binding(&store, request).unwrap().is_none());
        assert!(reserve_binding(&store, request, &second).unwrap());
        assert!(!reserve_binding(&store, request, &first).unwrap());
        assert!(!reserve_binding(&store, request, &second).unwrap());
        assert!(read_binding(&store, request).unwrap().as_ref() == Some(&second));
    }

    #[test]
    fn startup_timeout_preserves_the_reserved_request() {
        let home = tempfile::tempdir().unwrap();
        let store = Talks::at(home.path().join("talks"));
        let mut talk = seeded_for_test("owner", home.path().to_owned());
        let text = "Observe the release";
        talk.turns.push(operator_turn(text, Vec::new(), None));
        let mut record: Request = serde_json::from_value(json!({
            "request":"follow", "sender":"", "text":text,
            "input_turn":0, "status":"accepted"
        }))
        .unwrap();
        let id = "12345678-1234-4321-8234-123456789abc";
        let dir = home.path().join("workers/jobs").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("job.json"),
            json!({
                "id":id, "kind":"chat", "key":"chat-external:owner:follow",
                "payload":{"talk":talk.id,"external_request":"follow"},
                "generation":"fixture", "status":"starting", "created_at":0
            })
            .to_string(),
        )
        .unwrap();
        record_launch(
            &mut record,
            Err(crate::worker::StartupUnacknowledged { id: id.into() }.into()),
        );
        assert_eq!(
            record.status, "accepted",
            "the worker must still be admitted"
        );
        let queued = view(&store, &talk, &record, true);
        assert_eq!(queued["status"], "queued");
        assert_eq!(queued["worker"], id);
        std::fs::remove_file(dir.join("job.json")).unwrap();
        assert_eq!(view(&store, &talk, &record, true)["status"], "uncertain");
    }
}
