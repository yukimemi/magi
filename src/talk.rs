//! The standing conversation: a place to think out loud with an agent between
//! tasks, reachable from a phone.
//!
//! [`crate::chat`] is an interview with one purpose - arrive at a task file
//! and file it - and it ends the moment that happens. This module is the other
//! kind of conversation an operator wants: one that stays open. Ask a
//! question, have the agent read a file or run a command to check something,
//! talk through an idea, and when it is time to act, tell it to file the work
//! rather than do it here. The conversation does not end; it is what the
//! operator opens the next time something comes up.
//!
//! # Talking is not implementing
//!
//! Every turn here runs with `allow_write: false` - the same restriction
//! [`crate::chat`] puts on its own interview, for the same reason. An agent
//! that can edit files while the operator is mid-thought can leave the
//! checkout in a state neither of them chose. When the operator wants a
//! change made, the agent is told to run `magi task add --solo`
//! ([`briefing`]) rather than reach for an editor: the change goes through
//! magi's own queue, on the repository's own terms, and the operator can
//! watch it happen instead of trusting that it did.
//!
//! `--solo` rather than a plain `magi task add` is the point of pairing this
//! module with [`crate::queue::Task::solo`]. A task that came out of a
//! conversation the operator just had is a decision already made, not a
//! design question worth three independent takes - so it runs through one
//! implementer and straight into review, the way [`crate::graph::Runner`]
//! already degrades a single-candidate run.
//!
//! # Shape
//!
//! The same split [`crate::chat`] and [`crate::queue`] use: [`Talk`] is data
//! plus pure helpers, [`Talks`] owns the I/O and is constructed with its root,
//! so every test here drives a real store in a temp directory rather than the
//! operator's own home.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::config::Config;
use crate::plan;
use crate::queue::{Queue, Source, Task};

/// On-disk format for a conversation. Bumped when a field's meaning changes.
pub const SCHEMA: u32 = 1;

/// Wall-clock limit for one agent turn.
///
/// Fifteen minutes, three times [`crate::chat::TURN_TIMEOUT`]. A planning turn
/// answers a question about intent; a turn here is expected to run several
/// shell commands and read their output before answering one - "what does
/// this function do", "is this still true", "run the tests and tell me" - and
/// a five-minute budget cuts that off mid-investigation on exactly the
/// conversation meant to support it.
const TURN_TIMEOUT: Duration = Duration::from_secs(900);

/// Seat name for the conversation's agent, scoping its CLI-side session away
/// from every other seat magi ever opens - the same rule [`crate::chat`]
/// applies to its own interviewer.
const SEAT: &str = "talk";

/// Prefix on a turn magi wrote rather than an agent. See
/// [`crate::chat::MAGI_NOTE`], which this mirrors.
const MAGI_NOTE: &str = "magi: ";

/// Who said something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    /// The operator.
    Operator,
    /// The conversation's agent - or magi itself, reporting that a turn
    /// failed. See [`MAGI_NOTE`].
    Agent,
}

/// One message in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// Who wrote it.
    pub who: Who,
    /// What they said.
    pub body: String,
    /// When it was said.
    pub at: Timestamp,
}

/// Where a conversation is in its life. Unlike [`crate::chat::ChatStatus`]
/// there is no `filed`: this conversation can file any number of tasks
/// without ending, so it only ever moves once, from open to closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TalkStatus {
    /// Still open; the operator may say more, and may have already filed work
    /// out of it.
    Open,
    /// Closed by hand. Kept on disk as a record.
    Closed,
}

impl TalkStatus {
    /// Is this conversation still live?
    pub fn open(self) -> bool {
        matches!(self, Self::Open)
    }

    /// Wire form, for the phone and for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// One standing conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Talk {
    /// On-disk format version.
    pub schema: u32,
    /// Conversation id, e.g. `20260904-014455-ab12`.
    pub id: String,
    /// Repository this conversation is about.
    pub repo: PathBuf,
    /// Roster agent id holding the conversation.
    pub agent: String,
    /// Current state.
    pub status: TalkStatus,
    /// Everything said, oldest first.
    pub turns: Vec<Turn>,
    /// When the conversation was opened.
    pub created_at: Timestamp,
    /// Last change to this file.
    pub updated_at: Timestamp,
    /// The CLI-side conversation, so a turn after the first costs one
    /// sentence instead of the whole transcript. Not `pub` for the same
    /// reason [`crate::chat::Chat`]'s is not: it is magi's bookkeeping, and a
    /// caller that edited it would detach the record from the conversation
    /// the model actually holds.
    seat: SeatState,
}

