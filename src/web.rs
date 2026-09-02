//! The web UI: magi's queue and run history, readable from a phone.
//!
//! The terminal is the wrong surface for the two things an operator actually
//! does between runs — file a task and check whether the last competition
//! landed. Both happen away from the desk, so they get an HTTP surface: a
//! handful of JSON routes and three embedded files.
//!
//! # One binary
//!
//! `index.html`, `app.css` and `app.js` are compiled in with [`include_str!`].
//! There is no `--assets-dir` and no filesystem fallback, because a UI that
//! reads its own front end from disk breaks the moment the binary is copied
//! somewhere else — which is exactly what `cargo install magi-cli` does. No
//! JS toolchain, no CDN, no remote font: everything the phone needs arrives
//! from this process.
//!
//! # No authentication
//!
//! There is none, deliberately, and the startup log says so. The tailnet is
//! the security boundary: `--bind auto` resolves to this machine's Tailscale
//! address, so the UI is reachable from the operator's own devices and from
//! nothing else. Anyone who can open the URL can file and hold tasks, which is
//! why binding to `0.0.0.0` is not offered and why the fallback when Tailscale
//! is missing is loopback rather than every interface.
//!
//! # Change notification
//!
//! A phone must not poll a full run list on a mobile link. `GET /api/events`
//! is a server-sent stream carrying nothing but two revision numbers — the
//! newest modification time in the queue and under the runs directory — so the
//! client refetches only what moved. The browser's own SSE reconnection covers
//! a sleeping phone; there is no session to lose.
//!
//! # Reading state must never take the server down
//!
//! A corrupt `run.json` is skipped in the list and explained with a 500 on the
//! detail route. No handler unwraps a filesystem or parse result: a single bad
//! file left by a killed run would otherwise turn the whole history into a
//! blank page.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;

use crate::queue::{Queue, Source, Task, title_from};
use crate::run::{RunState, RunStatus};
use crate::{daemon, report, run};

/// Default port. Chosen high and memorable; nothing else in the fleet uses it.
pub const DEFAULT_PORT: u16 = 7878;

/// How often the change stream restats the queue and the runs directory.
const POLL: Duration = Duration::from_secs(1);

/// Keep-alive interval for the change stream. Phones and intermediaries drop
/// an idle connection within a minute; a comment every fifteen seconds keeps
/// the stream alive without waking the radio often enough to matter.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Runs returned when the client does not ask, and the ceiling if it asks for
/// more. The cap exists because the list handler parses every `run.json` it
/// returns, and a phone cannot render two thousand rows anyway.
const LIST_DEFAULT: usize = 50;
/// Upper bound for `?limit=`.
const LIST_MAX: usize = 500;

/// Width of a generated task title, matching what the CLI uses.
const TITLE_MAX: usize = 72;

const INDEX_HTML: &str = include_str!("../assets/ui/index.html");
const APP_CSS: &str = include_str!("../assets/ui/app.css");
const APP_JS: &str = include_str!("../assets/ui/app.js");

/// Which address to listen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    /// Ask Tailscale, and fall back to loopback with a warning.
    Auto,
    /// An address the operator named.
    Addr(IpAddr),
}

impl std::str::FromStr for Bind {
    type Err = String;

    /// `auto`, or anything [`IpAddr`] accepts. Parsing lives with the type so
    /// the CLI can take `--bind` straight into it: the one spelling of
    /// `auto` that matters is the one this function knows.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        s.parse()
            .map(Self::Addr)
            .map_err(|_| format!("expected `auto` or an IP address, got `{s}`"))
    }
}

impl std::fmt::Display for Bind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Addr(addr) => write!(f, "{addr}"),
        }
    }
}

/// How to serve.
#[derive(Debug, Clone)]
pub struct Opts {
    /// Address to listen on.
    pub bind: Bind,
    /// Port to listen on.
    pub port: u16,
    /// Repository used for tasks posted without one.
    pub repo: PathBuf,
    /// Print the URL on its own line for a caller that wants to hand it to a
    /// browser. magi never launches one itself.
    pub open: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            bind: Bind::Auto,
            port: DEFAULT_PORT,
            repo: PathBuf::from("."),
            open: false,
        }
    }
}

/// Everything the handlers touch.
///
/// The queue, the runs directory and the magi home are fields rather than
/// process-global lookups so a test drives the real router against a temp
/// directory instead of the operator's own history.
#[derive(Debug, Clone)]
pub struct Ui {
    queue: Queue,
    runs: PathBuf,
    home: PathBuf,
    repo: PathBuf,
}

impl Ui {
    /// A server over explicit paths.
    pub fn new(queue: Queue, runs: PathBuf, home: PathBuf, repo: PathBuf) -> Self {
        Self {
            queue,
            runs,
            home,
            repo,
        }
    }

    /// The operator's own state: `<home>/queue`, `<home>/runs`.
    pub fn open(repo: PathBuf) -> Self {
        Self::new(Queue::open(), run::runs_root(), run::home(), repo)
    }

