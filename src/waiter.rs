//! The waiter: something that keeps waiting on a question after the process
//! that asked it is gone.
//!
//! `magi ask` blocks in the agent's own shell tool, and that tool kills a wait
//! that runs long (see `ask::WAIT_SLICE`). An agent can also background the
//! call and simply finish. Either way the question outlives the only process
//! that would have read the owner's answer, and the owner's reply lands in a
//! file nobody is polling - the phone then says "waiting for the agent" while
//! there is no agent to wait for.
//!
//! This module is the guarantee that something is always on the other end. It
//! runs inside `magi serve`, on its own cadence rather than the queue's, and
//! for every question a `magi ask` filed it decides one of three things:
//!
//! - **the asker is still there** (its [`Lease`] is fresh): do nothing. A
//!   second agent must never be started while the first is blocked;
//! - **the asker is gone and the owner has said or answered something the agent
//!   has not read**: resume the *same seat's* CLI session with the question,
//!   the thread so far and the owner's words, so the agent that holds the
//!   context carries on;
//! - **nobody answered before `answer_timeout`**: abandon it, in the same words
//!   the asker would have used.
//!
//! # Shape
//!
//! Same split as [`crate::queue`] and [`crate::ask`]: [`decide`] is pure - a
//! question, a lease, a clock and a bool in, an [`Action`] out - and every
//! filesystem or process effect is in [`Waiter::tick`]. State lives on disk (the
//! question record and its lease), so a restarted daemon picks up exactly where
//! the last one stopped, including an answer nobody has read yet.
//!
//! # What it will not do
//!
//! It never starts a fresh consultant. If the seat's session cannot be resumed
//! (`agent::has_session` says no, the working directory is gone, the run is
//! over, the agent left the roster) the question stays as it is, the owner
//! gets a notification saying why, and nothing else runs. A new agent would
//! have to be caught up on everything the first one knew, and would act on the
//! owner's words without the context they were written against.
//!
//! It never assumes a resume worked either: a turn that fails or times out
//! leaves the word undelivered, and a later tick tries again.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use jiff::Timestamp;

use crate::agent::{self, Invocation, SeatState};
use crate::ask::{Lease, Question, QuestionStatus, Questions, Waiter as Note, WaiterKind, Who};
use crate::config::{AgentSpec, Config};
use crate::notices::{self, Notice};
use crate::prompt;
use crate::run::RunState;

/// How often the waiter looks at the store.
pub const TICK: Duration = Duration::from_secs(5);

/// Wall-clock limit for one resumed turn. A resumed agent may go on to do real
/// work once it has the owner's word, but it is one conversation turn, not a
/// whole node.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long a failed or refused delivery is left alone before the same input
/// is tried again. Long enough not to hammer a seat that is out of quota,
/// short enough that a fixed problem does not wait a day.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// What the agent has not read yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Word {
    /// The owner spoke back without deciding.
    Said(String),
    /// The owner decided.
    Answered(String),
}

/// What to do about one question, this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing: not ours, someone is waiting, or nothing new to hand over.
    Idle,
    /// The deadline passed with no answer and nobody waiting.
    Expire,
    /// Resume the asking seat with this.
    Deliver(Word),
}

/// The whole decision, with no I/O.
///
/// `seat_busy` is whether the asking seat's CLI is still mid-turn - the agent
/// that exited its `magi ask` may not have exited itself, and resuming a
/// conversation that is running would fork it.
///
/// Order matters: a word the owner gave before the deadline is delivered even
/// if the deadline has passed by the time the waiter looks, and only then does
/// an unanswered question expire.
pub fn decide(
    q: &Question,
    lease: Option<&Lease>,
    seat_busy: bool,
    default_timeout: u64,
    now: Timestamp,
) -> Action {
    if q.cwd.is_none() || lease.is_some_and(|l| l.fresh(now)) {
        return Action::Idle;
    }
    match q.status {
        QuestionStatus::Abandoned => Action::Idle,
        QuestionStatus::Answered => match q.resolution() {
            Some(a) if !q.answer_delivered && !seat_busy => Action::Deliver(Word::Answered(a)),
            _ => Action::Idle,
        },
        QuestionStatus::Open => {
            if let Some(said) = q.unread_from_owner() {
                if seat_busy {
                    return Action::Idle;
                }
                return Action::Deliver(Word::Said(said));
            }
            let secs = if q.answer_timeout > 0 {
                q.answer_timeout
            } else {
                default_timeout
            };
            let deadline = q.last_activity().saturating_add(secs as i64);
            if now.as_second() > deadline {
                Action::Expire
            } else {
                Action::Idle
            }
        }
    }
}