impl Talk {
    /// Short form used in lists and notifications, matching a run's short id.
    pub fn short(&self) -> &str {
        short(&self.id)
    }
}

/// A conversation store on disk.
#[derive(Debug, Clone)]
pub struct Talks {
    root: PathBuf,
    /// Serializes the read-modify-write cycle that reads a talk, decides
    /// something from its `status`, and writes the whole record back.
    /// [`close`], [`record`] and the tail of [`turn`] all take this before
    /// that cycle rather than after just the read: a re-read narrows the
    /// window another writer can land in, but does not close it, since
    /// nothing stopped that other writer's own put from landing between this
    /// call's re-read and its own put. Shared across every clone, since every
    /// clone is a handle onto the same files.
    lock: Arc<Mutex<()>>,
}

impl Talks {
    /// The operator's conversations, `<home>/talks`.
    pub fn open() -> Self {
        Self::at(crate::run::home().join("talks"))
    }

    /// A store at an explicit root. Tests use this, which is why none of them
    /// need the operator's real home.
    pub fn at(root: PathBuf) -> Self {
        Self {
            root,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Claim the right to read-modify-write a talk's `status`. A plain
    /// `std::sync::Mutex`, not an async one: every caller holds it across a
    /// handful of small file operations and never across an `.await`, so
    /// blocking the thread briefly is the right tool, not a reason to reach
    /// for `tokio::sync::Mutex`. Poisoning recovers rather than propagates -
    /// one panicking caller must not wedge every talk in the store the way it
    /// would wedge the loop's own lock; see [`crate::web`]'s `lock_or_recover`,
    /// which this mirrors.
    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Directory holding the conversation files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path for one conversation id.
    pub fn path_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// Where one conversation's prompts and CLI output are kept, beside the
    /// record rather than inside it - see [`crate::chat::Chats::artifacts_of`].
    pub fn artifacts_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.artifacts"))
    }

    /// Write a conversation, atomically, so a process killed mid-write leaves
    /// the previous state readable rather than a truncated file.
    pub fn put(&self, t: &mut Talk) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        t.updated_at = Timestamp::now();
        let body = serde_json::to_string_pretty(t).context("serialize talk")?;
        let path = self.path_of(&t.id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// Load a conversation by id or unambiguous id prefix.
    pub fn get(&self, id: &str) -> Result<Talk> {
        let resolved = self.resolve_id(id)?;
        read_path(&self.path_of(&resolved))
    }

    /// Every conversation on disk: open first, then newest first - the same
    /// ordering [`crate::chat::Chats::list`] uses, for the same reason: what
    /// the operator is still using belongs above what they are done with.
    pub fn list(&self) -> Vec<Talk> {
        let mut all: Vec<Talk> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| read_path(&p).ok())
            .collect();
        all.sort_unstable_by(|a, b| {
            let rank = |t: &Talk| u8::from(!t.status.open());
            rank(a).cmp(&rank(b)).then_with(|| b.id.cmp(&a.id))
        });
        all
    }

    /// Expand an id prefix to exactly one conversation id.
    pub fn resolve_id(&self, prefix: &str) -> Result<String> {
        if self.path_of(prefix).is_file() {
            return Ok(prefix.to_owned());
        }
        let hits: Vec<String> = self
            .list()
            .into_iter()
            .map(|t| t.id)
            .filter(|id| id.starts_with(prefix) || id.ends_with(prefix))
            .collect();
        match hits.len() {
            1 => Ok(hits.into_iter().next().expect("exactly one hit")),
            0 => bail!("no talk matches `{prefix}`"),
            _ => bail!(
                "`{prefix}` matches {} talks: {}",
                hits.len(),
                hits.join(", ")
            ),
        }
    }

    /// Change detection token, the same shape as
    /// [`crate::chat::Chats::revision`]: the newest modification time in the
    /// store, in milliseconds.
    pub fn revision(&self) -> u64 {
        std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter_map(|m| m.modified().ok())
            .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .max()
            .unwrap_or(0)
    }

    /// How many conversations are still open.
    pub fn count_open(&self) -> usize {
        self.list().iter().filter(|t| t.status.open()).count()
    }
}

/// Open a conversation. Unlike [`crate::chat::start`] this takes no agent
/// turn: there is no idea to answer yet, and a conversation the operator has
/// not said anything into yet is a normal, valid thing to have sitting on the
/// phone.
///
/// `agent` is resolved the same way `magi plan` and [`crate::chat::start`]
/// resolve their interviewer: [`plan::pick`] against `[roles] planner`, so
/// this surface adds no configuration of its own.
pub fn begin(store: &Talks, cfg: &Config, repo: PathBuf, agent: Option<&str>) -> Result<Talk> {
    // Absolute, for the same reason `chat::start` canonicalizes: a relative
    // path means the wrong repository once anything other than this process
    // reads it back.
    let repo = repo.canonicalize().unwrap_or(repo);
    let want = agent.or(cfg.roles.planner.as_deref());
    let spec = plan::pick(&cfg.agents, want, &plan::installed)?;

    let now = Timestamp::now();
    let mut talk = Talk {
        schema: SCHEMA,
        id: new_id(),
        repo,
        agent: spec.id.clone(),
        status: TalkStatus::Open,
        turns: Vec::new(),
        created_at: now,
        updated_at: now,
        seat: SeatState::new(SEAT, &spec.id, crate::rng::entropy()),
    };
    store.put(&mut talk)?;
    Ok(talk)
}

/// Append the operator's turn and flush it, without invoking anything.
///
/// Split out of [`say`] for the same reason [`crate::chat::record`] is split
/// out of [`crate::chat::say`]: `POST /api/talks/{id}/say` answers once the
/// message is safely on disk, and runs the agent's half in the background -
/// see that function's doc for why holding the connection for a turn that can
/// run fifteen minutes is the wrong shape for a phone.
pub fn record(talk: &mut Talk, store: &Talks, text: &str) -> Result<String> {
    // `web::talk_say` reads the talk, then awaits config discovery before
    // calling this - a gap a concurrent `POST /api/talks/{id}/close` can land
    // in. The guard held for the rest of this function is what actually closes
    // that gap: re-reading status without it only shrinks the window a
    // concurrent `close` could land in between this call's own read and its
    // `put`, it does not remove it. See [`Talks::guard`] and the matching
    // guard in `turn`, which this mirrors.
    let _guard = store.guard();
    if let Ok(fresh) = store.get(&talk.id) {
        talk.status = fresh.status;
    }
    if !talk.status.open() {
        bail!(
            "talk {} is {} and takes no more turns",
            talk.short(),
            talk.status.as_str()
        );
    }
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to say");
    }
    talk.turns.push(Turn {
        who: Who::Operator,
        body: text.to_owned(),
        at: Timestamp::now(),
    });
    store.put(talk)?;
    Ok(text.to_owned())
}

