//! Deputies: the seat that waits on a conductor's question, so the conductor
//! never has to.
//!
//! [`crate::conduct`] files a question for the operator and moves on; it is
//! non-blocking by construction, because one task's question must not park the
//! whole polling loop. The price used to be that nothing at all was waiting on
//! it: the owner's free-text reply landed in a thread no agent read, while the
//! phone said "waiting for the agent". Question f1dc on task 684d sat open
//! forever on a "setup done".
//!
//! A deputy is the answer. For each open conductor question, `magi serve`
//! runs one short-lived agent seat that is handed what the conductor knew
//! ([`brief`]: the task, why it asked, what each option leads to), blocks on
//! the question with `magi ask --wait`, answers back in the thread with
//! `magi ask --thread`, and - when the owner's own words clearly pick an
//! option - records that with `magi ask --settle`. Applying the outcome is not
//! the deputy's job: an answered conductor question goes through the daemon's
//! existing `resolve_blockers` / action path, exactly as one tapped on the
//! phone does.
//!
//! # Invariants
//!
//! - **One seat per question, keyed by seat name.** `deputy-<question id>`,
//!   never the conductor's shared seat (`conduct/seat.json`, overwritten by
//!   every cycle) and never an agent id.
//! - **Persisted on the question** ([`crate::ask::Deputy`]), always through
//!   [`Questions::update`]: the owner's say and answer write the same file. A
//!   restarted daemon resumes the seat only when [`agent::has_session`] says
//!   it can; otherwise it starts a fresh seat and re-sends the whole context,
//!   never assuming memory.
//! - **Bounded.** At most `daemon.max_deputies` at once; at most
//!   [`MAX_STARTS`] starts per question, and a restart does not reset that. A
//!   deputy never outlives the question's `answer_timeout`: the waiter retires
//!   the question at its deadline ([`crate::waiter`]) and the task goes to a
//!   machine hold (`daemon::resolve_blockers`).
//! - **Never a second agent on one question.** A fresh lease or the claim file
//!   means somebody is already on it.
//! - **No new authority.** A deputy does not edit, merge or touch the queue,
//!   and `--settle` accepts only an offered label backed by a verbatim quote
//!   of the owner, never on a task the operator holds.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use jiff::Timestamp;
use tokio::task::JoinSet;

use crate::agent::{self, Invocation, SeatState};
use crate::ask::{ChoiceAction, Deputy, Question, Questions, Waiter as Note, WaiterKind, Who};
use crate::config::Config;
use crate::notices::{self, Notice};
use crate::prompt;

/// Node name a deputy runs under (`MAGI_NODE`); `magi ask --settle` accepts
/// it and nothing else.
pub const NODE: &str = "deputy";

/// How often the runner looks at the store.
pub const TICK: Duration = Duration::from_secs(5);

/// Most turns ever started for one question. A deputy that keeps ending
/// without an answer is a problem for the owner, not something to retry until
/// the quota is gone.
pub const MAX_STARTS: u32 = 3;

/// Least gap between two starts for one question.
const RESTART_AFTER: Duration = Duration::from_secs(30);

/// Added to the question's remaining time for the invocation's wall clock, so
/// the deputy's own `magi ask --wait` reaches the deadline first.
const SLACK_SECS: u64 = 120;

/// A claim file older than this was left by a process that died.
const CLAIM_STALE: Duration = Duration::from_secs(60);

/// How long the context-handover turn may take.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Stops the deputy when true: the daemon is parking.
pub type Halt = Arc<dyn Fn() -> bool + Send + Sync>;