/// Everything needed to resume one seat.
struct Target {
    spec: AgentSpec,
    seat: SeatState,
    cwd: PathBuf,
    sessions: bool,
    allow_write: bool,
    language: String,
}

/// The waiter's state: the store it watches and what it must remember between
/// ticks (only which inputs it has already failed on).
pub struct Waiter {
    store: Questions,
    home: PathBuf,
    /// The config the conductor runs under, for a question the conductor asked
    /// (it has no run whose snapshot could say).
    conduct_cfg: Option<Config>,
    /// `answer_timeout` for a question that did not record its own.
    default_timeout: u64,
    /// Inputs already tried and failed, by question id: `(key, until)`.
    memo: HashMap<String, (String, Instant)>,
}

impl Waiter {
    /// A waiter over `store`, with `home` as the magi home its runs live under.
    pub fn new(store: Questions, home: PathBuf, conduct_cfg: Option<Config>) -> Self {
        let default_timeout = conduct_cfg
            .as_ref()
            .map_or(Config::default().graph.answer_timeout, |c| {
                c.graph.answer_timeout
            });
        Self {
            store,
            home,
            conduct_cfg,
            default_timeout,
            memo: HashMap::new(),
        }
    }

    /// Is this question's asking seat still mid-turn?
    fn seat_busy(&self, q: &Question, now: Timestamp) -> bool {
        if q.run == crate::conduct::NODE {
            return crate::conduct::busy(&self.home);
        }
        RunState::load_under(&q.run, &self.home).is_ok_and(|s| {
            s.seats_active()
                .any(|(k, a)| *k == q.seat && a.remaining_secs(now) > 0)
        })
    }

    /// Look at every question once and act on what needs acting on. `halt` is
    /// asked between questions and during a delivery: true means the daemon is
    /// parking, and whatever is in flight is dropped, undelivered and
    /// resumable.
    pub async fn tick(&mut self, now: Timestamp, halt: &(dyn Fn() -> bool + Sync)) {
        for q in self.store.list() {
            if halt() {
                return;
            }
            let lease = self.store.read_lease(&q.id);
            match decide(&q, lease.as_ref(), false, self.default_timeout, now) {
                Action::Idle => {}
                Action::Expire => self.expire(&q),
                Action::Deliver(word) => {
                    // A question with a deputy is the deputy's to read
                    // (`crate::deputy`); resuming the conductor's own seat
                    // here would fork a conversation that is not its own.
                    if q.deputy.is_some() {
                        // Unless that deputy is spent: then nobody will read
                        // the word, and the deadline still has to retire the
                        // question.
                        if crate::deputy::exhausted_past_deadline(
                            &q,
                            crate::deputy::can_start(
                                self.conduct_cfg.as_ref(),
                                crate::deputy::agent_of(&q),
                            ),
                            self.default_timeout,
                            now,
                        ) {
                            self.expire(&q);
                        }
                        continue;
                    }
                    if self.seat_busy(&q, now) {
                        continue;
                    }
                    let key = format!("{}:{:?}", q.thread.len(), word);
                    if matches!(self.memo.get(&q.id), Some((k, until)) if *k == key && Instant::now() < *until)
                    {
                        continue;
                    }
                    if let Err(e) = self.deliver(&q, &word, halt).await {
                        tracing::warn!("question {}: {e:#}", q.short());
                    }
                }
            }
        }
    }