/// One operator turn and one agent turn, appended - the synchronous form, used
/// by tests and by anything that is fine waiting out the turn itself.
pub async fn say(talk: &mut Talk, store: &Talks, cfg: &Config, text: &str) -> Result<()> {
    let text = record(talk, store, text)?;
    turn(talk, store, cfg, &text).await
}

/// The agent's half of a turn: invoke, append, flush. Pairs with [`record`],
/// the same way [`crate::chat::respond`] pairs with [`crate::chat::record`].
pub async fn respond(talk: &mut Talk, store: &Talks, cfg: &Config, text: &str) -> Result<()> {
    turn(talk, store, cfg, text).await
}

/// Close a conversation. Idempotent: closing an already-closed conversation is
/// not an error, since the operator's intent - "I am done with this" - is
/// already satisfied.
///
/// Re-reads the record under [`Talks::guard`] rather than trusting the
/// caller's copy of `talk`, and writes that fresh copy back rather than the
/// one passed in. `web::talk_close` loads `talk` and calls this right after
/// with no gap of its own, but without the guard that load can still land
/// between a `record` or `turn` elsewhere reading the file and writing it
/// back - and a close built on the older snapshot would put it right back,
/// silently dropping whatever turn the other call had just appended.
pub fn close(talk: &mut Talk, store: &Talks) -> Result<()> {
    let _guard = store.guard();
    let mut fresh = store.get(&talk.id).unwrap_or_else(|_| talk.clone());
    fresh.status = TalkStatus::Closed;
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(())
}