/// What the conductor knew about one question, for its deputy.
///
/// `reason` is the conductor's own one-line reasoning (may be empty). Each
/// choice is listed with what picking it does: an attached action, or - for
/// the usual conductor question - that the answer is recorded on the task and
/// the task is unblocked with it in its instructions.
pub fn brief(
    task_id: &str,
    reason: &str,
    choices: &[String],
    actions: &BTreeMap<String, ChoiceAction>,
) -> String {
    let reason = reason.trim();
    let mut s = format!(
        "Task {task_id} (`magi task show {task_id}`) is blocked on this question. \
         The conductor asked because: {}\n\n\
         When the owner answers, magi records the question and the answer on the \
         task and unblocks it, and the answer becomes part of the instructions of \
         the task's next attempt.",
        if reason.is_empty() {
            "(it recorded no reasoning)"
        } else {
            reason
        }
    );
    if !choices.is_empty() {
        s.push_str("\n\nWhat each option does:");
        for c in choices {
            match actions.get(c) {
                Some(a) => s.push_str(&format!("\n- `{c}`: also {}", a.describe())),
                None => s.push_str(&format!(
                    "\n- `{c}`: recorded on the task as the answer and the task is unblocked"
                )),
            }
        }
    }
    s
}

/// Has this question's deputy run out of starts with the deadline gone?
///
/// Then nothing will ever read an unread say, and the waiter must retire the
/// question anyway instead of deferring to a deputy that no longer starts.
pub fn exhausted_past_deadline(q: &Question, default_timeout: u64, now: Timestamp) -> bool {
    let secs = if q.answer_timeout > 0 {
        q.answer_timeout
    } else {
        default_timeout
    };
    q.status.open()
        && q.deputy.as_ref().is_some_and(|d| d.starts >= MAX_STARTS)
        && now.as_second() > q.last_activity().saturating_add(secs as i64)
}

/// The deputy runner: its own task inside `magi serve`, beside the waiter.
pub struct Deputies {
    store: Questions,
    home: PathBuf,
    cfg: Option<Config>,
    /// Working directory for a question that recorded none.
    fallback_repo: PathBuf,
    max: usize,
    halt: Halt,
    tasks: JoinSet<String>,
    inflight: HashSet<String>,
    /// Earliest next start per question.
    memo: HashMap<String, Instant>,
}

impl Deputies {
    /// A runner over `store`, with at most `max` deputies at once.
    pub fn new(
        store: Questions,
        home: PathBuf,
        cfg: Option<Config>,
        fallback_repo: PathBuf,
        max: usize,
        halt: Halt,
    ) -> Self {
        Self {
            store,
            home,
            cfg,
            fallback_repo,
            max,
            halt,
            tasks: JoinSet::new(),
            inflight: HashSet::new(),
            memo: HashMap::new(),
        }
    }

    fn default_timeout(&self) -> u64 {
        self.cfg
            .as_ref()
            .map_or(Config::default().graph.answer_timeout, |c| {
                c.graph.answer_timeout
            })
    }

    fn reap(&mut self) {
        while let Some(done) = self.tasks.try_join_next() {
            if let Ok(id) = done {
                self.inflight.remove(&id);
            }
        }
        if self.tasks.is_empty() {
            self.inflight.clear();
        }
    }

    /// Give a conductor question the record a deputy needs: the deputy itself
    /// (with a brief rebuilt from what the question stored, when it was filed
    /// before deputies existed - a lost reason is not invented), the working
    /// directory and the deadline the waiter and `magi ask --wait` enforce.
    fn attach(&self, q: &Question) -> Option<Question> {
        let default_timeout = self.default_timeout();
        let repo = self.fallback_repo.to_string_lossy().into_owned();
        self.store
            .update(&q.id, |r| {
                if r.deputy.is_none() {
                    r.deputy = Some(Deputy::new(brief(
                        &r.run, &r.detail, &r.choices, &r.actions,
                    )));
                }
                if r.cwd.is_none() {
                    r.cwd = Some(repo.clone());
                }
                if r.answer_timeout == 0 {
                    r.answer_timeout = default_timeout;
                }
                Ok(())
            })
            .map(|(r, ())| r)
            .map_err(|e| tracing::warn!("question {}: cannot attach a deputy: {e:#}", q.short()))
            .ok()
    }

