//! Deputies: the seat that waits on a conductor's question, so the conductor
//! never has to - and on land's merge approval, whose free-text reply used to
//! reach nobody either.
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
//! - **A merge approval is served too, but stays land's.** Its deputy has no
//!   `cwd` (so the waiter never touches it), its deadline is `asked_at +
//!   answer_timeout` and never moves on a reply ([`deadline`]), and the only
//!   thing that retires it is `daemon::land_resume_state`. A say alone never
//!   merges: `--settle` accepts `merge` only for the owner's own word `merge`.
//! - **A release-watch question is served too, and stays the watcher's.** The
//!   escalation, local-mode approval and failed-release questions
//!   ([`crate::release_watch`], node `release-bump`, seat `release-watch`) have
//!   no asker, so a say reached nobody. Its deputy has no `cwd` either, a
//!   fixed `asked_at + answer_timeout` deadline ([`deadline`]), and applies
//!   nothing: `--settle` records a choice and the watcher applies it on its next
//!   lap. A local approval's `merge` / `hold` go through the merge-approval
//!   rules ([`merge_gated`]). A deputy never closes or merges the pull request.
//! - **Every open question the owner can say something to has a listener.**
//!   [`Kind::Triage`] serves the triage questions; [`Kind::Generic`] is the
//!   fallback for any other question with choices and no `cwd` (the divergence
//!   question today, a node nobody has written yet tomorrow). It is decided by
//!   the missing `cwd`, never by node name. Only a conductor question is ever
//!   given a `cwd` ([`Deputies::attach`]); the other kinds would otherwise be
//!   resumed and expired by the waiter beside the deputy. A test enumerates
//!   every node filed without an asker, so a new kind of question cannot ship
//!   without one.
//! - **No new authority.** A deputy does not edit, merge or touch the queue,
//!   and `--settle` accepts only an offered label backed by a verbatim quote
//!   of the owner, never on a task the operator holds.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// What a deputy serves: the question kinds it is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A conductor's question ([`crate::conduct::NODE`]).
    Conduct,
    /// The merge approval ([`crate::land::APPROVAL_NODE`]).
    Land,
    /// A question the release watcher filed ([`crate::release_watch`]).
    Release,
    /// A triage question about a held task or a stuck dependency root
    /// ([`crate::triage::NODE`], [`crate::triage::DEPS_NODE`]).
    Triage,
    /// Any other question nobody asked from inside a run (no `cwd`) that offers
    /// choices: the fallback, so a node nobody has met yet still has a listener.
    Generic,
}

/// The kind of question a deputy serves for `q`, `None` for every other.
///
/// The fallback is decided by the absence of a `cwd`, never by the node name: a
/// question filed with `magi ask` inside a run records its `cwd` and has an
/// asker (and the waiter), and the node a seat asks from is the seat's own name
/// (a reviewer's `review` is also the divergence question's node). A question
/// with no choices cannot be settled and is a notice, not something to answer.
pub fn kind_of(q: &Question) -> Option<Kind> {
    match q.node.as_str() {
        crate::conduct::NODE => Some(Kind::Conduct),
        crate::land::APPROVAL_NODE => Some(Kind::Land),
        // The seat matters: `bump` files choice-less notices on the same node,
        // which nobody can answer and which must not cost a deputy.
        crate::bump::NOTICE_NODE if q.seat == "release-watch" => Some(Kind::Release),
        crate::bump::NOTICE_NODE => None,
        crate::triage::NODE | crate::triage::DEPS_NODE => Some(Kind::Triage),
        _ if q.cwd.is_none() && !q.choices.is_empty() => Some(Kind::Generic),
        _ => None,
    }
}