/// Invoke the conversation's agent once and append what it said.
///
/// The first turn ever taken carries the full [`briefing`], because nothing
/// else has told the agent what this conversation is or what it may do.
/// Every turn after that behaves like [`crate::chat`]'s: resend nothing when
/// the CLI can resume its own session, and fall back to [`transcript`] only
/// when it cannot.
async fn turn(talk: &mut Talk, store: &Talks, cfg: &Config, text: &str) -> Result<()> {
    let spec = cfg
        .agents
        .iter()
        .find(|a| a.id == talk.agent)
        .with_context(|| {
            format!(
                "talk {} was opened with agent `{}`, which is no longer in \
                 the roster; restore it in magi.toml or start a new \
                 conversation",
                talk.short(),
                talk.agent
            )
        })?;

    let resuming = agent::has_session(spec.kind, &talk.seat, cfg.graph.sessions);
    let body = if talk.seat.turns == 0 {
        format!(
            "{}\n\n# Operator\n\n{text}",
            briefing(&talk.repo, &cfg.graph.language)
        )
    } else if resuming {
        text.to_owned()
    } else {
        format!("{}\n\n{text}", transcript(talk))
    };

    let artifacts = store.artifacts_of(&talk.id);
    let stem = format!("turn-{}", talk.seat.turns + 1);
    let inv = Invocation {
        cwd: &talk.repo,
        prompt: &body,
        timeout: TURN_TIMEOUT,
        // This conversation never writes to the repository: it tells the
        // operator to run `magi task add --solo` instead, which is what keeps
        // an implementer's diff attributable to a run rather than to a chat
        // nobody reviewed.
        allow_write: false,
        sessions: cfg.graph.sessions,
        artifacts: &artifacts,
        stem: &stem,
        // The conversation's own id, so `magi task add` run from inside it is
        // attributed to this conversation - see `Source::Agent`.
        run: &talk.id,
        node: "chat",
    };

    let outcome = agent::invoke(spec, &mut talk.seat, &inv).await;
    let note = |why: String| Turn {
        who: Who::Agent,
        body: format!("{MAGI_NOTE}{why}"),
        at: Timestamp::now(),
    };
    let (reply, failure) = match outcome {
        Err(e) => (
            note(format!("could not run agent `{}`: {e}", talk.agent)),
            Some(format!("could not run agent `{}`: {e}", talk.agent)),
        ),
        Ok(out) if out.quota_exhausted() => {
            let reset = out
                .quota
                .as_ref()
                .and_then(|q| q.reset.clone())
                .map_or_else(String::new, |r| format!(" (resets {r})"));
            let why = format!(
                "agent `{}` is out of quota{reset}; your message is saved, so \
                 say it again when the window reopens",
                talk.agent
            );
            (note(why.clone()), Some(why))
        }
        Ok(out) if out.timed_out => {
            let why = format!(
                "agent `{}` did not answer within {}s; your message is saved",
                talk.agent,
                TURN_TIMEOUT.as_secs()
            );
            (note(why.clone()), Some(why))
        }
        Ok(out) if !out.usable() => {
            let why = format!(
                "agent `{}` produced no answer (exit {}); your message is saved",
                talk.agent,
                out.exit_code
                    .map_or_else(|| "unknown".to_owned(), |c| c.to_string())
            );
            (note(why.clone()), Some(why))
        }
        Ok(out) => (
            Turn {
                who: Who::Agent,
                body: out.text.trim().to_owned(),
                at: Timestamp::now(),
            },
            None,
        ),
    };

    // A close landed on disk while this turn was in flight is read back here
    // rather than trusted from the snapshot this call started with. `store`
    // holds nothing else this function does not itself own - the turn guard
    // in `web::Ui::begin_talk_turn` keeps `turns` and `seat` this call's
    // alone to mutate - but `status` is not behind that guard, and an
    // operator's close must stick: the whole point of ending a conversation
    // is that an agent's answer to the last message before the close cannot
    // silently reopen it. The guard is what makes that read-then-write
    // section atomic with `close`'s own - taken only for this tail and not
    // for the whole invocation above, so one talk's fifteen-minute turn does
    // not block another talk's close from proceeding.
    let _guard = store.guard();
    if let Ok(fresh) = store.get(&talk.id) {
        talk.status = fresh.status;
    }
    talk.turns.push(reply);
    store.put(talk)?;

    match failure {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
}

/// Everything said so far, as prose, for a CLI that cannot resume its own
/// conversation. See [`crate::chat::transcript`], which this mirrors.
fn transcript(talk: &Talk) -> String {
    let mut out = String::from(
        "This conversation cannot resume on the CLI's side, so here is \
         everything said so far; answer only the last message.\n",
    );
    for t in &talk.turns {
        let who = match t.who {
            Who::Operator => "operator",
            Who::Agent => "you",
        };
        out.push_str(&format!("\n## {who}\n\n{}\n", t.body.trim()));
    }
    out
}

/// The briefing the agent opens with, sent once as part of its first turn.
///
/// Pure, so the properties that matter can be asserted without an interview:
/// it names `magi task add --solo` (the only route this conversation has to
/// changing anything) and it does not carry
/// [`crate::plan::TASK_FILE_SPEC`] - that spec describes a task *file*, which
/// belongs to the planning interview and would tell this agent to write one
/// here instead of filing through the queue.
pub fn briefing(repo: &Path, language: &str) -> String {
    let mut out = format!(
        "You are magi's standing conversation partner for its operator, who \
         usually has this open on a phone. Keep replies short: no preamble, \
         no restating what they just said.\n\n\
         # Repository\n\n{repo}\n\n\
         You may look around: read files, run shell commands, search history, \
         run tests - whatever answers the question. Do not write files. \
         Implementing a change is not this conversation's job; a separate, \
         blind competition of agents does that, and a repository this \
         conversation has already edited would make their diffs unjudgeable.\n\n\
         # When the operator wants something done\n\n\
         Run:\n\n\
         magi task add --solo --repo {repo} <instruction>\n\n\
         and tell the operator the task id it prints, so they can follow it \
         from the Queue. Write <instruction> so that an implementer who has \
         never seen this conversation can act on it alone - it is everything \
         they get. Use --solo: it runs the task through one implementer \
         straight into review instead of the usual multi-agent competition, \
         which is the right shape for a change this conversation has already \
         settled, rather than one still worth several independent takes.\n",
        repo = repo.display(),
    );
    out.push_str(&language_note(language));
    out
}

/// The operator is talking, so their language matters here more than in most
/// prompts magi sends - see [`crate::chat::language_note`], which this
/// mirrors.
fn language_note(language: &str) -> String {
    if language.trim().is_empty() || language.eq_ignore_ascii_case("en") {
        String::new()
    } else {
        format!("\nHold this conversation in {language}.\n")
    }
}

/// Queue tasks this conversation has filed, oldest first.
///
/// A task is this conversation's when its [`Source::Agent`] names this
/// conversation's id as `run` - which is exactly what happens when
/// `magi task add` is run from inside a turn, because [`turn`] passes the
/// conversation's own id as [`Invocation::run`].
pub fn tasks_of(queue: &Queue, talk_id: &str) -> Vec<Task> {
    let mut tasks: Vec<Task> = queue
        .list()
        .into_iter()
        .filter(|t| matches!(&t.source, Source::Agent { run, .. } if run == talk_id))
        .collect();
    tasks.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    tasks
}

fn read_path(path: &Path) -> Result<Talk> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn short(id: &str) -> &str {
    id.split('-').next_back().unwrap_or(id)
}

fn new_id() -> String {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
    let seed = crate::rng::entropy();
    format!("{stamp}-{:04x}", (seed ^ (seed >> 32)) & 0xffff)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::{AgentKind, AgentSpec, Graph};
    use crate::queue::{Queue, Source, Task};

    use super::*;

    /// A store of its own, with no process-global state.
    fn store() -> (tempfile::TempDir, Talks) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let talks = Talks::at(tmp.path().join("talks"));
        (tmp, talks)
    }

    /// A `kind = "command"` agent whose whole behaviour is a POSIX shell
    /// script - see `chat`'s tests for why no test here may spawn a real
    /// agent CLI.
    fn mock_agent(dir: &Path, script: &str, env: BTreeMap<String, String>) -> AgentSpec {
        let path = dir.join("mock-talk-agent.sh");
        std::fs::write(&path, script).expect("write mock");
        AgentSpec {
            id: "mock".to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["sh".to_owned(), path.to_string_lossy().into_owned()],
            extra_args: Vec::new(),
            env,
            prompt_delivery: None,
        }
    }

    fn config(spec: AgentSpec) -> Config {
        Config {
            agents: vec![spec],
            graph: Graph {
                language: "en".to_owned(),
                ..Graph::default()
            },
            ..Config::default()
        }
    }

    /// Echo a canned reply, ignoring the prompt on stdin.
    const REPLY: &str = "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' \"$MOCK_REPLY\"\n";

    /// Say nothing and fail, the way a CLI that cannot start does.
    const BROKEN: &str = "#!/bin/sh\ncat >/dev/null\nexit 3\n";

    /// Reply with the prompt it was given, so a test can inspect exactly what
    /// the agent received on stdin.
    const ECHO: &str = "#!/bin/sh\ncat\n";

    fn env(reply: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("MOCK_REPLY".to_owned(), reply.to_owned())])
    }

    #[test]
    fn the_frozen_json_field_names_round_trip_through_disk() {
        let (tmp, talks) = store();
        let mut talk = Talk {
            schema: SCHEMA,
            id: "20260904-014455-ab12".to_owned(),
            repo: tmp.path().to_owned(),
            agent: "sonnet".to_owned(),
            status: TalkStatus::Open,
            turns: Vec::new(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            seat: SeatState::new(SEAT, "sonnet", 7),
        };
        talks.put(&mut talk).expect("put");

        let raw = std::fs::read_to_string(talks.path_of(&talk.id)).expect("read back");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        for field in [
            "schema",
            "id",
            "repo",
            "agent",
            "status",
            "turns",
            "created_at",
            "updated_at",
        ] {
            assert!(v.get(field).is_some(), "missing field `{field}`");
        }
        assert_eq!(v["schema"], 1);
        assert_eq!(v["status"], "open");

        let back = talks.get(&talk.id).expect("get");
        assert_eq!(back.id, talk.id);
        assert_eq!(back.status, TalkStatus::Open);
    }

    #[test]
    fn opening_a_talk_takes_no_agent_turn() {
        let (tmp, talks) = store();
        // A script that would fail loudly if it were ever run: `begin` must
        // not invoke anything, since there is nothing yet for an agent to
        // answer.
        let spec = mock_agent(tmp.path(), BROKEN, BTreeMap::new());
        let cfg = config(spec);

        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        assert_eq!(talk.status, TalkStatus::Open);
        assert!(talk.turns.is_empty(), "nothing has been said yet");

        let on_disk = talks.get(&talk.id).expect("get");
        assert_eq!(on_disk.turns.len(), 0);
    }

    #[tokio::test]
    async fn the_first_turn_carries_the_briefing_and_later_turns_do_not() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        say(&mut talk, &talks, &cfg, "what does the queue module do?")
            .await
            .expect("first turn");
        let first_prompt = &talk.turns[1].body;
        assert!(first_prompt.contains("magi task add --solo"));
        assert!(first_prompt.contains("what does the queue module do?"));

        say(&mut talk, &talks, &cfg, "and how is it locked?")
            .await
            .expect("second turn");
        let second_prompt = &talk.turns[3].body;
        assert!(
            !second_prompt.contains("magi task add --solo"),
            "the briefing is sent once, not on every turn: {second_prompt}"
        );
        assert!(second_prompt.contains("and how is it locked?"));
    }

    #[tokio::test]
    async fn say_appends_the_operator_turn_then_the_agent_turn() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("go ahead"));
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        say(&mut talk, &talks, &cfg, "can I rename this function?")
            .await
            .expect("say");

        assert_eq!(talk.turns.len(), 2);
        assert_eq!(talk.turns[0].who, Who::Operator);
        assert_eq!(talk.turns[0].body, "can I rename this function?");
        assert_eq!(talk.turns[1].who, Who::Agent);
        assert_eq!(talk.turns[1].body, "go ahead");
        assert_eq!(talks.get(&talk.id).expect("get").turns, talk.turns);
    }

    #[tokio::test]
    async fn a_failed_turn_keeps_the_operator_message_and_says_what_happened() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), BROKEN, BTreeMap::new());
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let err = say(&mut talk, &talks, &cfg, "check the tests")
            .await
            .expect_err("a turn with no answer is an error");
        assert!(err.to_string().contains("no answer"), "{err}");

        let on_disk = talks.get(&talk.id).expect("get");
        assert_eq!(on_disk.turns.len(), 2);
        assert_eq!(on_disk.turns[0].body, "check the tests");
        let note = &on_disk.turns[1];
        assert_eq!(note.who, Who::Agent);
        assert!(note.body.starts_with(MAGI_NOTE), "{}", note.body);
        assert!(note.body.contains("your message is saved"));
    }

    #[test]
    fn closing_is_idempotent_and_a_closed_talk_takes_no_more_turns() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        close(&mut talk, &talks).expect("close");
        assert_eq!(talk.status, TalkStatus::Closed);
        close(&mut talk, &talks).expect("closing twice is not an error");

        let err = record(&mut talk, &talks, "still there?").expect_err("closed talks refuse");
        assert!(err.to_string().contains("closed"));
        let _ = &cfg; // config kept only to build the agent above
    }

    #[tokio::test]
    async fn a_close_that_lands_while_a_turn_is_in_flight_is_not_undone_by_the_reply() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("here you go"));
        let cfg = config(spec);
        // The in-flight turn's own handle: loaded once, the way a spawned
        // background task in `web::talk_say` holds one for the whole turn.
        let mut in_flight = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        // The operator closes the conversation through a *different* handle
        // while the turn above is still running - exactly what a close typed
        // on the phone while an agent is mid-answer looks like.
        let mut closed_elsewhere = talks.get(&in_flight.id).expect("reread");
        close(&mut closed_elsewhere, &talks).expect("close");
        assert_eq!(
            talks.get(&in_flight.id).expect("reread").status,
            TalkStatus::Closed,
            "the close landed on disk before the turn finished"
        );

        // The turn's own handle still says `open` - it was loaded before the
        // close - and finishing it must not resurrect the conversation the
        // operator already ended.
        assert_eq!(in_flight.status, TalkStatus::Open);
        respond(&mut in_flight, &talks, &cfg, "one more question")
            .await
            .expect("the turn itself still completes");

        let on_disk = talks.get(&in_flight.id).expect("reread");
        assert_eq!(
            on_disk.status,
            TalkStatus::Closed,
            "a close must stick even when a turn that started before it finishes after it"
        );
        // The reply is not lost either: a turn already in flight when the
        // operator closed still gets its answer recorded.
        assert!(
            on_disk.turns.iter().any(|t| t.body == "here you go"),
            "the in-flight turn's own reply is still recorded: {:?}",
            on_disk.turns
        );
    }

    #[test]
    fn a_close_that_lands_before_record_is_called_is_not_undone_by_it() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        // The handle `web::talk_say` would have read before awaiting config
        // discovery, then carried across that await into `record`.
        let mut stale = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        // The operator closes the conversation through a *different* handle
        // in the gap between that read and the call to `record` below.
        let mut closed_elsewhere = talks.get(&stale.id).expect("reread");
        close(&mut closed_elsewhere, &talks).expect("close");
        assert_eq!(
            talks.get(&stale.id).expect("reread").status,
            TalkStatus::Closed,
            "the close landed on disk before record was called"
        );

        // The stale handle still says `open` - it was loaded before the
        // close - so a `record` that trusted it would append a turn and
        // write the conversation back open, undoing the close.
        assert_eq!(stale.status, TalkStatus::Open);
        let err = record(&mut stale, &talks, "still there?")
            .expect_err("a close that landed first must be honored, not overwritten");
        assert!(err.to_string().contains("closed"));

        let on_disk = talks.get(&stale.id).expect("reread");
        assert_eq!(
            on_disk.status,
            TalkStatus::Closed,
            "record must not resurrect a conversation closed while its snapshot was stale"
        );
        assert!(
            on_disk.turns.is_empty(),
            "the rejected turn must not have been appended: {:?}",
            on_disk.turns
        );
        let _ = &cfg; // config kept only to build the agent above
    }

    #[test]
    fn close_blocks_on_records_guard_rather_than_interleaving_with_it() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        // Hold the same guard `record`'s read-modify-write section holds for
        // the whole of its own read-then-write, standing in for `record`
        // being paused between its read and its `put`.
        let held = talks.guard();

        let talks2 = talks.clone();
        let id = talk.id.clone();
        let closing = std::thread::spawn(move || {
            let mut talk = talks2.get(&id).expect("get");
            close(&mut talk, &talks2).expect("close");
        });

        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !closing.is_finished(),
            "close must wait for the guard, not read and write while it is held - \
             a re-read alone narrows this window without closing it"
        );

        drop(held);
        closing.join().expect("close thread panicked");

        assert_eq!(
            talks.get(&talk.id).expect("reread").status,
            TalkStatus::Closed,
            "once the guard is free, close still lands"
        );
        let _ = &cfg; // config kept only to build the agent above
    }

    #[test]
    fn list_puts_open_talks_before_closed_ones() {
        let (tmp, talks) = store();
        let make = |id: &str, status: TalkStatus| {
            let mut t = Talk {
                schema: SCHEMA,
                id: id.to_owned(),
                repo: tmp.path().to_owned(),
                agent: "mock".to_owned(),
                status,
                turns: Vec::new(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                seat: SeatState::new(SEAT, "mock", 7),
            };
            talks.put(&mut t).expect("put");
        };
        make("20260901-000000-0001", TalkStatus::Open);
        make("20260902-000000-0002", TalkStatus::Open);
        make("20260903-000000-0003", TalkStatus::Closed);

        let ids: Vec<String> = talks.list().into_iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            [
                "20260902-000000-0002",
                "20260901-000000-0001",
                "20260903-000000-0003"
            ]
        );
        assert_eq!(talks.count_open(), 2);
    }

    #[test]
    fn tasks_of_finds_only_this_talks_own_tasks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Queue::at(dir.path().join("queue"));

        let mut mine = Task::new(
            "rework the loader".to_owned(),
            "rework the loader".to_owned(),
            PathBuf::from("/repo"),
            Source::Agent {
                run: "20260904-014455-ab12".to_owned(),
                node: "chat".to_owned(),
            },
        );
        queue.put(&mut mine).expect("put mine");

        let mut theirs = Task::new(
            "unrelated".to_owned(),
            "unrelated".to_owned(),
            PathBuf::from("/repo"),
            Source::Agent {
                run: "20260904-090000-zz99".to_owned(),
                node: "implement".to_owned(),
            },
        );
        queue.put(&mut theirs).expect("put theirs");

        let mut human = Task::new(
            "typed by hand".to_owned(),
            "typed by hand".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        queue.put(&mut human).expect("put human");

        let found = tasks_of(&queue, "20260904-014455-ab12");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, mine.id);
    }

    #[test]
    fn the_briefing_names_solo_task_add_and_not_the_task_file_spec() {
        let brief = briefing(Path::new("/repo"), "en");
        assert!(brief.contains("magi task add --solo"));
        assert!(!brief.contains(plan::TASK_FILE_SPEC));
        assert!(brief.contains("/repo"));
        assert!(!brief.contains("Hold this conversation in"));
    }

    #[test]
    fn the_briefing_names_the_language_when_it_is_not_english() {
        let brief = briefing(Path::new("/repo"), "Japanese");
        assert!(brief.contains("Hold this conversation in Japanese"));
    }
}