    /// Look at every open conductor question once and start a deputy on each
    /// that has none, within the limits. Returns without waiting for them.
    pub fn tick(&mut self, now: Timestamp) {
        self.reap();
        for q in self.store.list() {
            if (self.halt)() {
                return;
            }
            if !q.status.open() || q.node != crate::conduct::NODE {
                continue;
            }
            let q = if q.deputy.is_none() || q.cwd.is_none() || q.answer_timeout == 0 {
                match self.attach(&q) {
                    Some(q) => q,
                    None => continue,
                }
            } else {
                q
            };
            let Some(dep) = q.deputy.as_ref() else {
                continue;
            };
            if self.inflight.contains(&q.id)
                || self.store.read_lease(&q.id).is_some_and(|l| l.fresh(now))
            {
                continue;
            }
            if dep.starts >= MAX_STARTS {
                self.give_up(&q);
                continue;
            }
            // Past the deadline the waiter retires the question; a say that
            // arrived in time is still read first.
            let deadline = q.last_activity().saturating_add(q.answer_timeout as i64);
            if now.as_second() > deadline && q.unread_from_owner().is_none() {
                continue;
            }
            if self.inflight.len() >= self.max {
                continue;
            }
            if matches!(self.memo.get(&q.id), Some(until) if Instant::now() < *until) {
                continue;
            }
            self.memo
                .insert(q.id.clone(), Instant::now() + RESTART_AFTER);
            self.inflight.insert(q.id.clone());
            let job = Job {
                store: self.store.clone(),
                home: self.home.clone(),
                cfg: self.cfg.clone(),
                fallback_repo: self.fallback_repo.clone(),
                halt: Arc::clone(&self.halt),
            };
            let id = q.id.clone();
            self.tasks.spawn(async move {
                if let Err(e) = job.turn(&id).await {
                    tracing::warn!("deputy for question {}: {e:#}", crate::ask::short_id(&id));
                }
                id
            });
        }
    }

    /// Wait for every deputy turn in flight. The daemon never calls this; the
    /// tests do, to look at the record once a turn is over.
    pub async fn drain(&mut self) {
        while let Some(done) = self.tasks.join_next().await {
            if let Ok(id) = done {
                self.inflight.remove(&id);
            }
        }
        self.inflight.clear();
    }

    /// Say once that nobody is listening any more.
    fn give_up(&self, q: &Question) {
        notices::raise_in(
            &self.home,
            Notice::warn(
                &format!("deputy:{}", q.id),
                format!(
                    "Question {} \"{}\": its follow-up agent ended {MAX_STARTS} times \
                     without an answer and is not restarted. What you say is recorded \
                     but nothing will read it; answer with one of the choices instead.",
                    q.short(),
                    q.summary
                ),
            ),
        );
    }
}

/// Everything one deputy turn needs, owned so it can run detached.
struct Job {
    store: Questions,
    home: PathBuf,
    cfg: Option<Config>,
    fallback_repo: PathBuf,
    halt: Halt,
}

impl Job {
    /// Run one invocation, beating the lease; `None` when the daemon is
    /// parking and the turn was dropped.
    async fn drive(
        &self,
        spec: &crate::config::AgentSpec,
        seat: &mut SeatState,
        inv: &Invocation<'_>,
        id: &str,
    ) -> Option<Result<agent::AgentOutput>> {
        let fut = agent::invoke(spec, seat, inv);
        tokio::pin!(fut);
        let mut beat = tokio::time::interval(Duration::from_secs(1));
        let mut beats = 0u32;
        loop {
            tokio::select! {
                r = &mut fut => break Some(r),
                _ = beat.tick() => {
                    if (self.halt)() {
                        break None;
                    }
                    beats += 1;
                    if beats % 20 == 0 {
                        self.store.beat(id, WaiterKind::Deputy);
                    }
                }
            }
        }
    }

    /// Parking: the turn is dropped and the start refunded. The seat stays as
    /// last persisted - after the handover turn, resumable.
    fn park(&self, id: &str) {
        let _ = self.store.update(id, |r| {
            if let Some(d) = r.deputy.as_mut() {
                d.starts = d.starts.saturating_sub(1);
            }
            r.waiter = None;
            Ok(())
        });
        self.store.drop_lease(id);
    }