/// What a deputy is told about a question of no known kind: only what the
/// question itself stored.
pub fn generic_brief(
    q: &Question,
    actions: &BTreeMap<String, ChoiceAction>,
    home: &Path,
) -> String {
    // `Question::run` may also hold a task id; only a run id can be shown.
    let run = crate::run::is_run_id(&q.run).then_some(q.run.as_str());
    let knows = if run.is_some() {
        "You know what the question itself says and what you can learn from \
         its run by the read-only investigation described below."
    } else {
        "You know only what the question itself says."
    };
    let mut s = format!(
        "This question was filed by magi (node `{}`, seat `{}`) with no agent \
         waiting on it. Magi records the owner's answer and the component that \
         asked applies it; you apply nothing. {knows} A choice whose effect you \
         cannot read is not yours to guess: do not settle it, ask the owner \
         what they mean with `--thread` instead.",
        q.node, q.seat
    );
    if let Some(run) = run {
        let artifacts = home.join("runs").join(run).join("artifacts");
        s.push_str(&format!(
            "\n\nThis question concerns run `{run}`. You may investigate it on \
             your own, read-only: run `magi show {run}` and read files under \
             `{}`. When the owner asks about details (for example which part \
             was wrong), look it up yourself and answer; do not reply that you \
             cannot see the details, and do not tell the owner to run `magi show` \
             themselves. Investigating never changes anything: edit, commit, push \
             and apply nothing. If the run cannot be read, say exactly that and do \
             not guess at the cause or at what an option does.",
            artifacts.display()
        ));
    }
    if !q.choices.is_empty() {
        s.push_str("\n\nWhat each option does:");
        for c in &q.choices {
            match actions.get(c) {
                Some(a) => s.push_str(&format!("\n- `{c}`: also {}", a.describe())),
                None => s.push_str(&format!(
                    "\n- `{c}`: recorded as the answer (nothing more is known)"
                )),
            }
        }
    }
    s
}

/// Is picking `label` on `q` something that cannot be taken back, so that
/// `--settle` must hold the owner's words to the same mechanical standard as a
/// merge (`land::unhedged`)? Discarding a task deletes it; a divergence answer
/// drops commits.
pub fn destructive(q: &Question, label: &str) -> bool {
    let at = q.choices.iter().position(|c| c == label);
    match q.node.as_str() {
        crate::triage::NODE => at == Some(2),
        crate::triage::DEPS_NODE => at == Some(1),
        n => n == crate::reconcile::NODE && q.seat == crate::reconcile::SEAT,
    }
}

/// Does `--settle` hold `q` to the merge-approval rules (the verbatim quote of
/// the latest message for `merge`, the whole message for `hold`)? A merge
/// approval, and any other served question that offers `merge` (the release
/// watcher's local-mode approval merges just as irreversibly).
pub fn merge_gated(q: &Question) -> bool {
    q.node == crate::land::APPROVAL_NODE
        || (kind_of(q).is_some_and(|k| k != Kind::Conduct)
            && q.choices.iter().any(|c| c == crate::land::APPROVE))
}

/// Does `q`'s clock run from `asked_at` and never move on a reply? True for the
/// questions that something other than the waiter retires: a merge approval
/// (land) and a release-watch question (the watcher, by silence being a hold).
pub fn fixed_clock(q: &Question) -> bool {
    matches!(kind_of(q), Some(Kind::Land | Kind::Release)) || q.node == crate::github_text::ASK_NODE
}

/// Second after which nobody is to be started or kept on `q`.
///
/// A conductor question runs from its last activity (`magi ask --thread`
/// re-arms it, and the waiter retires it). A merge approval never moves: it
/// runs from `asked_at`, exactly where `daemon::land_resume_state` abandons it,
/// so a conversation cannot stretch the hold and land stays the only place that
/// retires one.
pub fn deadline(q: &Question, default_timeout: u64) -> i64 {
    let secs = if q.answer_timeout > 0 {
        q.answer_timeout
    } else {
        default_timeout
    };
    let from = if fixed_clock(q) {
        q.asked_at.as_second()
    } else {
        q.last_activity()
    };
    from.saturating_add(secs as i64)
}