    /// Abandon an unanswered question whose deadline passed with nobody
    /// waiting - the words the asker's own wait would have used.
    fn expire(&self, q: &Question) {
        let secs = if q.answer_timeout > 0 {
            q.answer_timeout
        } else {
            self.default_timeout
        };
        let startable =
            crate::deputy::can_start(self.conduct_cfg.as_ref(), crate::deputy::agent_of(q));
        let why = format!("no answer within {}s of asking", secs.max(1));
        let done = self.store.update(&q.id, |r| {
            // Decided again on the record as it is now: the owner may have
            // spoken since the tick read it, and a word given in time is
            // delivered, not abandoned.
            let now = Timestamp::now();
            let lease = self.store.read_lease(&r.id);
            if decide(r, lease.as_ref(), false, self.default_timeout, now) != Action::Expire
                && !(lease.as_ref().is_none_or(|l| !l.fresh(now))
                    && crate::deputy::exhausted_past_deadline(
                        r,
                        startable,
                        self.default_timeout,
                        now,
                    ))
            {
                return Ok(false);
            }
            r.abandon(&why);
            r.waiter = None;
            Ok(true)
        });
        match done {
            Ok((_, false)) => {}
            Ok((_, true)) => {
                self.store.drop_lease(&q.id);
                tracing::warn!(
                    "question {} went unanswered for {secs}s with nobody waiting; \
                     it stays as the record of it",
                    q.short()
                );
            }
            Err(e) => tracing::warn!("could not expire question {}: {e:#}", q.short()),
        }
    }

    /// Work out how to resume the seat, or why it cannot be.
    fn target(&self, q: &Question) -> std::result::Result<Target, String> {
        let cwd = q
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .ok_or("the directory the agent was working in is gone")?;
        let (cfg, seat, allow_write) = if q.run == crate::conduct::NODE {
            let cfg = self
                .conduct_cfg
                .clone()
                .ok_or("the conductor's configuration is not available")?;
            let seat = crate::conduct::load_seat(&self.home)
                .ok_or("the conductor has no recorded session")?;
            (cfg, seat, false)
        } else {
            let state = RunState::load_under(&q.run, &self.home)
                .map_err(|_| "the run that asked is gone".to_owned())?;
            if !state.status.resumable() {
                return Err(format!(
                    "the run that asked has already {}",
                    state.status.as_str()
                ));
            }
            let seat = state
                .seats
                .get(&q.seat)
                .cloned()
                .ok_or("the run has no record of the asking seat")?;
            let write = matches!(q.node.as_str(), "implement" | "fix");
            (state.config, seat, write)
        };
        let spec = cfg
            .agent(&seat.agent)
            .map_err(|_| format!("agent `{}` is no longer in the roster", seat.agent))?
            .clone();
        if !agent::has_session(spec.kind, &seat, cfg.graph.sessions) {
            return Err("the seat has no session to resume".to_owned());
        }
        Ok(Target {
            spec,
            seat,
            cwd,
            sessions: cfg.graph.sessions,
            allow_write,
            language: cfg.graph.language.clone(),
        })
    }

    /// Say why the owner's word cannot reach anybody, once per input.
    fn refuse(&mut self, q: &Question, key: String, why: &str) {
        tracing::warn!("question {} cannot be delivered: {why}", q.short());
        notices::raise_in(
            &self.home,
            Notice::warn(
                &format!("question:{}", q.id),
                format!(
                    "Question {} \"{}\": what you said cannot reach the agent \
                     ({why}). It is recorded, but nothing will read it.",
                    q.short(),
                    q.summary
                ),
            ),
        );
        let _ = self.store.update(&q.id, |r| {
            r.waiter = None;
            Ok(())
        });
        self.memo
            .insert(q.id.clone(), (key, Instant::now() + RETRY_AFTER));
    }