    async fn turn(&self, id: &str) -> Result<()> {
        let claim = self.store.root().join(format!("{id}.deputy-claim"));
        // The claim only covers the decision to start and the write that records
        // it, so a daemon that dies holding it blocks a restart for a minute,
        // not for a turn's length. The lease guards the turn itself.
        if !crate::waiter::take_claim(&claim, CLAIM_STALE) {
            return Ok(());
        }
        let release = crate::waiter::Release(claim);

        // Decided again under the claim, on the record as it is now.
        let q = self.store.get(id)?;
        let now = Timestamp::now();
        let Some(dep) = q.deputy.clone() else {
            return Ok(());
        };
        if !q.status.open()
            || dep.starts >= MAX_STARTS
            || self.store.read_lease(&q.id).is_some_and(|l| l.fresh(now))
        {
            return Ok(());
        }
        let cfg = self
            .cfg
            .as_ref()
            .context("the deputy's configuration is not available")?;
        let spec = match cfg.agent(&dep.agent) {
            Ok(s) if !dep.agent.is_empty() => s.clone(),
            _ => {
                cfg.resolve_roles()
                    .context("resolving the deputy's agent")?
                    .conductor
            }
        };
        // The same CLI conversation when it can be resumed; otherwise a fresh
        // seat - and either way the prompt carries the whole context.
        let key = crate::ask::deputy_seat_key(&q.id);
        let (mut seat, resumed) = match dep.seat.clone() {
            Some(s)
                if s.agent == spec.id && agent::has_session(spec.kind, &s, cfg.graph.sessions) =>
            {
                (s, true)
            }
            _ => (SeatState::new(&key, &spec.id, crate::rng::entropy()), false),
        };
        let cwd = q
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| self.fallback_repo.clone());

        let thread: Vec<(&str, &str)> = q
            .thread
            .iter()
            .map(|t| {
                (
                    if t.who == Who::Operator {
                        "operator"
                    } else {
                        "agent"
                    },
                    t.body.as_str(),
                )
            })
            .collect();
        let read = &thread[..q.delivered_turns.min(thread.len())];
        let unread = q.unread_from_owner();
        let snapshot = q.thread.len();
        let body = prompt::deputy(&prompt::DeputyPrompt {
            id: &q.id,
            summary: &q.summary,
            detail: &q.detail,
            brief: &dep.brief,
            choices: &q.choices,
            thread: read,
            unread: unread.as_deref(),
            resumed,
            handover: false,
            language: &cfg.graph.language,
        });

        // Recorded before the turn starts, so a daemon that dies mid-turn
        // leaves a start counted and the seat on the record.
        self.store.beat(&q.id, WaiterKind::Deputy);
        let starts = dep.starts + 1;
        let first = seat.clone();
        self.store.update(&q.id, |r| {
            if let Some(d) = r.deputy.as_mut() {
                d.agent = spec.id.clone();
                d.seat = Some(first);
                d.starts = starts;
            }
            r.waiter = Some(Note {
                kind: WaiterKind::Deputy,
                since: now,
            });
            Ok(())
        })?;
        drop(release);
        tracing::info!(
            "question {}: deputy seat {} {} (start {starts}/{MAX_STARTS})",
            q.short(),
            seat.key,
            if resumed { "resuming" } else { "starting" }
        );

        let left = (q.last_activity().saturating_add(q.answer_timeout as i64) - now.as_second())
            .max(0) as u64;
        let artifacts = self.store.root().join(format!("{}.deputy", q.id));
        let cache_dir = cfg.cache_dir();
        // `magi ask` writes the question record and its lock, which a read-only
        // sandbox refuses, so the seat cannot be read-only. It is told never to
        // edit anything; the daemon, not the deputy, applies outcomes.
        let allow_write = true;
        macro_rules! invocation {
            ($prompt:expr, $stem:expr, $timeout:expr) => {
                Invocation {
                    cwd: &cwd,
                    prompt: $prompt,
                    timeout: $timeout,
                    allow_write,
                    sessions: cfg.graph.sessions,
                    artifacts: &artifacts,
                    stem: $stem,
                    // The question's own run key (the task id) is what lets this
                    // seat's `magi ask` pass the ownership check on a conductor
                    // question.
                    run: &q.run,
                    node: NODE,
                    cache_dir: cache_dir.as_deref(),
                    attachments: &[],
                }
            };
        }