    /// The router, with this state baked in.
    ///
    /// The three front-end files get one explicit route each rather than a
    /// path parameter, so there is no traversal surface to get wrong: the set
    /// of servable paths is the set written here.
    pub fn router(self) -> Router {
        Router::new()
            .route("/", get(index))
            .route("/app.css", get(app_css))
            .route("/app.js", get(app_js))
            .route("/api/health", get(health))
            .route("/api/runs", get(runs_list))
            .route("/api/runs/{id}", get(run_detail))
            .route("/api/runs/{id}/report", get(run_report))
            .route("/api/queue", get(queue_list).post(queue_post))
            .route("/api/queue/{id}/hold", post(queue_hold))
            .route("/api/queue/{id}/release", post(queue_release))
            .route("/api/events", get(events))
            .with_state(Arc::new(self))
    }
}

/// Serve the UI until the process is stopped.
///
/// There is no graceful-shutdown hook: the server owns no state of its own, so
/// killing it loses nothing a restart cannot rebuild from the queue and the
/// runs directory.
pub async fn serve(opts: Opts) -> Result<()> {
    let (addr, warning) = resolve_bind(&opts.bind);
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }

    // Process-global, and therefore set exactly once, here: the report route
    // must never emit escape sequences into a browser, and toggling the flag
    // per request would race with a concurrent request rendering its own
    // report. Startup is the only moment at which no request can observe the
    // change. Nothing in the server turns colour back on.
    report::set_color(false);

    let ui = Ui::open(opts.repo);
    let socket = SocketAddr::new(addr, opts.port);
    let listener = tokio::net::TcpListener::bind(socket)
        .await
        .with_context(|| format!("bind {socket}"))?;
    let url = format!("http://{addr}:{}", opts.port);
    tracing::info!(
        "magi web UI on {url} - there is no authentication, so anyone who can \
         reach this address can file and hold tasks: the tailnet is the \
         security boundary"
    );
    if opts.open {
        // The URL alone on stdout, for a caller that wants to open it. magi
        // does not spawn a browser: on the machine this usually runs on there
        // is no display, and a failed launch would be the only output.
        println!("{url}");
    }
    axum::serve(listener, ui.router())
        .await
        .context("serve the web UI")
}

/// Resolve `--bind` to an address, plus a warning when the answer is not what
/// the operator asked for.
///
/// Split out from [`serve`] because the interesting half - deciding whether
/// Tailscale gave us something usable - is testable without opening a socket.
pub fn resolve_bind(bind: &Bind) -> (IpAddr, Option<String>) {
    match bind {
        Bind::Addr(addr) => (*addr, None),
        Bind::Auto => match tailscale_ip() {
            Ok(ip) => (IpAddr::V4(ip), None),
            Err(why) => (
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                Some(format!(
                    "--bind auto fell back to 127.0.0.1: {why}. The UI is \
                     local-only and a phone cannot reach it; start Tailscale \
                     or pass --bind <addr>"
                )),
            ),
        },
    }
}