    /// Resume the asking seat with the owner's word.
    async fn deliver(
        &mut self,
        q: &Question,
        word: &Word,
        halt: &(dyn Fn() -> bool + Sync),
    ) -> Result<()> {
        let key = format!("{}:{:?}", q.thread.len(), word);
        let mut target = match self.target(q) {
            Ok(t) => t,
            Err(why) => {
                self.refuse(q, key, &why);
                return Ok(());
            }
        };

        let claim = self.store.root().join(format!("{}.claim", q.id));
        if !take_claim(&claim, DELIVERY_TIMEOUT + Duration::from_secs(60)) {
            return Ok(());
        }
        let _release = Release(claim);

        // Re-read under the claim: the asker may have come back, or the owner
        // may have said more, since the tick's copy was taken.
        let q = self.store.get(&q.id)?;
        let now = Timestamp::now();
        let lease = self.store.read_lease(&q.id);
        let Action::Deliver(word) = decide(&q, lease.as_ref(), false, self.default_timeout, now)
        else {
            return Ok(());
        };
        let snapshot = q.thread.len();

        self.store.beat(&q.id, WaiterKind::Daemon);
        self.store.update(&q.id, |r| {
            r.waiter = Some(Note {
                kind: WaiterKind::Daemon,
                since: now,
            });
            Ok(())
        })?;
        tracing::info!(
            "question {}: the asker is gone, resuming seat {} with the owner's word",
            q.short(),
            q.seat
        );

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
        // What the owner said and the agent has not read has its own section in
        // the prompt, so the recap stops at what it already read.
        let shown = match word {
            Word::Said(_) => &thread[..q.delivered_turns.min(thread.len())],
            Word::Answered(_) => &thread[..],
        };
        let owner_word = match &word {
            Word::Said(s) => prompt::OwnerWord::Said(s),
            Word::Answered(a) => prompt::OwnerWord::Answered(a),
        };
        let body = prompt::question_resumed(
            &q.id,
            &q.summary,
            &q.detail,
            shown,
            &owner_word,
            &target.language,
        );

        let artifacts = self.store.root().join(format!("{}.delivery", q.id));
        let stem = format!("deliver-{}", now.as_second());
        let cache_dir = None;
        let inv = Invocation {
            cwd: &target.cwd,
            prompt: &body,
            timeout: DELIVERY_TIMEOUT,
            allow_write: target.allow_write,
            sessions: target.sessions,
            artifacts: &artifacts,
            stem: &stem,
            run: &q.run,
            node: &q.node,
            cache_dir,
            attachments: &[],
            writable: &[],
        };

        let store = self.store.clone();
        let id = q.id.clone();
        let out = {
            let fut = agent::invoke(&target.spec, &mut target.seat, &inv);
            tokio::pin!(fut);
            let mut beat = tokio::time::interval(Duration::from_secs(1));
            let mut beats = 0u32;
            loop {
                tokio::select! {
                    r = &mut fut => break Some(r),
                    _ = beat.tick() => {
                        if halt() {
                            break None;
                        }
                        beats += 1;
                        if beats % 20 == 0 {
                            store.beat(&id, WaiterKind::Daemon);
                        }
                    }
                }
            }
        };

        let Some(out) = out else {
            // Parking: leave everything as it was, resumable.
            let _ = self.store.update(&q.id, |r| {
                r.waiter = None;
                Ok(())
            });
            return Ok(());
        };
        let out = match out {
            Ok(o) if o.usable() => o,
            other => {
                let why = match other {
                    Ok(o) if o.timed_out => "the resumed turn timed out".to_owned(),
                    Ok(o) if o.quota_exhausted() => "the agent is out of quota".to_owned(),
                    Ok(o) => format!("the resumed turn failed (exit {:?})", o.exit_code),
                    Err(e) => format!("the agent could not be started: {e:#}"),
                };
                self.refuse(&q, key, &why);
                return Ok(());
            }
        };

        let text = out.text.trim().to_owned();
        self.store.update(&q.id, |r| {
            r.waiter = None;
            match &word {
                Word::Answered(_) => r.answer_delivered = true,
                Word::Said(_) => {
                    r.delivered_turns = r.delivered_turns.max(snapshot);
                    // An agent that answered in prose instead of calling
                    // `magi ask --thread` still spoke; keep it on the record so
                    // the owner reads it. If it did use --thread, or the owner
                    // said more meanwhile, the record already moved on.
                    let agent_spoke = r.thread[snapshot.min(r.thread.len())..]
                        .iter()
                        .any(|t| t.who == Who::Agent);
                    if !agent_spoke && r.thread.len() == snapshot && r.status.open() {
                        let choices = r.choices.clone();
                        r.reply(text, choices)?;
                    }
                }
            }
            Ok(())
        })?;
        self.memo.remove(&q.id);
        Ok(())
    }
}

/// Take the delivery claim, so two waiters (`magi serve` twice, or a stray
/// second daemon) cannot resume the same seat at once. A claim older than a
/// whole delivery plus slack was left by a process that died.
pub(crate) fn take_claim(path: &std::path::Path, stale_after: Duration) -> bool {
    let attempt = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .is_ok()
    };
    if attempt() {
        return true;
    }
    let stale = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > stale_after);
    if stale {
        let _ = std::fs::remove_file(path);
        return attempt();
    }
    false
}