        // A fresh seat first takes a short turn that ends: `agent::invoke` only
        // learns the CLI's session id when it returns, and the real turn blocks
        // in `magi ask` for hours, so a daemon stopped mid-wait would otherwise
        // leave nothing to resume. This turn persists the seat before the wait.
        let mut early = None;
        if !resumed && cfg.graph.sessions {
            let hbody = prompt::deputy(&prompt::DeputyPrompt {
                id: &q.id,
                summary: &q.summary,
                detail: &q.detail,
                brief: &dep.brief,
                choices: &q.choices,
                thread: read,
                unread: None,
                resumed: false,
                handover: true,
                language: &cfg.graph.language,
            });
            let hstem = format!("handover-{starts}");
            let hinv = invocation!(&hbody, &hstem, HANDOVER_TIMEOUT);
            let Some(done) = self.drive(&spec, &mut seat, &hinv, &q.id).await else {
                self.park(&q.id);
                return Ok(());
            };
            let kept = seat.clone();
            self.store.update(&q.id, |r| {
                if let Some(d) = r.deputy.as_mut() {
                    d.seat = Some(kept);
                }
                Ok(())
            })?;
            if !matches!(&done, Ok(o) if o.usable()) {
                early = Some(done);
            }
        }

        let out = match early {
            Some(done) => done,
            None => {
                let stem = format!("turn-{starts}");
                let timeout = Duration::from_secs(left.max(60) + SLACK_SECS);
                let inv = invocation!(&body, &stem, timeout);
                match self.drive(&spec, &mut seat, &inv, &q.id).await {
                    Some(out) => out,
                    None => {
                        self.park(&q.id);
                        return Ok(());
                    }
                }
            }
        };

        let (text, why) = match out {
            Ok(o) if o.usable() => (Some(o.text.trim().to_owned()), None),
            Ok(o) if o.timed_out => (None, Some("its turn timed out".to_owned())),
            Ok(o) if o.quota_exhausted() => (None, Some("the agent is out of quota".to_owned())),
            Ok(o) => (
                None,
                Some(format!("its turn failed (exit {:?})", o.exit_code)),
            ),
            Err(e) => (None, Some(format!("the agent could not be started: {e:#}"))),
        };
        let kept = seat.clone();
        self.store.update(&q.id, |r| {
            if let Some(d) = r.deputy.as_mut() {
                d.seat = Some(kept);
            }
            r.waiter = None;
            // The owner's words were in the prompt. An agent that read them but
            // answered in prose instead of `magi ask --thread` still spoke;
            // keep it on the record so the owner reads it.
            if let Some(text) = text
                && unread.is_some()
                && !text.is_empty()
                && r.status.open()
                && r.thread.len() == snapshot
            {
                r.delivered_turns = r.delivered_turns.max(snapshot);
                let choices = r.choices.clone();
                r.reply(text, choices)?;
            }
            Ok(())
        })?;
        self.store.drop_lease(&q.id);
        if let Some(why) = why {
            tracing::warn!("question {}: the deputy ended: {why}", q.short());
            notices::raise_in(
                &self.home,
                Notice::warn(
                    &format!("deputy-turn:{}", q.id),
                    format!(
                        "Question {} \"{}\": its follow-up agent stopped ({why}).",
                        q.short(),
                        q.summary
                    ),
                ),
            );
        }
        Ok(())
    }
}

/// The runner's loop, until `stop` is asked for.
pub async fn run(mut deputies: Deputies, stop: crate::daemon::Stop) {
    while !stop.stopped() {
        deputies.tick(Timestamp::now());
        tokio::time::sleep(TICK).await;
    }
}