/// Can `magi serve` start a deputy at all under `cfg`? Not when deputies are
/// switched off (`daemon.max_deputies = 0`) or the config could not be read.
///
/// Also false when the agent the deputy would run as cannot be resolved
/// (`agent` is the deputy's recorded agent, empty when it has none): `turn`
/// fails on that before it counts a start, so it would never reach
/// [`MAX_STARTS`].
pub fn can_start(cfg: Option<&Config>, agent: &str) -> bool {
    cfg.is_some_and(|c| {
        c.daemon.max_deputies > 0
            && ((!agent.is_empty() && c.agent(agent).is_ok()) || c.resolve_roles().is_ok())
    })
}

/// The agent a question's deputy records, empty when it has none yet.
pub fn agent_of(q: &Question) -> &str {
    q.deputy.as_ref().map_or("", |d| d.agent.as_str())
}

/// Has this question's deputy run out of starts, or can none ever start, with
/// the deadline gone?
///
/// Then nothing will ever read an unread say, and the waiter must retire the
/// question anyway instead of deferring to a deputy that no longer starts.
/// `startable` is [`can_start`]; a fresh lease is the caller's to check.
pub fn exhausted_past_deadline(
    q: &Question,
    startable: bool,
    default_timeout: u64,
    now: Timestamp,
) -> bool {
    q.status.open()
        && q.deputy
            .as_ref()
            .is_some_and(|d| d.starts >= MAX_STARTS || !startable)
        && now.as_second() > deadline(q, default_timeout)
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

    /// Give a question the record a deputy needs: the deputy itself (with a
    /// brief rebuilt from what the question stored, when it was filed before
    /// deputies existed - a lost reason is not invented) and the deadline.
    ///
    /// A conductor question also gets the working directory the waiter and
    /// `magi ask --wait` use. Every other kind never does: `cwd` is what makes a
    /// question the waiter's (it would resume and expire it beside the deputy),
    /// and a release-watch question, a merge approval, a triage question and a
    /// fallback one have no asker to resume.
    fn attach(&self, q: &Question, kind: Kind) -> Option<Question> {
        let default_timeout = self.default_timeout();
        let repo = self.fallback_repo.to_string_lossy().into_owned();
        let state = match kind {
            Kind::Land => crate::run::RunState::load(&q.run).ok(),
            Kind::Conduct | Kind::Release | Kind::Triage | Kind::Generic => None,
        };
        // The deadline land itself enforces for this run.
        let timeout = state
            .as_ref()
            .map_or(default_timeout, |s| s.config.graph.answer_timeout);
        self.store
            .update(&q.id, |r| {
                if r.deputy.is_none() {
                    r.deputy = Some(Deputy::new(match kind {
                        Kind::Conduct => brief(&r.run, &r.detail, &r.choices, &r.actions),
                        Kind::Land => crate::land::deputy_brief(r, state.as_ref()),
                        Kind::Release => crate::release_watch::deputy_brief(r, &self.home),
                        Kind::Triage => crate::triage::deputy_brief(
                            r,
                            &crate::queue::Queue::at(self.home.join("queue")),
                        ),
                        Kind::Generic => generic_brief(r, &r.actions, &self.home),
                    }));
                }
                if kind == Kind::Conduct && r.cwd.is_none() {
                    r.cwd = Some(repo.clone());
                }
                if r.answer_timeout == 0 {
                    r.answer_timeout = match kind {
                        Kind::Conduct => default_timeout,
                        Kind::Land => timeout,
                        Kind::Release | Kind::Triage | Kind::Generic => default_timeout,
                    };
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
            let Some(kind) = kind_of(&q) else {
                continue;
            };
            if !q.status.open() {
                continue;
            }
            let needs_cwd = kind == Kind::Conduct && q.cwd.is_none();
            let q = if q.deputy.is_none() || needs_cwd || q.answer_timeout == 0 {
                match self.attach(&q, kind) {
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
            if now.as_second() > deadline(&q, self.default_timeout())
                && q.unread_from_owner().is_none()
            {
                continue;
            }
            if self.inflight.len() >= self.max || !can_start(self.cfg.as_ref(), dep.agent.as_str())
            {
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

    /// The question settled or expired during the handover: release it without
    /// starting the long turn. The start stays counted - the handover ran.
    fn park_quietly(&self, id: &str) {
        let _ = self.store.update(id, |r| {
            r.waiter = None;
            Ok(())
        });
        self.store.drop_lease(id);
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
        // A release-watch question has no `cwd`; its watch record names the
        // checkout the pull request belongs to.
        let recorded = q.cwd.clone().or_else(|| match kind_of(&q) {
            Some(Kind::Release) => {
                crate::release_watch::state_for_question(&self.home, &q.id).map(|st| st.repo)
            }
            // These record a task id in `run` (or a run id for a divergence's
            // sibling kinds): the task's repository, when it can be found.
            Some(Kind::Triage | Kind::Generic) => crate::queue::Queue::at(self.home.join("queue"))
                .get(&q.run)
                .ok()
                .map(|t| t.repo.to_string_lossy().into_owned())
                .filter(|r| !r.is_empty()),
            _ => None,
        });
        let cwd = recorded
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| self.fallback_repo.clone());

        // A settle's note is part of what the seat said, so a resumed seat
        // reads its own report back.
        let bodies: Vec<String> = q
            .thread
            .iter()
            .map(|t| match &t.note {
                Some(n) => format!("{}\n(note: {n})", t.body),
                None => t.body.clone(),
            })
            .collect();
        let thread: Vec<(&str, &str)> = q
            .thread
            .iter()
            .zip(&bodies)
            .map(|(t, body)| {
                (
                    if t.who == Who::Operator {
                        "operator"
                    } else {
                        "agent"
                    },
                    body.as_str(),
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
            kind: kind_of(&q).unwrap_or(Kind::Conduct),
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

        let left = (deadline(&q, cfg.graph.answer_timeout) - now.as_second()).max(0) as u64;
        let artifacts = self.store.root().join(format!("{}.deputy", q.id));
        let cache_dir = cfg.cache_dir();
        // `magi ask` writes the question record and its lock, which a read-only
        // sandbox refuses, so the seat cannot be read-only. It is told never to
        // edit anything; the daemon, not the deputy, applies outcomes.
        let allow_write = true;
        // The question store is outside the repository, and `magi ask` writes it.
        // A merge approval's deputy also files follow-up tasks with `magi task
        // add`, which writes the queue; that is the one other place it writes.
        let writable = [
            self.store.root().to_path_buf(),
            crate::queue::Queue::open().root().to_path_buf(),
        ];
        macro_rules! invocation {
            ($prompt:expr, $stem:expr, $timeout:expr) => {
                Invocation {
                    cwd: &cwd,
                    prompt: $prompt,
                    timeout: $timeout,
                    allow_write,
                    unsandboxed: false,
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
                    writable: &writable,
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
                kind: kind_of(&q).unwrap_or(Kind::Conduct),
                language: &cfg.graph.language,
            });
            let hstem = format!("handover-{starts}");
            // Bounded by the question's own deadline, not only by its own cap: the
            // lease it beats would otherwise keep an expired question alive.
            let hlimit = HANDOVER_TIMEOUT.min(Duration::from_secs(left.max(1)));
            let hinv = invocation!(&hbody, &hstem, hlimit);
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

        // The handover may have been slow: look at the question again, and
        // measure the long turn from what is left now.
        let left = if early.is_none() && !resumed && cfg.graph.sessions {
            let now = Timestamp::now();
            let again = self.store.get(&q.id)?;
            let left = (deadline(&again, cfg.graph.answer_timeout) - now.as_second()).max(0) as u64;
            if !again.status.open() || (left == 0 && again.unread_from_owner().is_none()) {
                self.park_quietly(&q.id);
                return Ok(());
            }
            left
        } else {
            left
        };
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