/// Removes the delivery claim when the delivery ends, however it ends.
pub(crate) struct Release(pub(crate) PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The waiter's loop, until `stop` is asked for.
///
/// A stop ends the loop between ticks; a park also drops a delivery in flight.
/// Neither writes anything to a question: the lease just ages out and the next
/// waiter - this daemon restarted, or another - reads the same state off disk.
pub async fn run(mut waiter: Waiter, stop: crate::daemon::Stop) {
    let halt = {
        let stop = stop.clone();
        move || stop.parking()
    };
    while !stop.stopped() {
        waiter.tick(Timestamp::now(), &halt).await;
        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn asked(at: i64) -> Question {
        let mut q = Question::new(
            "run".into(),
            "implement".into(),
            "impl-A".into(),
            "which?".into(),
            String::new(),
            vec![],
        );
        q.cwd = Some("/tmp".into());
        q.asked_at = ts(at);
        q.answer_timeout = 1000;
        q
    }

    fn lease(at: i64) -> Lease {
        Lease {
            kind: WaiterKind::Asker,
            pid: 1,
            beat_at: ts(at),
        }
    }

    #[test]
    fn a_fresh_lease_means_nobody_else_acts() {
        let mut q = asked(0);
        q.say("why?").unwrap();
        assert_eq!(
            decide(&q, Some(&lease(95)), false, 86_400, ts(100)),
            Action::Idle
        );
    }

    #[test]
    fn a_stale_lease_delivers_what_the_owner_said() {
        let mut q = asked(0);
        q.say("why?").unwrap();
        assert_eq!(
            decide(&q, Some(&lease(0)), false, 86_400, ts(500)),
            Action::Deliver(Word::Said("why?".into()))
        );
        assert_eq!(
            decide(&q, None, true, 86_400, ts(500)),
            Action::Idle,
            "a seat still mid-turn is not resumed"
        );
    }

    #[test]
    fn an_answer_nobody_read_is_delivered_once() {
        let mut q = asked(0);
        q.choices = vec!["A".into()];
        q.answer(crate::ask::Answer::Choice("A".into())).unwrap();
        assert_eq!(
            decide(&q, None, false, 86_400, ts(10)),
            Action::Deliver(Word::Answered("A".into()))
        );
        q.answer_delivered = true;
        assert_eq!(decide(&q, None, false, 86_400, ts(10)), Action::Idle);
    }

    #[test]
    fn only_asked_questions_and_only_before_the_deadline() {
        let mut q = asked(0);
        assert_eq!(decide(&q, None, false, 86_400, ts(999)), Action::Idle);
        assert_eq!(decide(&q, None, false, 86_400, ts(1001)), Action::Expire);
        q.cwd = None;
        assert_eq!(decide(&q, None, false, 86_400, ts(1001)), Action::Idle);
    }

    #[test]
    fn the_deadline_runs_from_the_last_turn_not_from_asking() {
        let mut q = asked(0);
        q.thread.push(crate::ask::Turn {
            who: Who::Agent,
            body: "context".into(),
            at: ts(4000),
        });
        q.delivered_turns = 1;
        assert_eq!(decide(&q, None, false, 86_400, ts(4500)), Action::Idle);
        assert_eq!(decide(&q, None, false, 86_400, ts(5001)), Action::Expire);
    }

    #[test]
    fn a_word_given_in_time_is_delivered_after_the_deadline() {
        let mut q = asked(0);
        q.say("wait").unwrap();
        assert!(matches!(
            decide(&q, None, false, 86_400, ts(5000)),
            Action::Deliver(_)
        ));
    }

    #[test]
    fn an_action_answer_is_delivered_until_the_daemon_marks_it_handled() {
        let mut q = asked(0);
        q.choices = vec!["A".into()];
        q.actions
            .insert("A".into(), crate::ask::ChoiceAction::Requeue);
        q.answer(crate::ask::Answer::Choice("A".into())).unwrap();
        assert_eq!(
            decide(&q, None, false, 86_400, ts(10)),
            Action::Deliver(Word::Answered("A".into())),
            "an action nobody applied must not swallow the answer"
        );
        q.answer_delivered = true;
        assert_eq!(decide(&q, None, false, 86_400, ts(10)), Action::Idle);
    }
}