/// This machine's Tailscale IPv4, or why there is not one.
///
/// `tailscale ip -4` is a local call against the running daemon and returns in
/// milliseconds, so it is fine to make it synchronously before the server
/// exists. Only an address inside `100.64.0.0/10` is accepted: that is the
/// CGNAT block Tailscale assigns from, and anything else on that output would
/// be a different tool answering.
fn tailscale_ip() -> std::result::Result<Ipv4Addr, String> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .map_err(|e| format!("could not run `tailscale ip -4` ({e})"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        return Err(format!(
            "`tailscale ip -4` failed ({}){}",
            out.status,
            if why.is_empty() {
                String::new()
            } else {
                format!(": {why}")
            }
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        .find(is_tailnet)
        .ok_or_else(|| "`tailscale ip -4` printed no address in 100.64.0.0/10".to_owned())
}

/// Is this address in the CGNAT block Tailscale hands out from?
fn is_tailnet(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// What every handler returns. Spelled out because `Result` in this crate is
/// `anyhow::Result`, and a handler's error is a status code as much as a
/// message.
type ApiResult<T> = std::result::Result<T, ApiError>;

/// A handler failure, rendered as the `{"error": ".."}` body the UI expects.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    /// The client asked for something malformed.
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// No such run or task.
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    /// Someone else owns the thing the client wants to change.
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    /// Our fault, or the disk's.
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    /// Errors from `queue` and `run` carry their context chain, and the whole
    /// chain goes to the client: "parse /home/x/runs/y/run.json: expected
    /// value at line 3" is a message an operator can act on, and there is no
    /// secret in a path on a single-user tailnet.
    fn from(e: anyhow::Error) -> Self {
        Self::internal(format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

/// Run a handler's filesystem work off the executor.
///
/// Every route that touches the disk goes through here rather than each one
/// arguing about whether its own read is small enough. Uniform because the
/// expensive case is not rare: `run.json` for a finished competition holds
/// every judgement, deliberation turn and review round, so listing a few
/// hundred runs is megabytes of parsing, and the executor threads doing it are
/// the same ones serving the change stream of every other connected phone.
async fn blocking<T>(job: impl FnOnce() -> ApiResult<T> + Send + 'static) -> ApiResult<T>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(job).await {
        Ok(result) => result,
        Err(e) => Err(ApiError::internal(format!("filesystem task failed: {e}"))),
    }
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

async fn app_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

/// What `/api/health` answers.
#[derive(Debug, Serialize)]
struct HealthView {
    version: &'static str,
    home: String,
    queue_rev: u64,
    runs_rev: u64,
    /// Runs on disk whose state this build cannot parse - almost always a
    /// schema bump, occasionally a run killed mid-write.
    ///
    /// Reported because the list silently skips them, and "no competitions
    /// yet" is a lie when six of them are sitting in the runs directory. The
    /// terminal deck learned the same lesson: a run that fails to parse must
    /// not disappear from the count.
    runs_unreadable: usize,
    daemon: DaemonView,
}

/// The daemon's state as the UI presents it.
#[derive(Debug, Serialize)]
struct DaemonView {
    running: bool,
    idle: Option<bool>,
    pid: Option<u32>,
    current: Option<daemon::Current>,
    completed: Option<u64>,
    stale_for_secs: Option<i64>,
}

impl DaemonView {
    /// Judge a status file. Staleness is [`daemon::Reading::running`]'s call,
    /// not this UI's — a crashed daemon must not look alive here while
    /// `doctor` calls it dead.
    fn of(status: Option<daemon::Reading>) -> Self {
        let Some(status) = status else {
            return Self {
                running: false,
                idle: None,
                pid: None,
                current: None,
                completed: None,
                stale_for_secs: None,
            };
        };
        let now = Timestamp::now();
        let age = status.age_secs(now);
        Self {
            running: status.running(now),
            idle: Some(status.idle),
            pid: status.pid,
            current: status.current,
            completed: Some(status.completed),
            stale_for_secs: age,
        }
    }
}

async fn health(State(ui): State<Arc<Ui>>) -> ApiResult<Json<HealthView>> {
    blocking(move || {
        Ok(Json(HealthView {
            version: env!("CARGO_PKG_VERSION"),
            home: ui.home.display().to_string(),
            queue_rev: ui.queue.revision(),
            runs_rev: runs_revision(&ui.runs),
            runs_unreadable: runs_unreadable(&ui.runs),
            daemon: DaemonView::of(daemon::read_status(&ui.home)),
        }))
    })
    .await
}

/// One row in the run list.
///
/// The list route returns this rather than whole `RunState`s: the summary of a
/// run is a few hundred bytes and the state is megabytes, and the difference
/// is what makes the history usable on a mobile link.
#[derive(Debug, Serialize)]
struct RunSummary {
    id: String,
    short: String,
    status: String,
    done: bool,
    instruction: String,
    title: String,
    repo: String,
    repo_name: String,
    created_at: String,
    updated_at: String,
    candidates: usize,
    viable: usize,
    judges: usize,
    winner: Option<char>,
    reviews: usize,
    quota_losses: usize,
    event: Option<String>,
}

impl RunSummary {
    fn of(state: &RunState) -> Self {
        Self {
            id: state.id.clone(),
            short: state.short().to_owned(),
            status: status_word(state.status),
            done: state.status.done(),
            instruction: state.instruction.clone(),
            title: title_from(&state.instruction, TITLE_MAX),
            repo: state.repo.display().to_string(),
            repo_name: state
                .repo
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            created_at: state.created_at.to_string(),
            updated_at: state.updated_at.to_string(),
            candidates: state.candidates.len(),
            viable: state.viable().len(),
            judges: state.config.graph.judges,
            winner: state.winner().map(|c| c.label),
            reviews: state.reviews.len(),
            quota_losses: state.quota.len(),
            event: state.events.last().map(|e| e.message.clone()),
        }
    }
}

/// `RunStatus` as the wire spells it. Every variant is one word, so this is
/// the same string `serde` writes for the status inside a full run.
fn status_word(status: RunStatus) -> String {
    format!("{status:?}").to_lowercase()
}

/// `?limit=`, clamped by the handler.
#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn runs_list(
    State(ui): State<Arc<Ui>>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<RunSummary>>> {
    let limit = q.limit.unwrap_or(LIST_DEFAULT).min(LIST_MAX);
    blocking(move || {
        let summaries = run_ids(&ui.runs)
            .into_iter()
            // A run whose state cannot be read is skipped, not fatal: a run
            // killed mid-write must not blank the history of every other one.
            // The detail route still explains it, which is where an operator
            // asking "what happened to that run" ends up.
            .filter_map(|id| read_run(&ui.runs, &id).ok())
            .take(limit)
            .map(|state| RunSummary::of(&state))
            .collect();
        Ok(Json(summaries))
    })
    .await
}

async fn run_detail(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunState>> {
    blocking(move || {
        let id = resolve_run(&ui.runs, &id)?;
        Ok(Json(read_run(&ui.runs, &id)?))
    })
    .await
}

async fn run_report(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let text = blocking(move || {
        let id = resolve_run(&ui.runs, &id)?;
        // Colour is off for the whole process, set once in `serve`. Rendering
        // is CPU work over the full state, which is the other reason this is
        // not on the executor.
        Ok(report::run(&read_run(&ui.runs, &id)?))
    })
    .await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text))
}

/// A task as the UI sees it.
///
/// The whole task, plus the two things the client would otherwise have to
/// reimplement: the human-readable source and the status string. Nothing is
/// removed - the phone shows `last_error` and the run history verbatim.
#[derive(Debug, Serialize)]
struct TaskView {
    #[serde(flatten)]
    task: Task,
    source_label: String,
    status_str: &'static str,
}

impl From<Task> for TaskView {
    fn from(task: Task) -> Self {
        Self {
            source_label: task.source.label(),
            status_str: task.status.as_str(),
            task,
        }
    }
}

async fn queue_list(State(ui): State<Arc<Ui>>) -> ApiResult<Json<Vec<TaskView>>> {
    blocking(move || {
        Ok(Json(
            ui.queue.list().into_iter().map(TaskView::from).collect(),
        ))
    })
    .await
}

/// The body of `POST /api/queue`.
///
/// Every field defaults so the phone can send only what the operator typed,
/// and unknown fields are ignored so a newer front end talking to an older
/// binary still files the task.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NewTask {
    instruction: String,
    title: Option<String>,
    repo: Option<PathBuf>,
    priority: Option<i32>,
}

async fn queue_post(
    State(ui): State<Arc<Ui>>,
    body: std::result::Result<Json<NewTask>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    // Taken as a `Result` so a malformed body is the 400 the contract promises
    // rather than axum's default 422, which the UI has no branch for.
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    if body.instruction.trim().is_empty() {
        return Err(ApiError::bad_request(
            "instruction must not be blank: an empty task would burn a whole \
             competition on nothing",
        ));
    }
    let view = blocking(move || {
        let title = body
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| title_from(&body.instruction, TITLE_MAX));
        let repo = body.repo.unwrap_or_else(|| ui.repo.clone());
        let mut task = Task::new(title, body.instruction, repo, Source::Human);
        task.priority = body.priority.unwrap_or(0);
        ui.queue.put(&mut task)?;
        Ok(TaskView::from(task))
    })
    .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn queue_hold(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TaskView>> {
    mutate(ui, id, Task::hold).await
}

async fn queue_release(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TaskView>> {
    mutate(ui, id, Task::release).await
}

/// Read a task, change it, write it back, under the queue's own lock.
///
/// Taking the same claim a daemon takes is what makes hold and release safe to
/// press while magi is running: without it the daemon's next save would land
/// on top of the operator's hold and the task would keep going.
async fn mutate(ui: Arc<Ui>, id: String, change: fn(&mut Task)) -> ApiResult<Json<TaskView>> {
    blocking(move || {
        let id = resolve_task(&ui.queue, &id)?;
        // `claim` fails when the lock file already exists, which is the
        // conflict the UI must report: the daemon owns that task's file for
        // as long as it is running it, and our write would be lost under its
        // next save. The message names the lock either way.
        let _claim = ui.queue.claim(&id).map_err(|e| {
            ApiError::conflict(format!(
                "{e:#} - a daemon is running this task, so it cannot be \
                 changed from here yet"
            ))
        })?;
        let mut task = ui.queue.get(&id)?;
        change(&mut task);
        ui.queue.put(&mut task)?;
        Ok(Json(TaskView::from(task)))
    })
    .await
}

/// The change stream: two revision numbers, on connect and whenever either
/// moves.
///
/// The poll runs in one spawned task per client, which is affordable because
/// the work is a directory scan and a `stat` per file. It stops as soon as the
/// receiver is gone, so a phone that walks out of range costs nothing after
/// its next tick - there is no session and no cleanup to forget.
async fn events(State(ui): State<Arc<Ui>>) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(4);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        let mut last: Option<(u64, u64)> = None;
        loop {
            // The first tick completes immediately, which is what makes the
            // stream announce the current revisions on connect.
            ticker.tick().await;
            let state = Arc::clone(&ui);
            let revisions = tokio::task::spawn_blocking(move || {
                (state.queue.revision(), runs_revision(&state.runs))
            })
            .await;
            let Ok(revisions) = revisions else { break };
            if last == Some(revisions) {
                continue;
            }
            last = Some(revisions);
            let payload = serde_json::json!({
                "queue_rev": revisions.0,
                "runs_rev": revisions.1,
            });
            // Serializing two integers cannot fail; giving up beats looping.
            let Ok(event) = Event::default().event("change").json_data(payload) else {
                break;
            };
            if tx.send(event).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx).map(Ok::<Event, Infallible>))
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
}

/// Newest `run.json` modification time under `runs`, in milliseconds.
///
/// One `stat` per run rather than a parse per run: this is the number the
/// change stream polls every second, so it has to stay cheap even with a
/// thousand runs on disk.
fn runs_revision(runs: &FsPath) -> u64 {
    std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.path().join("run.json").metadata().ok())
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .max()
        .unwrap_or(0)
}

/// Run ids under `runs`, newest first.
///
/// Rooted at an explicit directory rather than calling [`run::list_ids`],
/// which reads the process-global home: the server has to be drivable against
/// a temp directory for any of this to be testable.
fn run_ids(runs: &FsPath) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("run.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // Ids start with a sortable timestamp.
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// Read one run's state from an explicit runs root.
fn read_run(runs: &FsPath, id: &str) -> Result<RunState> {
    let path = runs.join(id).join("run.json");
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let state: RunState =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    if state.schema != run::SCHEMA {
        anyhow::bail!(
            "run {} was written by a different magi (schema {}, this build speaks {})",
            state.id,
            state.schema,
            run::SCHEMA
        );
    }
    Ok(state)
}

/// Runs on disk under `runs` whose state this build cannot parse - almost
/// always a schema bump, occasionally a run killed mid-write.
///
/// Exposed so every surface that reports on runs shares one count instead of
/// each re-deriving it: `/api/health` reports it as `runs_unreadable`, and
/// `magi doctor` calls this directly rather than guessing at the same number
/// a second way.
#[must_use]
pub fn runs_unreadable(runs: &FsPath) -> usize {
    run_ids(runs)
        .into_iter()
        .filter(|id| read_run(runs, id).is_err())
        .count()
}

/// Expand an id or short id to exactly one run id.
fn resolve_run(runs: &FsPath, id: &str) -> ApiResult<String> {
    if runs.join(id).join("run.json").is_file() {
        return Ok(id.to_owned());
    }
    pick(run_ids(runs), id, "run")
}

/// Expand an id or short id to exactly one task id.
fn resolve_task(queue: &Queue, id: &str) -> ApiResult<String> {
    if queue.path_of(id).is_file() {
        return Ok(id.to_owned());
    }
    pick(queue.list().into_iter().map(|t| t.id).collect(), id, "task")
}

/// The one prefix rule, used for both runs and tasks: a leading match for a
/// full id, a trailing match for the short form an operator reads off a
/// report. Written here rather than borrowed from `queue::resolve_id` because
/// the UI needs the two failures as different status codes, and telling them
/// apart from an error message is not something to build a route on.
fn pick(ids: Vec<String>, prefix: &str, what: &str) -> ApiResult<String> {
    let mut hits = ids
        .into_iter()
        .filter(|id| id.starts_with(prefix) || id.ends_with(prefix));
    match (hits.next(), hits.next()) {
        (Some(one), None) => Ok(one),
        (None, _) => Err(ApiError::not_found(format!("no {what} matches `{prefix}`"))),
        (Some(a), Some(b)) => Err(ApiError::bad_request(format!(
            "`{prefix}` matches more than one {what}, including {a} and {b}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::config::Config;
    use crate::queue::TaskStatus;

    /// A home with a queue and a runs directory, and a router serving it on
    /// loopback. `tower`'s `oneshot` is not reachable - `tower` is axum's
    /// dependency, not ours - so the tests drive a real socket, which has the
    /// side benefit of asserting the status line and content types the phone
    /// actually receives.
    struct Fixture {
        home: TempDir,
        addr: SocketAddr,
    }

    impl Fixture {
        async fn start() -> Self {
            let home = TempDir::new().expect("temp home");
            let addr = Self::serve(home.path()).await;
            Self { home, addr }
        }

        async fn serve(home: &FsPath) -> SocketAddr {
            let queue = Queue::at(home.join("queue"));
            let runs = home.join("runs");
            std::fs::create_dir_all(&runs).expect("runs dir");
            let ui = Ui::new(queue, runs, home.to_path_buf(), PathBuf::from("/repo/magi"));
            let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            tokio::spawn(async move {
                let _ = axum::serve(listener, ui.router()).await;
            });
            addr
        }

        fn queue(&self) -> Queue {
            Queue::at(self.home.path().join("queue"))
        }

        fn runs(&self) -> PathBuf {
            self.home.path().join("runs")
        }

        async fn get(&self, path: &str) -> Res {
            request(self.addr, "GET", path, None).await
        }

        async fn post(&self, path: &str, body: Option<&str>) -> Res {
            request(self.addr, "POST", path, body).await
        }
    }

    struct Res {
        status: u16,
        headers: String,
        body: String,
    }

    impl Res {
        fn json(&self) -> Value {
            serde_json::from_str(&self.body)
                .unwrap_or_else(|e| panic!("body is not json ({e}): {}", self.body))
        }
    }

    /// A one-shot HTTP/1.1 client. `Connection: close` is what lets the reply
    /// be read to end-of-stream without parsing framing.
    async fn request(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> Res {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: magi\r\nConnection: close\r\n");
        if let Some(body) = body {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        if let Some(body) = body {
            head.push_str(body);
        }
        let mut socket = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the test server");
        socket
            .write_all(head.as_bytes())
            .await
            .expect("write request");
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("read response");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").expect("a header block");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        Res {
            status,
            headers: head.to_lowercase(),
            body: body.to_owned(),
        }
    }

    /// A run on disk, without touching the process-global magi home.
    fn write_run(runs: &FsPath, id: &str, status: RunStatus) {
        let mut state = RunState::new(
            PathBuf::from("/repo/magi"),
            "main".to_owned(),
            "0123456789abcdef".to_owned(),
            "Add a web UI\n\nMobile first.".to_owned(),
            Config::default(),
        );
        state.id = id.to_owned();
        state.status = status;
        let dir = runs.join(id);
        std::fs::create_dir_all(&dir).expect("run dir");
        std::fs::write(
            dir.join("run.json"),
            serde_json::to_string_pretty(&state).expect("serialize run"),
        )
        .expect("write run.json");
    }

    fn write_daemon(home: &FsPath, updated_at: Timestamp) {
        let body = serde_json::json!({
            "schema": 1,
            "pid": 4242,
            "started_at": Timestamp::now().to_string(),
            "updated_at": updated_at.to_string(),
            "idle": false,
            "current": { "task": "20260902-140501-aaaa", "run": "20260902-140502-bbbb" },
            "completed": 7,
            "polls": 143,
        });
        std::fs::write(home.join("daemon.json"), body.to_string()).expect("write daemon.json");
    }

    #[tokio::test]
    async fn a_blank_instruction_is_rejected_and_files_nothing() {
        let f = Fixture::start().await;

        let res = f
            .post("/api/queue", Some(r#"{"instruction":"   \n  "}"#))
            .await;

        assert_eq!(res.status, 400);
        assert!(
            res.json()["error"].as_str().is_some_and(|e| !e.is_empty()),
            "a rejection has to say why: {}",
            res.body
        );
        assert!(
            f.queue().list().is_empty(),
            "a rejected task must not reach the disk"
        );
    }

    #[tokio::test]
    async fn a_malformed_body_is_a_bad_request_not_an_unprocessable_entity() {
        let f = Fixture::start().await;

        let res = f.post("/api/queue", Some("{not json")).await;

        // The UI branches on 400; axum's default for a bad body is 422, which
        // it would report as an unknown failure.
        assert_eq!(res.status, 400);
    }

    #[tokio::test]
    async fn a_posted_task_is_queued_with_a_title_taken_from_its_instruction() {
        let f = Fixture::start().await;

        let created = f
            .post(
                "/api/queue",
                Some(
                    r##"{"instruction":"# Rework the config loader\n\nIt re-reads the file on every lookup"}"##,
                ),
            )
            .await;
        assert_eq!(created.status, 201);

        let listed = f.get("/api/queue").await;
        let tasks = listed.json();
        let task = &tasks[0];

        assert_eq!(tasks.as_array().map(Vec::len), Some(1));
        // The title the server derives is the summary the author already
        // wrote, without its marker.
        assert_eq!(task["title"], "Rework the config loader");
        assert_eq!(task["source_label"], "human");
        assert_eq!(task["status_str"], "queued");
        assert_eq!(task["repo"], "/repo/magi", "the server's default repo");
        assert_eq!(
            task["id"],
            created.json()["id"],
            "the posted task is the listed one"
        );
        assert!(
            task["instruction"]
                .as_str()
                .is_some_and(|i| i.starts_with("# Rework the config loader\n\nIt re-reads")),
            "the instruction reaches the graph verbatim, markers and all: {}",
            task["instruction"]
        );
    }

    #[tokio::test]
    async fn holding_then_releasing_returns_a_task_to_the_loop_with_a_fresh_budget() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "spent".to_owned(),
            "Try again".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        task.start("20260902-140502-bbbb".to_owned());
        task.fail("agent gave up", 9);
        queue.put(&mut task).expect("file the task");

        let held = f.post(&format!("/api/queue/{}/hold", task.id), None).await;
        assert_eq!(held.status, 200);
        assert_eq!(held.json()["status_str"], "held");

        let released = f
            .post(&format!("/api/queue/{}/release", task.id), None)
            .await;
        assert_eq!(released.status, 200);
        assert_eq!(released.json()["status_str"], "queued");
        assert_eq!(
            released.json()["attempts"],
            0,
            "release is a real second chance, not an instant re-hold"
        );
        assert_eq!(
            queue.get(&task.id).expect("reload").status,
            TaskStatus::Queued,
            "the change is on disk, not only in the reply"
        );
        assert!(
            !f.home
                .path()
                .join("queue")
                .join(format!("{}.lock", task.id))
                .exists(),
            "the claim the mutation took is released again"
        );
    }

    #[tokio::test]
    async fn a_task_a_daemon_is_running_cannot_be_changed_from_the_phone() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "busy".to_owned(),
            "Running right now".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        queue.put(&mut task).expect("file the task");
        let _claim = queue.claim(&task.id).expect("stand in for the daemon");

        let res = f.post(&format!("/api/queue/{}/hold", task.id), None).await;

        assert_eq!(res.status, 409);
        assert_eq!(
            queue.get(&task.id).expect("reload").status,
            TaskStatus::Queued,
            "the refused hold changed nothing"
        );
    }

    #[tokio::test]
    async fn unknown_ids_are_json_not_found_on_both_stores() {
        let f = Fixture::start().await;

        let run = f.get("/api/runs/nosuchrun").await;
        let task = f.post("/api/queue/nosuchtask/hold", None).await;

        assert_eq!(run.status, 404);
        assert_eq!(task.status, 404);
        assert!(
            run.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains("run")),
            "the error names what was not found: {}",
            run.body
        );
        assert!(
            task.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains("task")),
            "the error names what was not found: {}",
            task.body
        );
    }

    #[tokio::test]
    async fn the_daemon_counts_as_running_only_while_its_heartbeat_is_fresh() {
        let f = Fixture::start().await;

        let missing = f.get("/api/health").await.json();
        assert_eq!(missing["daemon"]["running"], false, "no file, no daemon");

        write_daemon(
            f.home.path(),
            Timestamp::now() - jiff::SignedDuration::from_secs(60),
        );
        let stale = f.get("/api/health").await.json();
        assert_eq!(
            stale["daemon"]["running"], false,
            "a minute without a heartbeat is a dead daemon, not a busy one"
        );
        assert!(
            stale["daemon"]["stale_for_secs"]
                .as_i64()
                .is_some_and(|s| s >= 55),
            "staleness is reported so the UI can say how long: {stale}"
        );

        write_daemon(f.home.path(), Timestamp::now());
        let fresh = f.get("/api/health").await.json();
        assert_eq!(fresh["daemon"]["running"], true);
        assert_eq!(fresh["daemon"]["idle"], false);
        assert_eq!(fresh["daemon"]["pid"], 4242);
        assert_eq!(fresh["daemon"]["completed"], 7);
        assert_eq!(fresh["daemon"]["current"]["task"], "20260902-140501-aaaa");
        assert_eq!(fresh["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn a_newer_daemon_status_file_still_renders() {
        let f = Fixture::start().await;
        // A field this build has never heard of must not turn the status line
        // into a 500; that is the whole reason the reader is permissive.
        std::fs::write(
            f.home.path().join("daemon.json"),
            serde_json::json!({
                "schema": 2,
                "updated_at": Timestamp::now().to_string(),
                "idle": true,
                "surprise": { "nested": [1, 2, 3] },
            })
            .to_string(),
        )
        .expect("write daemon.json");

        let health = f.get("/api/health").await;

        assert_eq!(health.status, 200);
        assert_eq!(health.json()["daemon"]["running"], true);
    }

    #[tokio::test]
    async fn a_corrupt_run_is_skipped_in_the_list_and_explained_on_its_own_route() {
        let f = Fixture::start().await;
        write_run(&f.runs(), "20260902-140501-good", RunStatus::Ready);
        let broken = f.runs().join("20260902-140502-bad");
        std::fs::create_dir_all(&broken).expect("run dir");
        std::fs::write(broken.join("run.json"), "{ truncated").expect("write run.json");

        let list = f.get("/api/runs").await;
        let detail = f.get("/api/runs/20260902-140502-bad").await;

        assert_eq!(list.status, 200);
        let listed = list.json();
        let ids: Vec<&str> = listed
            .as_array()
            .expect("an array")
            .iter()
            .map(|r| r["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(
            ids,
            vec!["20260902-140501-good"],
            "one unreadable run must not cost the operator the whole history"
        );
        assert_eq!(detail.status, 500);
        assert!(
            detail.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains("run.json")),
            "the failure names the file to look at: {}",
            detail.body
        );
        // A skipped run has to be countable somewhere, or the UI shows an
        // empty history with nothing to explain it - which is exactly what a
        // directory full of older-schema runs looks like.
        let health = f.get("/api/health").await;
        assert_eq!(health.json()["runs_unreadable"], 1);
    }

    #[tokio::test]
    async fn a_run_is_summarised_for_the_list_and_served_whole_on_its_own_route() {
        let f = Fixture::start().await;
        write_run(&f.runs(), "20260902-140501-a1b2", RunStatus::Ready);

        let summary = f.get("/api/runs").await.json();
        let row = &summary[0];
        assert_eq!(row["short"], "a1b2");
        assert_eq!(row["status"], "ready");
        assert_eq!(row["done"], true);
        assert_eq!(row["title"], "Add a web UI");
        assert_eq!(row["repo_name"], "magi");
        assert_eq!(row["judges"], 3);
        assert_eq!(row["winner"], Value::Null);
        assert_eq!(row["reviews"], 0);

        // The short id resolves, and the detail route is the state itself, not
        // a projection of it: the UI reads fields the summary does not carry.
        let detail = f.get("/api/runs/a1b2").await;
        assert_eq!(detail.status, 200);
        assert_eq!(detail.json()["base_branch"], "main");
        assert_eq!(detail.json()["id"], "20260902-140501-a1b2");
    }

    #[tokio::test]
    async fn the_run_list_is_newest_first_and_honours_a_limit() {
        let f = Fixture::start().await;
        for id in [
            "20260902-140501-aaaa",
            "20260902-140502-bbbb",
            "20260902-140503-cccc",
        ] {
            write_run(&f.runs(), id, RunStatus::Merged);
        }

        let all = f.get("/api/runs").await.json();
        let capped = f.get("/api/runs?limit=2").await.json();

        assert_eq!(all[0]["id"], "20260902-140503-cccc");
        assert_eq!(all.as_array().map(Vec::len), Some(3));
        assert_eq!(capped.as_array().map(Vec::len), Some(2));
        assert_eq!(capped[0]["id"], "20260902-140503-cccc");
    }

    #[tokio::test]
    async fn the_report_route_serves_the_terminal_report_as_plain_text() {
        let f = Fixture::start().await;
        write_run(&f.runs(), "20260902-140501-a1b2", RunStatus::Blocked);

        let res = f.get("/api/runs/20260902-140501-a1b2/report").await;

        assert_eq!(res.status, 200);
        assert!(
            res.headers
                .contains("content-type: text/plain; charset=utf-8"),
            "a browser must render it, not download it: {}",
            res.headers
        );
        // The assertion is on content, not on the absence of escapes: colour
        // is a process-global that `serve` turns off at startup, and another
        // test in this binary may own it while this one runs.
        assert!(
            res.body.contains("20260902-140501-a1b2"),
            "the report is about the run that was asked for: {}",
            res.body
        );
    }

    #[tokio::test]
    async fn the_front_end_is_served_from_the_binary_with_types_a_phone_renders() {
        let f = Fixture::start().await;

        let html = f.get("/").await;
        let css = f.get("/app.css").await;
        let js = f.get("/app.js").await;

        assert_eq!((html.status, css.status, js.status), (200, 200, 200));
        assert!(
            html.headers
                .contains("content-type: text/html; charset=utf-8")
        );
        assert!(css.headers.contains("content-type: text/css"));
        assert!(js.headers.contains("content-type: text/javascript"));
        assert_eq!(html.body, INDEX_HTML, "compiled in, never read from disk");
    }

    #[tokio::test]
    async fn the_change_stream_announces_the_current_revisions_on_connect() {
        let f = Fixture::start().await;

        let mut socket = tokio::net::TcpStream::connect(f.addr)
            .await
            .expect("connect");
        socket
            .write_all(
                b"GET /api/events HTTP/1.1\r\nHost: magi\r\nAccept: text/event-stream\r\n\r\n",
            )
            .await
            .expect("write request");

        // Read until the first event arrives rather than to end of stream: the
        // stream is endless by design, which is the point of the route.
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        while !seen.contains("event: change") {
            let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
                .await
                .expect("the stream must speak within five seconds")
                .expect("read");
            assert!(read > 0, "the server closed the change stream: {seen}");
            seen.push_str(&String::from_utf8_lossy(&buf[..read]));
        }

        assert!(
            seen.to_lowercase()
                .contains("content-type: text/event-stream"),
            "the browser only reconnects automatically for a real SSE stream: {seen}"
        );
        let data = seen
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .expect("a data line");
        let payload: Value = serde_json::from_str(data.trim()).expect("json payload");
        assert!(
            payload["queue_rev"].is_u64() && payload["runs_rev"].is_u64(),
            "the client needs both revisions to know what to refetch: {payload}"
        );
    }

    #[test]
    fn bind_reads_back_from_the_spelling_the_cli_prints() {
        // The CLI shows the default in `--help` and parses whatever comes
        // back, so the two directions have to agree or `--bind auto` breaks
        // the moment someone copies the help text.
        for bind in [Bind::Auto, Bind::Addr(IpAddr::V4(Ipv4Addr::LOCALHOST))] {
            assert_eq!(bind.to_string().parse::<Bind>(), Ok(bind));
        }
        assert_eq!("AUTO".parse::<Bind>(), Ok(Bind::Auto));
        assert!("everywhere".parse::<Bind>().is_err());
    }

    #[test]
    fn an_explicit_bind_address_is_taken_verbatim() {
        let asked = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));

        let (addr, warning) = resolve_bind(&Bind::Addr(asked));

        assert_eq!(addr, asked);
        assert!(
            warning.is_none(),
            "an operator who named an address gets no lecture"
        );
    }

    #[test]
    fn bind_auto_either_finds_a_tailnet_address_or_says_the_ui_is_local_only() {
        let (addr, warning) = resolve_bind(&Bind::Auto);

        // This has to hold on a CI runner with no `tailscale` and on a dev box
        // with one, so the invariant asserted is the one shared by both
        // outcomes: the address is either a real tailnet address offered
        // without comment, or loopback with an explanation. What must never
        // happen is a silent fallback - an operator told "listening on
        // 127.0.0.1" with no reason would go looking for a firewall.
        match addr {
            IpAddr::V4(ip) if is_tailnet(&ip) => {
                assert!(warning.is_none(), "a tailnet address needs no warning");
            }
            other => {
                assert_eq!(other, IpAddr::V4(Ipv4Addr::LOCALHOST));
                let warning = warning.expect("a fallback has to explain itself");
                assert!(
                    warning.contains("127.0.0.1") && warning.contains("local-only"),
                    "the warning says what happened and what it costs: {warning}"
                );
            }
        }
    }

    #[test]
    fn only_the_cgnat_block_counts_as_a_tailnet_address() {
        // `tailscale ip -4` output is trusted only inside 100.64.0.0/10; the
        // boundary cases are what stop us binding to some other tool's idea of
        // an address.
        assert!(is_tailnet(&Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailnet(&Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_tailnet(&Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_tailnet(&Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_tailnet(&Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn an_ambiguous_prefix_is_a_bad_request_and_a_missing_one_is_not_found() {
        let ids = vec![
            "20260902-140501-aaaa".to_owned(),
            "20260902-140502-aabb".to_owned(),
        ];

        let missing = pick(ids.clone(), "zzzz", "run").expect_err("no match");
        let ambiguous = pick(ids.clone(), "202609", "run").expect_err("two matches");
        let short = pick(ids, "aabb", "run").expect("the short id is the tail of an id");

        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(ambiguous.status, StatusCode::BAD_REQUEST);
        assert_eq!(short, "20260902-140502-aabb");
    }
}
