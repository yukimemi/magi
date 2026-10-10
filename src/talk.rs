//! The standing conversation: a place to think out loud with an agent between
//! tasks, reachable from a phone.
//!
//! This is a conversation that stays open. Ask a question, have the agent
//! read a file or run a command to check something, talk through an idea,
//! and when it is time to act, tell it to file the work rather than do it
//! here. The conversation does not end; it is what the operator opens the
//! next time something comes up.
//!
//! # Talking is not implementing
//!
//! Every turn here runs with `allow_write: false` by default, for a reason
//! that is not security, but attribution. An agent that edits a checkout
//! mid-conversation leaves a diff that belongs to no run and passed no
//! review, and on a repository entered into magi's blind competition that
//! makes every candidate's diff unjudgeable. That is why the default holds
//! regardless of what a repository's own `magi.toml` says about anything
//! else. When the operator wants a change made, the agent is told to run
//! `magi task add --solo` ([`briefing`]) rather than reach for an editor: the
//! change goes through magi's own queue, on the repository's own terms, and
//! the operator can watch it happen instead of trusting that it did.
//!
//! `[talk] allow_write` ([`crate::config::Talk::allow_write`]) lets a
//! specific repository opt out of that default - a dotfiles or personal
//! config checkout that is never entered into a competition and never
//! reviewed has nothing for the restriction to protect, and filing a task for
//! a one-line edit there is pure overhead. Turning it on does not turn this
//! conversation into an implementer: [`briefing`] still sends everything
//! bigger than a small, operator-named edit to the queue, and still tells the
//! agent to say what it changed.
//!
//! With `[talk] allow_write` on, a codex turn also runs unsandboxed
//! ([`turn_access`]), as a claude turn already does under `bypassPermissions`:
//! otherwise `magi task add` (writes to magi's queue under the home directory)
//! or `git fetch` is refused by the sandbox. Unattended seats never get this.
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
//! The same split [`crate::queue`] uses: [`Talk`] is data plus pure helpers,
//! [`Talks`] owns the I/O and is constructed with its root, so every test
//! here drives a real store in a temp directory rather than the operator's
//! own home.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::config::{AgentSpec, Config};
use crate::queue::{Queue, Source, Task};

/// On-disk format for a conversation. Bumped when a field's meaning changes.
pub const SCHEMA: u32 = 1;

/// Wall-clock limit for one agent turn. See [`crate::config::Graph::timeout_talk`].
///
/// An hour by default. This turn is expected to run several shell commands
/// and read their output before answering one - "what does this function
/// do", "is this still true", "run the tests and tell me" - which argues for
/// an hour rather than the five minutes a short budget once assumed, because
/// the thing that made a short budget matter - an operator watching a
/// spinner - is not how this conversation gets used: the operator moves on
/// to something else while a turn runs and checks back later, so a long turn
/// spends a held seat, not anyone's attention.
fn turn_timeout(cfg: &Config) -> Duration {
    Duration::from_secs(cfg.graph.timeout_talk)
}

/// Seat name for the conversation's agent, scoping its CLI-side session away
/// from every other seat magi ever opens.
const SEAT: &str = "talk";

/// Prefix on a turn magi wrote rather than an agent.
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

/// One image the operator attached to a turn.
///
/// Never carries the bytes themselves: the picture lives on disk under
/// [`Talks::attachments_dir`], named by `id` alone. `name` is the filename
/// the operator's browser reported, kept only for display - it never
/// contributes to a path, which is what keeps an upload from being able to
/// traverse outside its own directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    /// Server-minted id; also the file's stem under `attachments_dir`.
    pub id: String,
    /// The operator's own filename, for display only.
    pub name: String,
    /// Validated by `web` at upload time against a closed whitelist:
    /// `image/png`, `image/jpeg`, `image/gif`, `image/webp`.
    pub mime: String,
    /// Size in bytes, so the phone can show it without a second request.
    pub bytes: u64,
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
    /// Images attached to this turn. `#[serde(default)]` so a conversation
    /// recorded before attachments existed still reads.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// What the agent's CLI reported reading for this reply; only ever set on
    /// an agent reply that carried usage. `#[serde(default)]` so a
    /// conversation recorded before this field existed still reads, which is
    /// why [`SCHEMA`] stays put: no existing field changed meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    /// Byte offsets in [`Self::body`] where the second and later messages of
    /// an operator turn begin. Replies queued while a turn runs are joined
    /// with a blank line, which an owner's own blank lines cannot be told
    /// apart from, so the boundary is recorded when it is made. `Some(empty)`
    /// is one message; `None` is unknown (a turn stored before this field, or
    /// not an operator turn) and readers fall back to paragraphs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breaks: Option<Vec<usize>>,
}

/// Raw usage of one agent reply, stored as the CLI reported it.
///
/// Counts only, never a percentage: the window is configuration
/// ([`Config::context_window`]) and the conversation's model can change, so a
/// stored percentage would go stale the moment either did. `agent` and
/// `model` record who read that many tokens, which is how [`context_usage`]
/// notices the figure predates a switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnUsage {
    /// Input-side tokens the CLI reported - see `agent::context_tokens`.
    pub context_tokens: u64,
    /// Roster id that answered.
    pub agent: String,
    /// That agent's model at the time (`None`: the CLI's own default).
    #[serde(default)]
    pub model: Option<String>,
}

/// How full the conversation's context window is, as the phone shows it.
///
/// Derived at read time and never persisted. `None` means unknown, which the
/// UI says in words - it is never a made-up 0.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContextUsage {
    /// Tokens the last agent reply's turn read, if its CLI reported any.
    pub tokens: Option<u64>,
    /// Window of the conversation's *current* model, if one is configured.
    pub window: Option<u64>,
    /// `tokens / window`, rounded down, not capped (over 100 is real news).
    pub percent: Option<u64>,
    /// 80% or more, judged before rounding: the conversation is getting long.
    pub warn: bool,
    /// The conversation's agent or model is not the one that produced
    /// `tokens`: the figure describes the old session and the next turn
    /// re-measures it.
    pub since_switch: bool,
    /// The current model, if the roster names one.
    pub model: Option<String>,
    /// `tokens` is a transcript-length estimate, not a CLI measurement (see
    /// [`estimate_context_tokens`]). A measured figure is never overridden.
    #[serde(default)]
    pub estimated: bool,
}

/// Fraction (in percent) of the window at which [`ContextUsage::warn`] fires.
const CONTEXT_WARN_PERCENT: u64 = 80;

/// Allowance for the standing prompt when no config is readable to render
/// the real [`briefing`].
const STANDING_PROMPT_FALLBACK_CHARS: u64 = 7000;

/// Rough estimate of the context a conversation occupies, for CLIs whose usage
/// is cumulative and so is never measured (claude / codex / agy).
///
/// Counts the characters (not bytes) of every operator and agent turn, skips
/// magi's own notes (never sent to the agent), adds `standing_chars` for the
/// standing prompt, and divides by ~3.5 chars per token, rounding up. `None`
/// when no turn counts: an empty conversation stays unknown.
///
/// Known biases: CJK runs near one token per character, and tool output,
/// images and the CLI's own system prompt are not counted, so this tends to
/// under-estimate (the 80% warning comes late); a compacted session is still
/// counted in full, which over-estimates.
pub fn estimate_context_tokens(talk: &Talk, standing_chars: u64) -> Option<u64> {
    let mut counted = false;
    let mut chars = standing_chars;
    for t in talk.turns.iter().filter(|t| !t.body.starts_with(MAGI_NOTE)) {
        counted = true;
        chars += t.body.chars().count() as u64;
    }
    // chars / 3.5, rounded up, in integers.
    counted.then(|| (chars * 2).div_ceil(7))
}

/// Context usage of `talk`, measured against its current model.
///
/// Only the latest agent reply counts (magi's own notes are skipped) and a
/// reply without usage makes the answer unknown - older replies are never
/// consulted, since a stale count passed off as current is worse than "unknown".
/// The window comes from the *current* agent's model, so switching model moves
/// the denominator at once.
///
/// Switching agent (or model, which is an agent change) mints a fresh CLI
/// session, so the next turn re-sends the whole transcript and the count
/// resets or jumps. Until that turn lands, the old figure is reported with
/// `since_switch` set. Deterministic: same talk and config, same answer.
pub fn context_usage(talk: &Talk, cfg: Option<&Config>) -> ContextUsage {
    let current = cfg.and_then(|c| c.agents.iter().find(|a| a.id == talk.agent));
    let model = current.and_then(|a| a.model.clone());
    let window = cfg
        .zip(model.as_deref())
        .and_then(|(c, m)| c.context_window(m))
        .filter(|w| *w > 0);
    let usage = talk
        .turns
        .iter()
        .rev()
        .find(|t| t.who == Who::Agent && !t.body.starts_with(MAGI_NOTE))
        .and_then(|t| t.usage.as_ref());
    let measured = usage.map(|u| u.context_tokens);
    let tokens = measured.or_else(|| {
        let standing = cfg.map_or(STANDING_PROMPT_FALLBACK_CHARS, |c| {
            briefing_for(
                &talk.repo,
                &c.graph.language,
                c.talk.allow_write,
                crate::persona::active(&c.talk.personas, &talk.persona).as_ref(),
                c.talk.operator_name(),
                talk.implementers,
            )
            .chars()
            .count() as u64
        });
        estimate_context_tokens(talk, standing)
    });
    let estimated = measured.is_none() && tokens.is_some();
    let since_switch =
        usage.is_some_and(|u| u.agent != talk.agent || (current.is_some() && u.model != model));
    let (percent, warn) = match (tokens, window) {
        (Some(t), Some(w)) => (
            Some(t.saturating_mul(100) / w),
            t.saturating_mul(100) >= w.saturating_mul(CONTEXT_WARN_PERCENT),
        ),
        _ => (None, false),
    };
    ContextUsage {
        tokens,
        window,
        percent,
        warn,
        since_switch,
        model,
        estimated,
    }
}

/// Where a conversation is in its life: this conversation can file any
/// number of tasks without ending, so it only ever moves once, from open to
/// closed.
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
    /// Text and attachments accepted while the single CLI turn is busy.
    /// They are durable, but become a real turn only when [`drain`] records them.
    #[serde(default)]
    pub pending: String,
    /// Attachments paired with [`Self::pending`].
    #[serde(default)]
    pub pending_attachments: Vec<Attachment>,
    /// Message boundaries inside [`Self::pending`], as [`Turn::breaks`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_breaks: Option<Vec<usize>>,
    /// May a failed turn fall back through the rest of `[roles] chatter`?
    /// True only while the agent was chosen by that chain; an explicit
    /// `--agent` or an operator's switch pins the conversation to its agent
    /// even when that agent also appears in the chain.
    #[serde(default)]
    pub fallback: bool,
    /// Persona id (see [`crate::persona`]); empty is the plain default voice.
    /// Independent of [`Self::agent`]: it survives an agent switch.
    #[serde(default)]
    pub persona: String,
    /// The persona changed after the CLI session was last told about it. The
    /// next resumed turn carries a persona update; cleared only once a turn
    /// has succeeded (a fresh seat's briefing already holds the new persona).
    #[serde(default)]
    pub persona_dirty: bool,
    /// How many implementers the tasks this chat files use (1..=3; 1 is
    /// `--solo`). Only the briefing's wording changes; nothing enforces it.
    #[serde(default = "solo_implementers")]
    pub implementers: u8,
    /// [`Self::implementers`] changed after the CLI session was last told;
    /// cleared only once a turn has succeeded, like [`Self::persona_dirty`].
    #[serde(default)]
    pub implementers_dirty: bool,
    /// When the conversation was opened.
    pub created_at: Timestamp,
    /// Last change to this file.
    pub updated_at: Timestamp,
    /// The CLI-side conversation, so a turn after the first costs one
    /// sentence instead of the whole transcript. Not `pub`: it is magi's
    /// bookkeeping, and a caller that edited it would detach the record from
    /// the conversation the model actually holds.
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
    ///
    /// The mutex only serializes this process. The cycle is also taken under a
    /// short file lock beside the records, so the CLI (`magi answer
    /// --ask-chat`) and the web server cannot overwrite each other's draft.
    /// The file lock is never skipped: a cycle that cannot get it within
    /// longer than the lock's own expiry fails instead of writing unguarded.
    fn guard(&self) -> Result<StoreGuard<'_>> {
        let mutex = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let path = self.root.join(".store.turn");
        let deadline = std::time::Instant::now() + TAKEOVER_LOCK_TTL * 2;
        loop {
            if let Some(file) = TurnLock::take(&path)? {
                return Ok(StoreGuard {
                    _file: file,
                    _mutex: mutex,
                });
            }
            if std::time::Instant::now() >= deadline {
                bail!("the talk store is locked by another process");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
    /// record rather than inside it.
    pub fn artifacts_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.artifacts"))
    }

    /// Where this conversation's attached images live: a subdirectory of
    /// `artifacts_of`, so deleting the conversation deletes its attachments
    /// too and nothing here needs its own cleanup path.
    pub fn attachments_dir(&self, id: &str) -> PathBuf {
        self.artifacts_of(id).join("attachments")
    }

    /// Persist one already-validated attachment and return its metadata.
    ///
    /// `web::talk_attachment_post` is the only caller: it has already
    /// checked `mime` against the whitelist and sniffed the bytes, so an
    /// unrecognised mime reaching here is a bug in that caller, not
    /// something an operator did. The id is minted here and never taken
    /// from the client; `name` is stored for display only and never used to
    /// build a path.
    pub fn put_attachment(
        &self,
        id: &str,
        mime: &str,
        name: &str,
        data: &[u8],
    ) -> Result<Attachment> {
        let dir = self.attachments_dir(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let ext = attachment_ext(mime).with_context(|| format!("unsupported mime `{mime}`"))?;
        let att = Attachment {
            id: new_attachment_id(),
            name: name.to_owned(),
            mime: mime.to_owned(),
            bytes: data.len() as u64,
        };
        std::fs::write(dir.join(format!("{}.{ext}", att.id)), data)
            .with_context(|| format!("write attachment {}", att.id))?;
        std::fs::write(
            dir.join(format!("{}.json", att.id)),
            serde_json::to_string(&att).context("serialize attachment")?,
        )
        .with_context(|| format!("write attachment metadata {}", att.id))?;
        Ok(att)
    }

    /// Just the metadata, without reading the image bytes back off disk -
    /// what `web::talk_say` uses to turn an id the operator referenced into
    /// an [`Attachment`] before appending a [`Turn`], where the bytes
    /// themselves are of no interest. `None` for an id this conversation
    /// never stored - including one that merely looks plausible:
    /// [`valid_attachment_id`] is checked here too, not only by the caller,
    /// the same defence-in-depth `Questions::panel_asset` uses for its own
    /// asset ids.
    pub fn attachment_meta(&self, id: &str, att_id: &str) -> Result<Option<Attachment>> {
        if !valid_attachment_id(att_id) {
            return Ok(None);
        }
        let meta_path = self.attachments_dir(id).join(format!("{att_id}.json"));
        if !meta_path.is_file() {
            return Ok(None);
        }
        let att = serde_json::from_str(
            &std::fs::read_to_string(&meta_path)
                .with_context(|| format!("read {}", meta_path.display()))?,
        )
        .with_context(|| format!("parse {}", meta_path.display()))?;
        Ok(Some(att))
    }

    /// A stored attachment's metadata and its bytes together, for serving it
    /// back on `GET`. `None` under the same conditions as
    /// [`Talks::attachment_meta`], which this is built on.
    pub fn read_attachment(&self, id: &str, att_id: &str) -> Result<Option<(Attachment, Vec<u8>)>> {
        let Some(att) = self.attachment_meta(id, att_id)? else {
            return Ok(None);
        };
        let ext = attachment_ext(&att.mime).with_context(|| {
            format!("attachment {att_id} has an unsupported mime `{}`", att.mime)
        })?;
        let data_path = self.attachments_dir(id).join(format!("{att_id}.{ext}"));
        let data =
            std::fs::read(&data_path).with_context(|| format!("read {}", data_path.display()))?;
        Ok(Some((att, data)))
    }

    /// Absolute path of one attachment's bytes, for the prompt note [`turn`]
    /// appends and for [`Invocation::attachments`]. `None` only for a mime
    /// [`put_attachment`] could never have written, which means the
    /// attachment did not come from this store.
    ///
    /// `self.root` (and so `attachments_dir`) is not guaranteed absolute on
    /// its own - `run::home()` returns a bare relative `PathBuf` verbatim
    /// when the operator sets `MAGI_HOME` to a relative path, and nothing
    /// canonicalizes it on the way in. That is harmless for every other use
    /// of this store, since its own I/O runs in this process against this
    /// process's cwd - but this path is handed to a CLI invoked with `cwd:
    /// &talk.repo`, a different directory, so a relative path here would
    /// resolve against the wrong place once it reached the prompt.
    /// `std::path::absolute` fixes it against *this* process's cwd before
    /// that happens; see `disk::free_bytes_by_os` for the same function used
    /// the same way elsewhere in this codebase.
    fn attachment_path(&self, id: &str, att: &Attachment) -> Option<PathBuf> {
        let ext = attachment_ext(&att.mime)?;
        let path = self.attachments_dir(id).join(format!("{}.{ext}", att.id));
        std::path::absolute(&path).ok()
    }

    /// Write a conversation, atomically, so a process killed mid-write leaves
    /// the previous state readable rather than a truncated file.
    ///
    /// The write-then-rename itself is retried a handful of times - see
    /// [`write_atomic`] - because a reader with the destination file briefly
    /// open is exactly the kind of failure that must not cost an agent's
    /// whole reply; see `turn`'s own tail for what happens when even that is
    /// not enough.
    pub fn put(&self, t: &mut Talk) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        t.updated_at = Timestamp::now();
        let body = serde_json::to_string_pretty(t).context("serialize talk")?;
        let path = self.path_of(&t.id);
        let tmp = path.with_extension("json.tmp");
        write_atomic(&tmp, &path, &body)
    }

    /// Load a conversation by id or unambiguous id prefix.
    pub fn get(&self, id: &str) -> Result<Talk> {
        let resolved = self.resolve_id(id)?;
        read_path(&self.path_of(&resolved))
    }

    /// Every conversation on disk: open first, then newest first, so what the
    /// operator is still using belongs above what they are done with.
    pub fn list(&self) -> Vec<Talk> {
        self.list_counting_unreadable().0
    }

    /// [`Talks::list`], plus how many `*.json` files could not be read, so a
    /// caller that reports to the operator can say what it skipped.
    pub fn list_counting_unreadable(&self) -> (Vec<Talk>, usize) {
        let mut unreadable = 0;
        let mut all: Vec<Talk> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| {
                let talk = read_path(&p).ok();
                if talk.is_none() {
                    unreadable += 1;
                }
                talk
            })
            .collect();
        all.sort_unstable_by(|a, b| {
            let rank = |t: &Talk| u8::from(!t.status.open());
            rank(a).cmp(&rank(b)).then_with(|| b.id.cmp(&a.id))
        });
        (all, unreadable)
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

    /// Change detection token: the newest modification time in the store, in
    /// milliseconds.
    pub fn revision(&self) -> u64 {
        std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_none_or(|x| x != "turn"))
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
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

    /// Where one conversation's turn lease lives. Not a `*.json`, so
    /// [`Talks::list`] never sees it.
    pub fn turn_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.turn"))
    }

    /// Claim the right to run one agent turn in `id`, across processes.
    ///
    /// `None` means somebody else holds a fresh lease - the caller reports
    /// "a turn is already running" and must not start one. A lease is held
    /// while its last beat is within [`crate::ask::LEASE_TTL`]; pids are never
    /// consulted. A stale (or unreadable) lease is taken over: renamed to a
    /// unique name first, so of several takers only the one whose rename wins
    /// deletes it, and the claim itself is an exclusive create.
    pub fn claim_turn(&self, id: &str) -> Result<Option<TurnLease>> {
        self.claim_turn_at(id, Timestamp::now())
    }

    fn claim_turn_at(&self, id: &str, now: Timestamp) -> Result<Option<TurnLease>> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let path = self.turn_path(id);
        let token = fresh_token();
        if create_turn(&path, &token, now)? {
            return Ok(Some(TurnLease {
                talk: id.to_owned(),
                path,
                token,
            }));
        }
        if lease_blocks(&path, now) {
            return Ok(None);
        }
        // Stale or unreadable. Every replacement happens under a short-lived
        // exclusive lock file, so two takers cannot each delete the other's
        // fresh lease. A taker that finds the lock held simply loses; the lock
        // itself ages out, so a taker that died inside it cannot wedge the talk.
        let Some(_lock) = TurnLock::take(&path)? else {
            return Ok(None);
        };
        if lease_blocks(&path, now) {
            return Ok(None);
        }
        let _ = std::fs::remove_file(&path);
        Ok(create_turn(&path, &token, now)?.then_some(TurnLease {
            talk: id.to_owned(),
            path,
            token,
        }))
    }

    /// Is a turn running in `id` anywhere, by a fresh lease?
    pub fn turn_held(&self, id: &str) -> bool {
        read_turn(&self.turn_path(id)).is_some_and(|r| r.fresh(Timestamp::now()))
    }

    /// Remove a conversation from disk, record and artifacts both. The
    /// operator's way of saying "not just done, gone" - [`close`] alone
    /// leaves the record as history.
    ///
    /// Takes [`Talks::guard`] for the same reason [`close`] does: a delete
    /// racing a [`record`] or the tail of [`turn`] must not land between
    /// their own read and write, or the file removed here would look, to
    /// them, like a record that simply has not been written yet. The other
    /// half of that story is on their side - both check under this same
    /// guard that the record they are about to write is still there, and
    /// give up without writing if it is not, which is what stops their `put`
    /// from resurrecting a conversation this call already removed.
    pub fn remove(&self, id: &str) -> Result<()> {
        let _guard = self.guard()?;
        let resolved = self.resolve_id(id)?;
        let path = self.path_of(&resolved);
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        let artifacts = self.artifacts_of(&resolved);
        if artifacts.is_dir() {
            std::fs::remove_dir_all(&artifacts)
                .with_context(|| format!("remove {}", artifacts.display()))?;
        }
        let _ = std::fs::remove_file(self.turn_path(&resolved));
        Ok(())
    }
}

/// What [`Talks::guard`] hands out: the in-process mutex plus the
/// cross-process file lock. The file lock is released first.
struct StoreGuard<'a> {
    _file: TurnLock,
    _mutex: MutexGuard<'a, ()>,
}

/// The body of a `<id>.turn` file. `pid` is for a human reading it; nothing
/// decides on it.
#[derive(Debug, Serialize, Deserialize)]
struct TurnRecord {
    token: String,
    pid: u32,
    beat_at: Timestamp,
}

impl TurnRecord {
    fn fresh(&self, now: Timestamp) -> bool {
        now.as_second() - self.beat_at.as_second() <= crate::ask::LEASE_TTL.as_secs() as i64
    }
}

/// A token no other caller in this process shares: `rng::entropy` is the
/// clock and the pid, so two threads in one clock tick would otherwise get the
/// same token, and with it the same temp file name and the same identity.
fn fresh_token() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let seed = crate::rng::entropy() ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    crate::rng::SplitMix64::new(seed).uuid_v4()
}

fn read_turn(path: &Path) -> Option<TurnRecord> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// A takeover lock older than this belonged to a taker that died inside it.
const TAKEOVER_LOCK_TTL: Duration = Duration::from_secs(10);

/// A break ticket older than this belonged to a taker that died holding it;
/// the next generation's ticket may then be tried for the same dead token.
const TICKET_TTL: Duration = Duration::from_secs(10);

/// Most ticket generations tried for one dead token.
const TICKET_GENERATIONS: u32 = 16;

/// Tickets are forgotten only this long after their last write.
const TICKET_SWEEP_AGE: Duration = Duration::from_secs(3600);

/// Create `path` exclusively with `body`; `false` when it already exists.
fn create_exclusive(path: &Path, body: &str) -> Result<bool> {
    use std::io::Write as _;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(body.as_bytes()) {
                drop(f);
                let _ = std::fs::remove_file(path);
                return Err(e).with_context(|| format!("write {}", path.display()));
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    }
}

fn create_turn(path: &Path, token: &str, now: Timestamp) -> Result<bool> {
    let record = TurnRecord {
        token: token.to_owned(),
        pid: std::process::id(),
        beat_at: now,
    };
    let body = serde_json::to_string(&record).context("serialize turn lease")?;
    // Written in full under a private name, then published exclusively: a
    // reader never sees a half-written lease.
    let tmp = path.with_extension(format!("turn.{token}.new"));
    publish_exclusive(path, &tmp, &body)
}

/// Is there a lease at `path` that must not be taken over? A fresh beat, or
/// an empty file young enough to be a [`publish_exclusive`] placeholder whose
/// writer has not yet renamed the real record into place.
fn lease_blocks(path: &Path, now: Timestamp) -> bool {
    if read_turn(path).is_some_and(|r| r.fresh(now)) {
        return true;
    }
    std::fs::metadata(path).is_ok_and(|m| {
        m.len() == 0
            && m.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age <= TAKEOVER_LOCK_TTL)
    })
}

/// Does this error mean the filesystem has no hard links (as opposed to an
/// ordinary I/O failure, which must not be hidden by a fallback)?
fn link_unsupported(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::Unsupported || (cfg!(windows) && e.raw_os_error() == Some(1))
}

/// Publish the finished file `tmp` (holding `body`) at `path` unless `path`
/// exists; `false` when it does. `tmp` is always removed.
///
/// The first choice is `hard_link`, which is exclusive and atomic. Where hard
/// links are unsupported the fallback is an exclusive `create_new` of `path`
/// itself, written through that handle (never a `rename` over `path`, which
/// would let a stalled writer clobber a successor). Readers can briefly see
/// the file empty: [`lease_blocks`] treats a young empty one as held, and a
/// lock side treats it as an "invalid" token that cannot be broken for
/// [`TAKEOVER_LOCK_TTL`]. Residual windows, not closable by path alone: a
/// reader may see a partly written file for the length of one small write, and
/// a writer stalled between create and write for longer than the TTL can have
/// its file replaced; it then finds the path no longer carries its body and
/// reports `false`.
/// The no-hard-link path of [`publish_exclusive`]. A failed write never
/// removes `path`: by then it may belong to a successor, so the half-made file
/// is left to expire (a young empty one blocks, an unreadable one is stale).
fn create_in_place(path: &Path, body: &str) -> Result<bool> {
    use std::io::Write as _;
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
    };
    f.write_all(body.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    // Written through our own handle, so a stalled writer can only write into
    // its own file; confirm the path still carries it before claiming it.
    Ok(std::fs::read_to_string(path).is_ok_and(|t| t == body))
}

fn publish_exclusive(path: &Path, tmp: &Path, body: &str) -> Result<bool> {
    std::fs::write(tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(test)]
    let linked = if failpoint::no_link_forced() {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    } else {
        std::fs::hard_link(tmp, path)
    };
    #[cfg(not(test))]
    let linked = std::fs::hard_link(tmp, path);
    let out = match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) if link_unsupported(&e) => create_in_place(path, body),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    };
    let _ = std::fs::remove_file(tmp);
    out
}

/// Remove `path` only if it carries exactly `expected`; `true` when it did.
///
/// There is no atomic compare-and-unlink on a path, so the file is first
/// renamed to a private name, which makes the read and the removal act on the
/// same inode. If it carried something else (a newer holder published between
/// the caller's read and now) it is put back exclusively, so the wrong lock is
/// off the path for microseconds instead of deleted. Residual window: in those
/// microseconds a fourth party can publish at `path`; the restore then loses,
/// the displaced file is dropped (with a warning) and two holders can overlap
/// until the survivor's TTL. Path operations alone cannot close that.
fn remove_if_carries(path: &Path, expected: &str) -> bool {
    let gone = path.with_extension(format!("gone.{}", fresh_token()));
    if std::fs::rename(path, &gone).is_err() {
        return false;
    }
    let found = std::fs::read_to_string(&gone);
    if found.as_ref().is_ok_and(|t| t == expected) {
        let _ = std::fs::remove_file(&gone);
        return true;
    }
    if let Ok(body) = found {
        let back = path.with_extension(format!("back.{}", fresh_token()));
        match publish_exclusive(path, &back, &body) {
            Ok(true) => {}
            Ok(false) | Err(_) => tracing::warn!(
                "{} was replaced while a stale removal had it aside; \
                 the displaced file is dropped",
                path.display()
            ),
        }
    }
    let _ = std::fs::remove_file(&gone);
    false
}

/// The short exclusive lock every change to an existing lease (takeover, beat,
/// release) happens under, so none of them can act on a stale reading. It
/// ages out after [`TAKEOVER_LOCK_TTL`] in case its holder died inside it.
struct TurnLock {
    path: PathBuf,
    token: String,
}

impl TurnLock {
    fn token() -> String {
        fresh_token()
    }

    /// Publish `token` at `path` complete (written privately, then published
    /// exclusively), so nobody reads a half-written token; `false` if `path`
    /// exists.
    fn publish(path: &Path, token: &str) -> Result<bool> {
        let tmp = path.with_extension(format!("lock.{token}.new"));
        publish_exclusive(path, &tmp, token)
    }

    fn take(lease: &Path) -> Result<Option<Self>> {
        let path = lease.with_extension("turn.lock");
        let token = Self::token();
        if Self::publish(&path, &token)? {
            return Ok(Some(Self { path, token }));
        }
        let Ok(seen) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        // A lock that is not a whole token (an older build wrote it in two
        // steps) is breakable too, under one fixed ticket name.
        let key = if !seen.is_empty() && seen.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            seen.as_str()
        } else {
            "invalid"
        };
        let aged = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > TAKEOVER_LOCK_TTL);
        if !aged {
            return Ok(None);
        }
        // Breaking a dead lock is decided by a ticket named after the token
        // that was judged dead: exactly one taker per generation can create
        // it. The lock itself is never moved aside, so a fresh lock made by
        // the winner is never off the path, not even for an instant.
        //
        // The ticket name carries a generation, not the wall clock, so no
        // time boundary can let a second taker in while a ticket is alive.
        // Generation n+1 is tried only when ticket n is older than
        // `TICKET_TTL` (its owner died). Residual risk, same kind as the lock's
        // own TTL: an owner stalled past `TICKET_TTL` between creating its
        // ticket and publishing can overlap with the next generation's taker.
        let mut won = false;
        for n in 0..TICKET_GENERATIONS {
            let ticket = path.with_extension(format!("lock.{key}.break.{n}"));
            if create_exclusive(&ticket, "")? {
                won = true;
                break;
            }
            let stale = std::fs::metadata(&ticket)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > TICKET_TTL);
            if !stale {
                return Ok(None);
            }
        }
        if !won {
            // Every generation was abandoned. A ticket is never unlinked by
            // name while it may still be someone's exclusion: only the sweep
            // removes tickets, and only ones far older than any taker lives,
            // so recovery resumes once they age out.
            Self::sweep_tickets(&path);
            return Ok(None);
        }
        Self::sweep_tickets(&path);
        // Only the ticket's owner reaches this point for `seen`, but the
        // lock's own holder may still release it (its `Drop`) and a third
        // taker publish a new one meanwhile, so the removal is verified
        // against the content (see [`remove_if_carries`] for the window that
        // remains); if the lock changed, leave it.
        if !remove_if_carries(&path, &seen) {
            return Ok(None);
        }
        if Self::publish(&path, &token)? {
            return Ok(Some(Self { path, token }));
        }
        Ok(None)
    }

    /// Forget tickets far older than the lock's TTL. They stay that long so a
    /// slow taker that read the same dead token cannot break it a second time.
    fn sweep_tickets(path: &Path) {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return;
        };
        let prefix = format!("{name}.");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let file = entry.file_name();
            let Some(file) = file.to_str() else { continue };
            if !(file.starts_with(&prefix) && file.contains(".break.")) {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > TICKET_SWEEP_AGE);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Wait briefly for the lock; the holders only do a read and a write.
    fn take_patiently(lease: &Path) -> Option<Self> {
        for _ in 0..50 {
            match Self::take(lease) {
                Ok(Some(lock)) => return Some(lock),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for TurnLock {
    fn drop(&mut self) {
        // Only our own lock: one that aged out and was taken over is not ours.
        remove_if_carries(&self.path, &self.token);
    }
}

/// How often a running turn renews its lease: well inside
/// [`crate::ask::LEASE_TTL`].
pub const TURN_BEAT: Duration = Duration::from_secs(20);

/// One talk's cross-process turn slot, released on drop (success, error,
/// panic or a dropped handler future alike). Release only removes the file
/// while it still carries this lease's token, so a guard that outlived its
/// own expiry cannot delete the lease of whoever took over.
#[derive(Debug)]
pub struct TurnLease {
    talk: String,
    path: PathBuf,
    token: String,
}

impl TurnLease {
    /// Does the lease file still carry this lease's token? A read only.
    pub fn holds(&self) -> bool {
        read_turn(&self.path).is_some_and(|r| r.token == self.token)
    }

    /// Renew the lease. `Ok(false)` means it was taken over or removed, so
    /// this turn no longer owns the slot; `Err` is a transient failure (the
    /// lock stayed busy, a write failed) and the next beat tries again.
    pub fn beat(&self) -> Result<bool> {
        let _lock = TurnLock::take_patiently(&self.path)
            .with_context(|| format!("lock {} to renew it", self.path.display()))?;
        let Some(mut record) = read_turn(&self.path).filter(|r| r.token == self.token) else {
            return Ok(false);
        };
        record.beat_at = Timestamp::now();
        let body = serde_json::to_string(&record).context("serialize turn lease")?;
        let tmp = self.path.with_extension(format!("turn.{}.tmp", self.token));
        write_atomic(&tmp, &self.path, &body)?;
        Ok(true)
    }

    /// Run `fut` while renewing this lease every [`TURN_BEAT`]. A transient
    /// beat failure is logged and the turn goes on; a lost lease drops `fut`
    /// (the turn stops) and is an error, since somebody else may now be
    /// running the same conversation.
    pub async fn beating<T>(&self, fut: impl std::future::Future<Output = T>) -> Result<T> {
        self.beating_every(TURN_BEAT, fut).await
    }

    async fn beating_every<T>(
        &self,
        period: Duration,
        fut: impl std::future::Future<Output = T>,
    ) -> Result<T> {
        tokio::pin!(fut);
        loop {
            match tokio::time::timeout(period, &mut fut).await {
                Ok(out) => return Ok(out),
                Err(_) => match self.beat() {
                    Ok(true) => {}
                    Ok(false) => bail!(
                        "the turn lease {} was taken over; this turn is stopped",
                        self.path.display()
                    ),
                    Err(e) => tracing::warn!("{e:#}"),
                },
            }
        }
    }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        // Under the lock, so a takeover cannot slip in between the check and
        // the removal. If the lock stays busy the lease just ages out.
        if let Some(_lock) = TurnLock::take_patiently(&self.path) {
            if read_turn(&self.path).is_some_and(|r| r.token == self.token) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Open a conversation. Takes no agent turn: there is no idea to answer yet,
/// and a conversation the operator has not said anything into yet is a
/// normal, valid thing to have sitting on the phone.
///
/// `agent` beats `[roles] chatter`, which beats [`agent::pick`]'s own default
/// order (a claude seat, else the first runnable agent in roster order) when
/// nothing names a seat at all - see `[roles] chatter`'s own doc in
/// [`crate::config`] for why a dedicated field exists rather than reusing a
/// judge seat.
pub fn begin(store: &Talks, cfg: &Config, repo: PathBuf, agent: Option<&str>) -> Result<Talk> {
    begin_with(store, cfg, repo, agent, &Preferred::default())
}

/// Remembered choices for a new conversation (`POST /api/talks`). Every field
/// is soft: one that does not fit `cfg` is dropped on its own and the default
/// stands, so a stale remembered value never stops a conversation opening.
#[derive(Debug, Default, Clone)]
pub struct Preferred {
    /// Remembered agent id.
    pub agent: Option<String>,
    /// Remembered persona id.
    pub persona: Option<String>,
    /// Remembered implementer count.
    pub implementers: Option<i64>,
}

/// [`begin`] with remembered choices. An explicit `agent` wins over
/// `preferred.agent` and keeps its strict error. A preferred agent that is on
/// the roster and runnable is an explicit choice (`fallback = false`) unless
/// it is the chatter chain's own head. Persona and implementers are applied
/// before the single save, so neither is marked dirty: the new seat's briefing
/// already carries them.
pub fn begin_with(
    store: &Talks,
    cfg: &Config,
    repo: PathBuf,
    agent: Option<&str>,
    preferred: &Preferred,
) -> Result<Talk> {
    // Absolute: a relative path means the wrong repository once anything
    // other than this process reads it back.
    let repo = repo.canonicalize().unwrap_or(repo);
    // An explicit agent is a chain of one; otherwise the first id of
    // `[roles] chatter` that can run here (later ones are `turn`'s fallbacks).
    let head = || -> Result<AgentSpec> {
        Ok(agent::pick_chain(
            &cfg.agents,
            cfg.roles.chatter.as_ref(),
            &agent::installed,
            "chatter",
        )?
        .remove(0))
    };
    let mut fallback = agent.is_none();
    let spec = match agent {
        Some(id) => agent::pick(&cfg.agents, Some(id), &agent::installed)?,
        None => {
            let head = head()?;
            match preferred
                .agent
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                Some(id) => match agent::pick(&cfg.agents, Some(id), &agent::installed) {
                    Ok(spec) => {
                        fallback = spec.id == head.id;
                        spec
                    }
                    Err(_) => head,
                },
                None => head,
            }
        }
    };
    let persona = preferred
        .persona
        .as_deref()
        .and_then(|id| crate::persona::find(&cfg.talk.personas, id))
        .filter(|p| !p.is_default())
        .map(|p| p.id)
        .unwrap_or_default();
    let implementers = preferred
        .implementers
        .and_then(|n| u8::try_from(n).ok())
        .and_then(|n| check_implementers(n, cfg).ok())
        .unwrap_or(1);

    let now = Timestamp::now();
    let mut talk = Talk {
        pending_breaks: None,
        schema: SCHEMA,
        id: new_id(),
        repo,
        agent: spec.id.clone(),
        status: TalkStatus::Open,
        turns: Vec::new(),
        pending: String::new(),
        pending_attachments: Vec::new(),
        fallback,
        persona,
        persona_dirty: false,
        implementers,
        implementers_dirty: false,
        created_at: now,
        updated_at: now,
        seat: SeatState::new(SEAT, &spec.id, crate::rng::entropy()),
    };
    store.put(&mut talk)?;
    Ok(talk)
}

/// Append the operator's turn and flush it, without invoking anything.
///
/// Split out of [`say`] so `POST /api/talks/{id}/say` can answer once the
/// message is safely on disk, and run the agent's half in the background -
/// holding the connection for a turn that can run fifteen minutes is the
/// wrong shape for a phone.
pub fn record(
    talk: &mut Talk,
    store: &Talks,
    text: &str,
    attachments: Vec<Attachment>,
) -> Result<String> {
    // `web::talk_say` reads the talk, then awaits config discovery before
    // calling this - a gap a concurrent `POST /api/talks/{id}/close` can land
    // in. The guard held for the rest of this function is what actually closes
    // that gap: re-reading status without it only shrinks the window a
    // concurrent `close` could land in between this call's own read and its
    // `put`, it does not remove it. See [`Talks::guard`] and the matching
    // guard in `turn`, which this mirrors.
    let _guard = store.guard()?;
    // A concurrent `Talks::remove` can have landed in that same gap. `put`
    // writes unconditionally, so trusting the stale `talk` here would recreate
    // the file a delete just removed - the record must still be there for a
    // turn to have anywhere to append to.
    let Ok(fresh) = store.get(&talk.id) else {
        bail!("talk {} was deleted", talk.short());
    };
    talk.status = fresh.status;
    // Do not let this older handle overwrite a draft accepted while it was
    // waiting for configuration discovery.
    talk.pending = fresh.pending;
    talk.pending_breaks = fresh.pending_breaks;
    talk.pending_attachments = fresh.pending_attachments;
    if !talk.status.open() {
        bail!(
            "talk {} is {} and takes no more turns",
            talk.short(),
            talk.status.as_str()
        );
    }
    let text = text.trim();
    if text.is_empty() && attachments.is_empty() {
        bail!("nothing to say");
    }
    talk.turns.push(Turn {
        who: Who::Operator,
        body: text.to_owned(),
        at: Timestamp::now(),
        attachments,
        usage: None,
        breaks: Some(Vec::new()),
    });
    store.put(talk)?;
    Ok(text.to_owned())
}

/// Add an unrecorded message to the durable draft while another turn runs.
pub fn queue(
    talk: &mut Talk,
    store: &Talks,
    text: &str,
    attachments: Vec<Attachment>,
) -> Result<()> {
    let text = text.trim();
    if text.is_empty() && attachments.is_empty() {
        bail!("nothing to say");
    }
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if !fresh.status.open() {
        bail!(
            "talk {} is {} and takes no more turns",
            fresh.short(),
            fresh.status.as_str()
        );
    }
    if !text.is_empty() {
        if fresh.pending.is_empty() {
            fresh.pending = text.to_owned();
            fresh.pending_breaks = Some(Vec::new());
        } else {
            fresh.pending.push_str("\n\n");
            if let Some(b) = fresh.pending_breaks.as_mut() {
                b.push(fresh.pending.len());
            }
            fresh.pending.push_str(text);
        }
    }
    fresh.pending_attachments.extend(attachments);
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(())
}

/// Promote the current durable draft to one operator turn.
pub fn drain(talk: &mut Talk, store: &Talks) -> Result<Option<String>> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if !fresh.status.open() || (fresh.pending.is_empty() && fresh.pending_attachments.is_empty()) {
        *talk = fresh;
        return Ok(None);
    }
    let text = std::mem::take(&mut fresh.pending);
    let attachments = std::mem::take(&mut fresh.pending_attachments);
    let breaks = fresh.pending_breaks.take();
    fresh.turns.push(Turn {
        who: Who::Operator,
        body: text.clone(),
        at: Timestamp::now(),
        attachments,
        usage: None,
        breaks,
    });
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(Some(text))
}

/// One operator turn and one agent turn, appended - the synchronous form, used
/// by tests and by anything that is fine waiting out the turn itself.
pub async fn say(
    lease: &TurnLease,
    talk: &mut Talk,
    store: &Talks,
    cfg: &Config,
    text: &str,
    attachments: Vec<Attachment>,
) -> Result<()> {
    check_lease(lease, talk)?;
    let text = record(talk, store, text, attachments)?;
    respond(lease, talk, store, cfg, &text).await
}

/// The agent's half of a turn: invoke, append, flush. Pairs with [`record`].
///
/// Requires the talk's [`TurnLease`] (from [`Talks::claim_turn`]), so no
/// starter can run a turn without holding the cross-process slot. The lease is
/// renewed while the turn runs; a lease lost mid-turn stops it with an error.
pub async fn respond(
    lease: &TurnLease,
    talk: &mut Talk,
    store: &Talks,
    cfg: &Config,
    text: &str,
) -> Result<()> {
    check_lease(lease, talk)?;
    lease
        .beating(turn(talk, store, cfg, text))
        .await
        .and_then(|done| done)
}

fn check_lease(lease: &TurnLease, talk: &Talk) -> Result<()> {
    if lease.talk != talk.id {
        bail!("the turn lease is for talk {}, not {}", lease.talk, talk.id);
    }
    // A lease that aged out and was taken over must not start a turn (or
    // write the operator's text) at all.
    if !lease.holds() {
        bail!("the turn lease for talk {} is no longer held", talk.short());
    }
    Ok(())
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
///
/// If the re-read fails, this errors rather than falling back to the
/// caller's stale copy: `talk::begin` always `put`s the record before handing
/// out a `Talk`, so the only way a re-read can fail is a concurrent
/// [`Talks::remove`] having deleted it, and writing the stale copy back would
/// resurrect exactly what that delete removed.
pub fn close(talk: &mut Talk, store: &Talks) -> Result<()> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    fresh.status = TalkStatus::Closed;
    // A closed conversation must not replay a draft if it is reopened later.
    fresh.pending.clear();
    fresh.pending_breaks = None;
    fresh.pending_attachments.clear();
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(())
}

/// Reopen a closed conversation. Idempotent for the same reason [`close`] is:
/// reopening an already-open conversation is not an error, since the
/// operator's intent - "I want to keep talking about this" - is already
/// satisfied.
///
/// Written symmetrically with [`close`]: re-reads the record under
/// [`Talks::guard`] rather than trusting the caller's copy of `talk`, writes
/// that fresh copy back rather than the one passed in, and errors rather than
/// falling back to the stale copy if the re-read fails, for the same reasons
/// `close`'s doc gives.
pub fn reopen(talk: &mut Talk, store: &Talks) -> Result<()> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    fresh.status = TalkStatus::Open;
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(())
}

/// Hand the conversation to another roster agent.
///
/// A CLI session belongs to one CLI and cannot be carried to another, so the
/// seat is minted afresh rather than edited: the next turn finds
/// `seat.turns == 0` and re-sends the transcript, since the new agent has
/// heard none of it. A magi-written note records the change in the
/// transcript. Returns `false` (and writes nothing, not even a note) when the
/// stored talk already uses `spec`.
///
/// Re-reads under [`Talks::guard`], like [`close`], and errors rather than
/// resurrecting a record a concurrent delete removed. Refusing a closed talk
/// or a turn in flight is the caller's job: only it can see the latter.
pub fn switch_agent(talk: &mut Talk, store: &Talks, spec: &AgentSpec) -> Result<bool> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if fresh.agent == spec.id {
        *talk = fresh;
        return Ok(false);
    }
    let from = std::mem::replace(&mut fresh.agent, spec.id.clone());
    fresh.seat = SeatState::new(SEAT, &spec.id, crate::rng::entropy());
    // A deliberate switch pins the conversation to the agent chosen.
    fresh.fallback = false;
    fresh.turns.push(Turn {
        breaks: None,
        who: Who::Agent,
        body: format!("{MAGI_NOTE}agent changed from {from} to {}", spec.id),
        at: Timestamp::now(),
        attachments: Vec::new(),
        usage: None,
    });
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(true)
}

/// Pick the conversation's persona (`""` or `default` is the plain voice).
///
/// The CLI session keeps its seat: the change is recorded with
/// [`Talk::persona_dirty`] and a magi note, and the next turn tells the session
/// about it (or its fresh briefing already does). Returns `false` and writes
/// nothing when the stored persona is already `id`. Refusing a closed talk, an
/// unknown id or a turn in flight is the caller's job.
pub fn switch_persona(talk: &mut Talk, store: &Talks, id: &str) -> Result<bool> {
    let id = if id.trim() == crate::persona::DEFAULT_ID {
        ""
    } else {
        id.trim()
    };
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if fresh.persona == id {
        *talk = fresh;
        return Ok(false);
    }
    let from = std::mem::replace(&mut fresh.persona, id.to_owned());
    fresh.persona_dirty = true;
    let label = |p: &str| {
        if p.is_empty() {
            crate::persona::DEFAULT_ID.to_owned()
        } else {
            p.to_owned()
        }
    };
    fresh.turns.push(Turn {
        breaks: None,
        who: Who::Agent,
        body: format!(
            "{MAGI_NOTE}persona changed from {} to {}",
            label(&from),
            label(id)
        ),
        at: Timestamp::now(),
        attachments: Vec::new(),
        usage: None,
    });
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(true)
}

/// Discard the durable draft without adding a transcript turn.
pub fn clear_pending(talk: &mut Talk, store: &Talks) -> Result<()> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    fresh.pending.clear();
    fresh.pending_breaks = None;
    fresh.pending_attachments.clear();
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(())
}

/// Clear a draft only when the caller still sees its complete snapshot.
pub fn clear_pending_if_matches(
    talk: &mut Talk,
    store: &Talks,
    expected_text: &str,
    expected_attachments: &[String],
) -> Result<bool> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if !pending_matches(&fresh, expected_text, expected_attachments) {
        *talk = fresh;
        return Ok(false);
    }
    fresh.pending.clear();
    fresh.pending_breaks = None;
    fresh.pending_attachments.clear();
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(true)
}

/// Replace just the text of the durable draft, but only if the caller's
/// snapshot still identifies the entire draft. This refuses to overwrite a
/// message another client queued or a draft the drain already promoted.
pub fn edit_pending_text(
    talk: &mut Talk,
    store: &Talks,
    text: &str,
    expected_text: &str,
    expected_attachments: &[String],
) -> Result<bool> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if !pending_matches(&fresh, expected_text, expected_attachments) {
        *talk = fresh;
        return Ok(false);
    }
    fresh.pending = text.trim().to_owned();
    fresh.pending_breaks = Some(Vec::new());
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(true)
}

fn pending_matches(talk: &Talk, expected_text: &str, expected_attachments: &[String]) -> bool {
    talk.pending == expected_text
        && talk
            .pending_attachments
            .iter()
            .map(|attachment| &attachment.id)
            .eq(expected_attachments.iter())
}

/// Invoke the conversation's agent once and append what it said.
///
/// The first turn ever taken carries the full [`briefing`], because nothing
/// else has told the agent what this conversation is or what it may do.
/// Every turn after that resends nothing when the CLI can resume its own
/// session, and falls back to [`transcript`] only when it cannot.
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

    // The newest turn is always the operator message this call is answering
    // - `record` appended it before `turn` was ever called - so its own
    // attachments are what belong at the end of *this* prompt.
    let last_note = attachment_note(
        store,
        &talk.id,
        talk.turns
            .last()
            .map_or(&[][..], |t| t.attachments.as_slice()),
    );

    // Every attachment this conversation has ever held, not only this
    // turn's: a resumed session gets a fresh process every turn, so a CLI
    // whose sandbox needs `--add-dir` (see `agent::build_command`) needs the
    // grant again to open an image from an earlier turn, even when nothing
    // new was attached just now.
    let attachment_paths: Vec<PathBuf> = talk
        .turns
        .iter()
        .flat_map(|t| t.attachments.iter())
        .filter_map(|a| store.attachment_path(&talk.id, a))
        .collect();

    // A question handed to this conversation is answered with `magi answer`,
    // which writes the question store; a read-only sandbox refuses that. So
    // while such a question is still open - the hand-over turn and the turns
    // in which the owner decides - the turn may write, with the question store
    // as a writable root. It goes back to read-only once the question is
    // answered or abandoned. As for the deputy, what the seat may touch beyond
    // that rests on the prompt, not the sandbox.
    // `try_home`, not `Questions::open`: a unit test that never pinned a home
    // has no store to consult, and must not abort the turn on `run::home()`.
    let questions =
        crate::run::try_home().map(|home| crate::ask::Questions::at(home.join("questions")));
    let consulted = questions
        .as_ref()
        .is_some_and(|q| crate::consult::pending_consults(q, &talk.id));
    let consult_roots: Vec<PathBuf> = match &questions {
        Some(q) if consulted => vec![q.root().to_path_buf()],
        _ => Vec::new(),
    };

    let (allow_write, unsandboxed) = turn_access(cfg.talk.allow_write, consulted);

    let artifacts = store.artifacts_of(&talk.id);
    // From the transcript, not the seat: a switched agent's seat restarts at
    // zero and must not overwrite an earlier turn's artifacts.
    let operator_turns = talk.turns.iter().filter(|t| t.who == Who::Operator).count();
    let stem = format!("turn-{}", operator_turns.max(1));
    // The chat's build cache is the same shared one the graph's seats get, so
    // a conversation that compiles does not mint another multi-GB target dir.
    let cache_dir = cfg.cache_dir();

    // The agent holding the conversation, then - only when it came from
    // `[roles] chatter` - the rest of that chain, each at most once.
    let mut chain = vec![spec.clone()];
    if let Some(choice) = cfg.roles.chatter.as_ref()
        && talk.fallback
    {
        for id in choice.ids() {
            if id == talk.agent || chain.iter().any(|s| s.id == id) {
                continue;
            }
            match agent::pick(&cfg.agents, Some(id), &agent::installed) {
                Ok(s) => chain.push(s),
                Err(e) => tracing::warn!("[roles] chatter: skipping `{id}`: {e:#}"),
            }
        }
    }

    let persona = crate::persona::active(&cfg.talk.personas, &talk.persona);
    let operator_name = cfg.talk.operator_name();
    let persona_update = if talk.persona_dirty {
        format!(
            "{}\n\n",
            crate::persona::update_block_for(persona.as_ref(), operator_name)
        )
    } else {
        String::new()
    };

    // The filing command is not in the transcript either, so a non-Solo count
    // rides on every turn that has no session to remember it.
    let filing_update = if talk.implementers_dirty {
        format!("{}\n\n", filing_update_block(talk.implementers))
    } else {
        String::new()
    };

    let mut outcome = None;
    let mut fell_back_from: Option<String> = None;
    // What the conversation looked like after the first agent's failed try,
    // so an exhausted chain leaves exactly what a single failed seat would.
    let mut first_try: Option<(String, SeatState)> = None;
    for (n, spec) in chain.iter().enumerate() {
        if n > 0 {
            if first_try.is_none() {
                first_try = Some((talk.agent.clone(), talk.seat.clone()));
            }
            tracing::warn!("chat: falling back from `{}` to `{}`", talk.agent, spec.id);
            // A new CLI has none of the old one's conversation: a fresh seat
            // puts `has_session` at false and the full transcript is re-sent.
            fell_back_from.get_or_insert_with(|| talk.agent.clone());
            talk.agent = spec.id.clone();
            talk.seat = SeatState::new(SEAT, &spec.id, crate::rng::entropy());
        }
        let resuming = agent::has_session(spec.kind, &talk.seat, cfg.graph.sessions);
        let first_ever = talk.turns.len() <= 1;
        let body = if talk.seat.turns == 0 && first_ever {
            format!(
                "{}\n\n# Operator\n\n{text}{last_note}",
                briefing_for(
                    &talk.repo,
                    &cfg.graph.language,
                    cfg.talk.allow_write,
                    persona.as_ref(),
                    operator_name,
                    talk.implementers
                )
            )
        } else if talk.seat.turns == 0 {
            // A fresh seat on a conversation that already has history (the
            // agent was switched): the briefing, then everything said so far.
            format!(
                "{}\n\n{}\n\n# Operator\n\n{text}{last_note}",
                briefing_for(
                    &talk.repo,
                    &cfg.graph.language,
                    cfg.talk.allow_write,
                    persona.as_ref(),
                    operator_name,
                    talk.implementers
                ),
                transcript(talk, store)
            )
        } else if resuming {
            format!("{persona_update}{filing_update}{text}{last_note}")
        } else {
            // No session to hold the persona: the transcript never stores it,
            // so a non-default persona is re-sent on every such turn (the
            // update block already carries it when the choice just changed).
            let mut standing = match (&persona, talk.persona_dirty) {
                (Some(p), false) => format!("{}\n", crate::persona::section_for(p, operator_name)),
                _ => persona_update.clone(),
            };
            // The default voice has no persona section to carry the name, and
            // the transcript never stores the briefing's addressing line.
            if let (None, Some(n)) = (&persona, operator_name) {
                standing.push_str(&format!(
                    "# Addressing the operator\n{}\n",
                    crate::persona::addressing(n)
                ));
            }
            if talk.implementers_dirty || talk.implementers != 1 {
                standing.push_str(&format!("{}\n\n", filing_update_block(talk.implementers)));
            }
            format!("{}\n\n{standing}{text}{last_note}", transcript(talk, store))
        };
        let attempt_stem = if n == 0 {
            stem.clone()
        } else {
            format!("{stem}-{}", spec.id)
        };
        let inv = Invocation {
            cwd: &talk.repo,
            prompt: &body,
            timeout: turn_timeout(cfg),
            // Off unless this repository's own config opts in - see
            // `crate::config::Talk::allow_write` and this module's doc for why
            // the default keeps a conversational edit from landing in a checkout
            // no run or review can claim.
            allow_write,
            unsandboxed,
            sessions: cfg.graph.sessions,
            artifacts: &artifacts,
            stem: &attempt_stem,
            // The conversation's own id, so `magi task add` run from inside it is
            // attributed to this conversation - see `Source::Agent`.
            run: &talk.id,
            node: crate::queue::CHAT_NODE,
            cache_dir: cache_dir.as_deref(),
            attachments: &attachment_paths,
            writable: &consult_roots,
        };
        let result = agent::invoke(spec, &mut talk.seat, &inv).await;
        let advance = agent::chain_advances(&result);
        if n == 0 || !advance {
            outcome = Some(result);
        } else {
            // A later failure is only logged; the note describes the first.
            tracing::warn!("chat: fallback agent `{}` also failed", spec.id);
        }
        if !advance {
            break;
        }
    }
    if outcome.as_ref().is_some_and(agent::chain_advances) {
        // Exhausted: back to the agent the conversation had, so the note
        // below names it and the next turn starts from it again.
        if let Some((id, seat)) = first_try {
            talk.agent = id;
            talk.seat = seat;
            fell_back_from = None;
        }
    }
    let outcome = outcome.expect("a chain holds at least one agent");
    let note = |why: String| Turn {
        breaks: None,
        who: Who::Agent,
        body: format!("{MAGI_NOTE}{why}"),
        at: Timestamp::now(),
        attachments: Vec::new(),
        usage: None,
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
                turn_timeout(cfg).as_secs()
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
                breaks: None,
                who: Who::Agent,
                body: out.text.trim().to_owned(),
                at: Timestamp::now(),
                attachments: Vec::new(),
                // `talk.agent` is whoever actually answered: a fallback has
                // already moved it, and an exhausted chain never reaches here.
                usage: out.context_tokens.map(|context_tokens| TurnUsage {
                    context_tokens,
                    agent: talk.agent.clone(),
                    model: cfg
                        .agents
                        .iter()
                        .find(|a| a.id == talk.agent)
                        .and_then(|a| a.model.clone()),
                }),
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
    let _guard = store.guard()?;
    // A delete is the more final version of that same race: `put` writes
    // unconditionally, so a talk removed while this turn was in flight must
    // stay removed rather than being written back with this turn's reply
    // appended to it. The reply is simply given up on - there is no
    // conversation left for it to belong to.
    let Ok(fresh) = store.get(&talk.id) else {
        return Ok(());
    };
    talk.status = fresh.status;
    // `queue` may have accepted another operator message while the CLI was
    // running. This handle predates that write, so preserving only `status`
    // would overwrite the durable draft when the reply is appended below.
    talk.pending = fresh.pending;
    talk.pending_breaks = fresh.pending_breaks;
    talk.pending_attachments = fresh.pending_attachments;
    if let Some(from) = fell_back_from.filter(|_| failure.is_none()) {
        // The switch persists: quota coming back does not move the chat
        // home, an operator's switch does.
        talk.turns.push(note(format!(
            "agent changed from {from} to {} (fallback)",
            talk.agent
        )));
    }
    // The persona is in the session's context only once a turn has answered
    // (a failed one may never have reached the agent), so only then is it clean.
    if failure.is_none() {
        talk.persona_dirty = false;
        talk.implementers_dirty = false;
    }
    talk.turns.push(reply);
    if let Err(put_err) = store.put(talk) {
        // `Talks::put` already retried the write itself - reaching here
        // means a passing race is not what this is. An agent's answer,
        // possibly the result of an hour-long call, must not vanish with
        // nothing to show for it just because the very last step failed:
        // pop it back off, stash its text beside the conversation, and
        // replace it with a note the operator can actually see, the same
        // mechanism the failure branches above already use for a quota or a
        // timeout.
        let lost = talk.turns.pop().expect("just pushed above");
        let stash = stash_lost_turn(store, &talk.id, &stem, &lost);
        let why = match &stash {
            Ok(path) => format!(
                "agent `{}` answered, but the reply could not be saved to \
                 this conversation ({put_err:#}); the raw text was kept at \
                 {} - your message is saved, ask again",
                talk.agent,
                path.display()
            ),
            Err(stash_err) => format!(
                "agent `{}` answered, but the reply could not be saved to \
                 this conversation ({put_err:#}), and it could not be kept \
                 anywhere else either ({stash_err:#}); your message is \
                 saved, ask again",
                talk.agent
            ),
        };
        talk.turns.push(note(why.clone()));
        // Writing the note also carries the seat this call already advanced -
        // `agent::invoke` incremented `turns` and, for a vendor that reports
        // its own session id, recorded that too. That is what keeps the next
        // turn resuming the session the CLI is already holding instead of
        // re-opening it, so losing the reply costs the transcript a turn but
        // not the conversation.
        return match store.put(talk) {
            Ok(()) => bail!("{why}"),
            Err(note_err) => {
                // Even the short note failed to save, which means this
                // conversation's file cannot be written at all right now -
                // nothing is left for this call to retry or record. Pop the
                // note so `talk.turns` matches the transcript on disk, and
                // surface both failures for whoever reads the log.
                //
                // `talk.seat` is deliberately not wound back to match. The
                // CLI really did take the turn and really did consume this
                // seat's session id; pretending otherwise would be a second
                // untruth on top of the unwritable file, and the handle is
                // reloaded from disk by the next `drain` or `get` anyway -
                // see `web::drain_loop`. What the seat cannot do is reach
                // disk, so the record stays a turn behind the CLI until some
                // later write lands, and a turn taken before then re-opens a
                // session id the CLI already holds. That is the desync
                // `20260907-011805-fb57` is about, and tolerating it belongs
                // there rather than here: no write this branch could make
                // would help, since a failed write is exactly what put it in
                // this position twice over.
                talk.turns.pop();
                Err(note_err).context(why)
            }
        };
    }

    match failure {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
}

/// Everything said so far, as prose, for a CLI that cannot resume its own
/// conversation.
fn transcript(talk: &Talk, store: &Talks) -> String {
    let mut out = String::from(
        "This conversation cannot resume on the CLI's side, so here is \
         everything said so far; answer only the last message.\n",
    );
    for t in &talk.turns {
        let who = match t.who {
            Who::Operator => "operator",
            Who::Agent if t.body.starts_with(MAGI_NOTE) => "magi",
            Who::Agent => "you",
        };
        out.push_str(&format!("\n## {who}\n\n{}\n", t.body.trim()));
        out.push_str(&attachment_note(store, &talk.id, &t.attachments));
    }
    out
}

/// The section named at the end of a turn's body, listing every attachment's
/// absolute path and mime so the agent knows exactly what to open. Empty
/// when `attachments` is, which is every turn but the rare one carrying an
/// image, so a turn with none changes nothing about the prompt.
fn attachment_note(store: &Talks, talk_id: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nThe operator attached the image(s) below to this message. Open \
         and look at each one before you answer.\n",
    );
    for att in attachments {
        if let Some(path) = store.attachment_path(talk_id, att) {
            out.push_str(&format!("\n- {} ({})", path.display(), att.mime));
        }
    }
    out.push('\n');
    out
}

/// The briefing the agent opens with, sent once as part of its first turn.
///
/// Pure, so the properties that matter can be asserted without an interview:
/// it names `magi task add --solo` (the route this conversation always has to
/// changing anything) and it never tells the agent to write a task *file* of
/// its own - that would compete with filing through the queue.
/// What a turn may do: `(allow_write, unsandboxed)`. Only the repository's own
/// `[talk] allow_write` lifts the CLI sandbox (codex's counterpart of claude's
/// `bypassPermissions`); a turn writable merely because a handed-over question
/// is open keeps the sandbox and gets just the question store as a writable root.
pub(crate) fn turn_access(talk_allow_write: bool, consulted: bool) -> (bool, bool) {
    (talk_allow_write || consulted, talk_allow_write)
}

/// `allow_write` only ever adds an extra permission on top of that; it never
/// removes the queue as an option, which is why both branches keep the same
/// `# When the operator wants something done` section - `write_policy` is
/// the only part that changes.
///
/// It also tells the agent that `--repo` is not stuck naming this
/// conversation's own directory: `resolve_repo` (`src/main.rs`) now accepts a
/// short `owner/repo` or bare `repo` name and resolves it against
/// `[repos] roots`, the same local checkouts `magi repos` lists. Without this
/// line an agent asked to change some other repository has no way to know
/// that option exists, and the only path it can see - asking the operator to
/// dictate a full path - is exactly the friction this change exists to
/// remove. A miss or an ambiguous name still fails the command outright, so
/// the instruction is to ask rather than guess when that happens - the
/// silent-decision line this task must not cross.
pub fn briefing(repo: &Path, language: &str, allow_write: bool) -> String {
    briefing_with(repo, language, allow_write, None, None)
}

/// [`briefing`] plus the selected persona's tone-only section and, when the
/// operator has a configured name, how to address them. `None` for both is
/// byte-for-byte the plain briefing. A name changed mid-session reaches a
/// running CLI session only on its next fresh seat (nothing is persisted).
pub fn briefing_with(
    repo: &Path,
    language: &str,
    allow_write: bool,
    persona: Option<&crate::persona::Persona>,
    operator_name: Option<&str>,
) -> String {
    briefing_for(repo, language, allow_write, persona, operator_name, 1)
}

/// The most implementers a chat can ask its filed tasks to use.
pub const MAX_IMPLEMENTERS: u8 = 3;

fn solo_implementers() -> u8 {
    1
}

/// The `magi task add` flag for `n` implementers: `--solo` for one,
/// `--implementers N` otherwise (the two are never combined).
fn filing_flag(n: u8) -> String {
    if n <= 1 {
        "--solo".to_owned()
    } else {
        format!("--implementers {n}")
    }
}

/// The note a resumed (or transcript-replayed) turn opens with after the
/// implementer count changed, or while it is not Solo.
pub fn filing_update_block(n: u8) -> String {
    let flag = filing_flag(n);
    if n <= 1 {
        format!(
            "# Task filing update\n\nThe operator changed how many implementers tasks use. \
             From now on file with `magi task add {flag} --repo <repo> <instruction>` \
             (one implementer, straight into review).\n"
        )
    } else {
        format!(
            "# Task filing update\n\nThe operator changed how many implementers tasks use. \
             From now on file with `magi task add {flag} --repo <repo> <instruction>` \
             ({n} independent implementations compete). Do not pass `--solo` \
             together with `--implementers`; drop any earlier `--solo`.\n"
        )
    }
}

/// Validate a requested implementer count: 1..=3, and one the repository's
/// config can actually resolve. Never clamps.
pub fn check_implementers(n: u8, cfg: &crate::config::Config) -> std::result::Result<u8, String> {
    if !(1..=MAX_IMPLEMENTERS).contains(&n) {
        return Err(format!(
            "implementers must be 1..={MAX_IMPLEMENTERS}, got {n}"
        ));
    }
    if n > 1 {
        let mut cfg = cfg.clone();
        crate::queue::RunOverrides {
            candidates: Some(usize::from(n)),
            ..Default::default()
        }
        .apply(&mut cfg);
        cfg.resolve_roles()
            .map_err(|e| format!("{n} implementers is not allowed here: {e:#}"))?;
    }
    Ok(n)
}

/// Set how many implementers this chat files tasks with. Same shape as
/// [`switch_persona`]: `false` and no write when unchanged.
pub fn switch_implementers(talk: &mut Talk, store: &Talks, n: u8) -> Result<bool> {
    let _guard = store.guard()?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if fresh.implementers == n {
        *talk = fresh;
        return Ok(false);
    }
    let from = std::mem::replace(&mut fresh.implementers, n);
    fresh.implementers_dirty = true;
    fresh.turns.push(Turn {
        breaks: None,
        who: Who::Agent,
        body: format!("{MAGI_NOTE}implementers changed from {from} to {n}"),
        at: Timestamp::now(),
        attachments: Vec::new(),
        usage: None,
    });
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(true)
}

/// [`briefing_with`] for a chat that files tasks with `implementers`
/// implementers (1 is the unchanged `--solo` text).
pub fn briefing_for(
    repo: &Path,
    language: &str,
    allow_write: bool,
    persona: Option<&crate::persona::Persona>,
    operator_name: Option<&str>,
    implementers: u8,
) -> String {
    let flag = filing_flag(implementers);
    let shape = if implementers <= 1 {
        "Use --solo: it runs the task through one implementer \
         straight into review instead of the usual multi-agent competition, \
         which is the right shape for a change this conversation has already \
         settled, rather than one still worth several independent takes."
            .to_owned()
    } else {
        format!(
            "Use --implementers {implementers}: it has {implementers} \
             implementers work on the task independently and compete, which \
             is the right shape when several independent takes are worth \
             having. Never add --solo to it; the two do not go together."
        )
    };
    let write_policy = if allow_write {
        "Write access is enabled for this conversation (`allow_write = \
         true`), so you may write files - but only a small, \
         already-decided edit the operator names outright in this \
         conversation, not an implementation. This is a permission on the \
         conversation as a whole, not a property of whichever repository \
         it happened to start in: if the operator names a different \
         repository for that small edit, the policy allows it there too. \
         Your own tool may still confine writes to the repository this \
         conversation started in regardless - if a write elsewhere is \
         refused, say so plainly rather than working around it. Once you \
         have made an edit, say plainly what you edited. Anything bigger, \
         or anything still open-ended, still goes through the queue below \
         rather than being done here."
    } else {
        "Do not write files. Implementing a change is not this \
         conversation's job; a separate, blind competition of agents does \
         that, and a repository this conversation has already edited would \
         make their diffs unjudgeable."
    };
    let mut out = format!(
        "You are magi's standing conversation partner for its operator, who \
         usually has this open on a phone. Keep replies short: no preamble, \
         no restating what they just said.\n\n\
         # Repository\n\n{repo}\n\n\
         You may look around: read files, run shell commands, search history, \
         run tests - whatever answers the question. {write_policy}\n\n\
         A short, command-shaped message (\"list\", \"info <id>\", \"show \
         3cbf\") is almost always the operator asking you to look something \
         up, not an instruction to file - answer it yourself with `magi \
         list`, `magi show <id>`, `magi task list`, or the like, the same way \
         you would answer any other question in this conversation.\n\n\
         # When the operator wants something done\n\n\
         Run:\n\n\
         magi task add {flag} --repo {repo} <instruction>\n\n\
         and tell the operator the task id it prints, so they can follow it \
         from the Queue. If it refuses with a duplicate warning (the \
         instruction names a branch, commit or pull request that an \
         unfinished task, run or PR already owns), do not repeat it with \
         --force yourself: tell the operator what it matched and let them \
         decide. Write <instruction> so that an implementer who has \
         never seen this conversation can act on it alone - it is everything \
         they get. {shape}\n\n\
         If the operator asks for something in a different repository, \
         --repo does not have to be a full path: --repo owner/repo (or just \
         repo, when that is unambiguous) is resolved against local checkouts \
         the same way `magi repos` lists them. If the command fails because \
         nothing matches or more than one checkout shares that name, ask the \
         operator which repository they mean (or run `magi repos` yourself \
         to see the candidates) rather than guessing.\n\n\
         The current state of the code is whatever origin/main holds, not \
         whatever a working tree shows: a primary checkout often lags \
         upstream, sits on a detached HEAD and carries uncommitted changes. \
         Before answering about code, run `git fetch origin` in that \
         repository if it is cheap, then read through \
         `git show origin/main:<path>` or `git grep <pattern> origin/main`. \
         If the working tree differs, say so; if the fetch fails, say that \
         too, so the operator knows the answer may be stale.\n\n\
         If the operator attached an image (a screenshot, say) that the task \
         is about, pass it with `--attach <path>`, using the absolute path \
         the turn's attachment note gives; repeat the flag for several. \
         `magi task add {flag} --attach <path> <instruction>` copies the \
         file into the task, so the implementer receives it. Do not paste the \
         path into <instruction> instead: deleting this conversation deletes \
         its attachments, and then that path reaches no one.\n",
        repo = repo.display(),
    );
    out.push_str(&language_note(language));
    match (persona, operator_name) {
        (Some(p), n) => out.push_str(&crate::persona::section_for(p, n)),
        (None, Some(n)) => {
            out.push_str("\n# Addressing the operator\n");
            out.push_str(&crate::persona::addressing(n));
        }
        (None, None) => {}
    }
    out
}

/// The operator is talking, so their language matters here more than in most
/// prompts magi sends.
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

/// How many times [`write_atomic`] retries a failed write-then-rename before
/// giving up.
const PUT_RETRIES: u32 = 5;

/// Write `body` to `tmp` and rename it onto `path`, retrying the whole thing
/// a handful of times with a short sleep in between.
///
/// The only failure this is meant to absorb is a passing one - most
/// concretely, a reader elsewhere in this process (or another `magi`
/// process) with `path` briefly open for `read_to_string` at the exact
/// moment this call tries to rename over it. That clears in milliseconds
/// once the reader lets go; a caller still failing after several short
/// sleeps has something more durable wrong (a full disk, a permissions
/// change) that a longer sleep would not fix either, and is left to report
/// it.
fn write_atomic(tmp: &Path, path: &Path, body: &str) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..PUT_RETRIES {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(20 * u64::from(attempt)));
        }
        match try_write_atomic(tmp, path, body) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("the loop above always runs at least once"))
}

fn try_write_atomic(tmp: &Path, path: &Path, body: &str) -> Result<()> {
    #[cfg(test)]
    if failpoint::take_forced_put_failure() {
        bail!("simulated write failure (test)");
    }
    std::fs::write(tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Last resort when `turn`'s own `store.put` fails even after
/// [`write_atomic`]'s retries: keep the generated text somewhere still
/// findable rather than let the whole of an agent's answer disappear along
/// with the write that was supposed to record it.
fn stash_lost_turn(store: &Talks, id: &str, stem: &str, reply: &Turn) -> Result<PathBuf> {
    let dir = store.artifacts_of(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{stem}-lost.txt"));
    std::fs::write(&path, &reply.body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// A test-only seam that lets [`try_write_atomic`] simulate the kind of
/// passing I/O race [`write_atomic`] is meant to retry through, without
/// depending on real OS-level file-locking behaviour, which differs across
/// the three platforms this crate ships on (and, on the one platform where a
/// reader really does block a rename, is awkward to trigger deterministically
/// in a unit test).
#[cfg(test)]
mod failpoint {
    use std::cell::Cell;

    thread_local! {
        static FORCE_PUT_FAILURES: Cell<u32> = const { Cell::new(0) };
        static FORCE_NO_LINK: Cell<bool> = const { Cell::new(false) };
    }

    /// While set, `hard_link` on this thread fails as unsupported, so the
    /// create-and-rename fallback runs.
    pub(super) fn force_no_link(on: bool) {
        FORCE_NO_LINK.with(|c| c.set(on));
    }

    pub(super) fn no_link_forced() -> bool {
        FORCE_NO_LINK.with(Cell::get)
    }

    /// Arrange for the next `count` calls into [`super::try_write_atomic`] to
    /// fail before touching the filesystem at all.
    pub(super) fn force_put_failures(count: u32) {
        FORCE_PUT_FAILURES.with(|c| c.set(count));
    }

    /// Consumed once per attempt inside [`super::try_write_atomic`]; `true`
    /// means simulate this attempt failing.
    pub(super) fn take_forced_put_failure() -> bool {
        FORCE_PUT_FAILURES.with(|c| {
            let n = c.get();
            if n == 0 {
                false
            } else {
                c.set(n - 1);
                true
            }
        })
    }
}

fn short(id: &str) -> &str {
    id.split('-').next_back().unwrap_or(id)
}

fn new_id() -> String {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
    let seed = crate::rng::entropy();
    format!("{stamp}-{:04x}", (seed ^ (seed >> 32)) & 0xffff)
}

/// Extension an attachment's bytes are stored under, from its (already
/// validated) mime. The one place this mapping exists on the write side;
/// `web`'s own whitelist is what actually decides which mimes are accepted
/// in the first place.
fn attachment_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// Is `id` a shape [`put_attachment`](Talks::put_attachment) could have
/// produced? 32 lowercase hex digits and nothing else, checked before an id
/// that came from the client is ever allowed to build a path - so `..` and a
/// path separator are never even possible.
pub fn valid_attachment_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A fresh attachment id: 128 bits of process entropy as lowercase hex - the
/// same "mint it, never take it from the client" rule [`new_id`] follows for
/// conversation ids.
fn new_attachment_id() -> String {
    let mut r = crate::rng::SplitMix64::new(crate::rng::entropy());
    format!("{:016x}{:016x}", r.next_u64(), r.next_u64())
}

#[cfg(test)]
mod tests {
    #[test]
    fn turn_access_lifts_the_sandbox_only_for_the_talk_opt_in() {
        use super::turn_access;
        assert_eq!(turn_access(false, false), (false, false));
        assert_eq!(turn_access(true, false), (true, true));
        // A handed-over question opens the question store, never the sandbox.
        assert_eq!(turn_access(false, true), (true, false));
        assert_eq!(turn_access(true, true), (true, true));
    }

    #[test]
    fn the_briefing_points_at_origin_main_not_the_working_tree() {
        let b = briefing(Path::new("/r"), "en", false);
        assert!(b.contains("origin/main"));
        assert!(b.contains("git show origin/main:"));
    }
    use std::collections::BTreeMap;

    use crate::config::{AgentChoice, AgentKind, AgentSpec, Graph};
    use crate::queue::{Queue, Source, Task};

    use super::*;

    fn ctx_agent(id: &str, model: Option<&str>) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            kind: AgentKind::Command,
            model: model.map(str::to_owned),
            command: Vec::new(),
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }
    }

    fn ctx_talk(agent: &str, turns: Vec<Turn>) -> Talk {
        Talk {
            pending_breaks: None,
            schema: SCHEMA,
            id: "20260904-014455-ab12".to_owned(),
            repo: PathBuf::from("."),
            agent: agent.to_owned(),
            status: TalkStatus::Open,
            turns,
            pending: String::new(),
            pending_attachments: Vec::new(),
            fallback: false,
            persona: String::new(),
            persona_dirty: false,
            implementers: 1,
            implementers_dirty: false,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            seat: SeatState::new(SEAT, agent, 1),
        }
    }

    fn reply(body: &str, usage: Option<(u64, &str, Option<&str>)>) -> Turn {
        Turn {
            breaks: None,
            who: Who::Agent,
            body: body.to_owned(),
            at: Timestamp::now(),
            attachments: Vec::new(),
            usage: usage.map(|(t, a, m)| TurnUsage {
                context_tokens: t,
                agent: a.to_owned(),
                model: m.map(str::to_owned),
            }),
        }
    }

    fn ctx_config(windows: &[(&str, u64)]) -> Config {
        Config {
            agents: vec![
                ctx_agent("small", Some("small-model")),
                ctx_agent("big", Some("big-model")),
                ctx_agent("plain", None),
            ],
            context_windows: windows.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn context_usage_computes_percent_and_warns_at_eighty() {
        let cfg = ctx_config(&[("small-model", 1000)]);
        let at = |tokens| {
            let t = ctx_talk(
                "small",
                vec![reply("hi", Some((tokens, "small", Some("small-model"))))],
            );
            context_usage(&t, Some(&cfg))
        };
        let u = at(799);
        assert_eq!((u.percent, u.warn, u.window), (Some(79), false, Some(1000)));
        let u = at(800);
        assert_eq!((u.percent, u.warn), (Some(80), true));
        let u = at(1500);
        assert_eq!((u.percent, u.warn), (Some(150), true));
        assert!(!u.since_switch);
    }

    #[test]
    fn context_usage_is_unknown_without_usage_and_never_looks_back() {
        let cfg = ctx_config(&[("small-model", 1000)]);
        let t = ctx_talk(
            "small",
            vec![
                reply("old", Some((900, "small", Some("small-model")))),
                reply("new", None),
            ],
        );
        let u = context_usage(&t, Some(&cfg));
        // No measurement in the latest reply: an estimate, never the stale 900.
        assert!(u.estimated);
        assert_ne!(u.tokens, Some(900));
        assert!(u.tokens.is_some());
        // A magi note after the reply neither hides nor replaces it.
        let t = ctx_talk(
            "small",
            vec![
                reply("old", Some((900, "small", Some("small-model")))),
                reply("magi: could not run agent", None),
            ],
        );
        assert_eq!(context_usage(&t, Some(&cfg)).tokens, Some(900));
        assert_eq!(
            context_usage(&ctx_talk("small", Vec::new()), Some(&cfg)).tokens,
            None
        );
    }

    #[test]
    fn estimate_counts_chars_both_sides_and_standing_prompt() {
        let mut t = ctx_talk("small", vec![reply("abcdefg", None)]);
        assert_eq!(estimate_context_tokens(&t, 0), Some(2)); // 7 chars -> 2
        let op = Turn {
            who: Who::Operator,
            ..reply("abcdefg", None)
        };
        t.turns.push(op);
        assert_eq!(estimate_context_tokens(&t, 0), Some(4));
        assert!(
            estimate_context_tokens(&t, 700).unwrap() > estimate_context_tokens(&t, 0).unwrap()
        );
        // Characters, not bytes: 7 kanji are 7 chars.
        let ja = ctx_talk("small", vec![reply("日本語日本語日", None)]);
        assert_eq!(estimate_context_tokens(&ja, 0), Some(2));
        // magi notes are not sent to the agent; with nothing else, unknown.
        let note = ctx_talk("small", vec![reply("magi: could not run agent", None)]);
        assert_eq!(estimate_context_tokens(&note, 1000), None);
        assert_eq!(
            estimate_context_tokens(&ctx_talk("small", Vec::new()), 1000),
            None
        );
    }

    #[test]
    fn context_usage_measured_wins_and_estimate_gets_percent_and_warn() {
        let cfg = ctx_config(&[("small-model", 1000)]);
        let t = ctx_talk(
            "small",
            vec![reply(
                &"x".repeat(5000),
                Some((10, "small", Some("small-model"))),
            )],
        );
        let u = context_usage(&t, Some(&cfg));
        assert_eq!((u.tokens, u.estimated), (Some(10), false));
        let t = ctx_talk("small", vec![reply(&"x".repeat(5000), None)]);
        let u = context_usage(&t, Some(&cfg));
        assert!(u.estimated && !u.since_switch);
        assert_eq!(u.window, Some(1000));
        assert!(u.warn && u.percent.unwrap() >= 80);
        let t = ctx_talk("small", vec![reply("hi", None)]);
        let u = context_usage(&t, Some(&cfg));
        assert!(u.estimated && u.percent.is_some());
    }

    #[test]
    fn context_usage_without_a_window_shows_tokens_only() {
        let cfg = ctx_config(&[]);
        // No model at all, and a model nothing matches.
        let t = ctx_talk("plain", vec![reply("hi", Some((5000, "plain", None)))]);
        let u = context_usage(&t, Some(&cfg));
        assert_eq!(
            (u.tokens, u.window, u.percent, u.warn),
            (Some(5000), None, None, false)
        );
        let t = ctx_talk(
            "small",
            vec![reply("hi", Some((5000, "small", Some("small-model"))))],
        );
        assert_eq!(context_usage(&t, Some(&cfg)).percent, None);
        // No readable config: same, and no panic.
        assert_eq!(context_usage(&t, None).window, None);
    }

    #[test]
    fn context_usage_switching_model_changes_the_denominator() {
        let cfg = ctx_config(&[("small-model", 1000), ("big-model", 10_000)]);
        let used = reply("hi", Some((900, "small", Some("small-model"))));
        let before = context_usage(&ctx_talk("small", vec![used.clone()]), Some(&cfg));
        assert_eq!(
            (before.percent, before.warn, before.since_switch),
            (Some(90), true, false)
        );
        // Same turns, conversation now on the big model: new denominator, and
        // the figure is flagged as describing the previous session.
        let after = context_usage(&ctx_talk("big", vec![used]), Some(&cfg));
        assert_eq!(after.window, Some(10_000));
        assert_eq!(
            (after.percent, after.warn, after.since_switch),
            (Some(9), false, true)
        );
        assert_eq!(after.model.as_deref(), Some("big-model"));
    }

    #[test]
    fn a_turn_recorded_before_usage_existed_still_reads() {
        let old = r#"{"who":"agent","body":"hi","at":"2026-09-04T01:44:55Z"}"#;
        let turn: Turn = serde_json::from_str(old).expect("old turn reads");
        assert!(turn.usage.is_none());
        let json = serde_json::to_string(&turn).expect("serialize");
        assert!(
            !json.contains("usage"),
            "absent usage is not written: {json}"
        );
    }

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
            pending_breaks: None,
            schema: SCHEMA,
            id: "20260904-014455-ab12".to_owned(),
            repo: tmp.path().to_owned(),
            agent: "sonnet".to_owned(),
            status: TalkStatus::Open,
            turns: Vec::new(),
            pending: String::new(),
            pending_attachments: Vec::new(),
            fallback: false,
            persona: String::new(),
            persona_dirty: false,
            implementers: 1,
            implementers_dirty: false,
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

    /// `[roles] chatter`, when set, decides who holds this conversation; unset,
    /// it falls back to [`agent::pick`]'s own default order (a claude seat,
    /// else the first runnable agent in roster order) rather than to any
    /// other role - see `[roles] chatter`'s own doc in [`crate::config`] for
    /// why a dedicated field exists at all: opening this against the same
    /// seat as a judge is what produced the `agent ... did not answer within
    /// 300s` timeout that led to it.
    #[test]
    fn chatter_wins_when_set_and_falls_back_to_pick_s_default_order_otherwise() {
        let (tmp, talks) = store();
        let first_spec = mock_agent(tmp.path(), BROKEN, BTreeMap::new());
        let mut chatter_spec = mock_agent(tmp.path(), BROKEN, BTreeMap::new());
        chatter_spec.id = "chatter-mock".to_owned();

        let mut cfg = Config {
            agents: vec![first_spec.clone(), chatter_spec.clone()],
            graph: Graph {
                language: "en".to_owned(),
                ..Graph::default()
            },
            ..Config::default()
        };
        cfg.roles.chatter = Some(chatter_spec.id.as_str().into());

        let talk =
            begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin with chatter set");
        assert_eq!(talk.agent, chatter_spec.id, "an explicit chatter must win");

        cfg.roles.chatter = None;
        let fallback =
            begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin with chatter unset");
        assert_eq!(
            fallback.agent, first_spec.id,
            "unset chatter must fall back to agent::pick's own default order"
        );
    }

    /// A conversation recorded before attachments existed - schema 1, no
    /// `attachments` key on any turn - must still read.
    #[test]
    fn a_talk_recorded_without_attachments_still_reads() {
        let (tmp, talks) = store();
        let path = talks.path_of("20260904-014455-ab12");
        std::fs::create_dir_all(talks.root()).expect("talks dir");
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": 1,
                "id": "20260904-014455-ab12",
                "repo": tmp.path(),
                "agent": "sonnet",
                "status": "open",
                "turns": [
                    { "who": "operator", "body": "still there?",
                      "at": Timestamp::now().to_string() },
                ],
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string(),
                "seat": SeatState::new(SEAT, "sonnet", 7),
            })
            .to_string(),
        )
        .expect("write pre-attachments talk");

        let talk = talks.get("20260904-014455-ab12").expect("must still read");
        assert!(talk.turns[0].attachments.is_empty());
    }

    fn lease_store() -> (tempfile::TempDir, Talks, Talks) {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path().join("talks");
        (tmp, Talks::at(root.clone()), Talks::at(root))
    }

    #[test]
    fn two_starters_on_one_talk_one_wins_and_the_other_is_refused() {
        let (_tmp, a, b) = lease_store();
        let won = a.claim_turn("t1").expect("claim").expect("first wins");
        assert!(
            b.claim_turn("t1").expect("claim").is_none(),
            "second is refused"
        );
        assert!(b.turn_held("t1"));
        assert!(
            b.claim_turn("t2").expect("claim").is_some(),
            "other talks are free"
        );
        drop(won);
    }

    #[test]
    fn a_stale_lease_is_taken_over_and_the_old_guard_cannot_release_it() {
        let (_tmp, a, b) = lease_store();
        let old = a.claim_turn("t1").expect("claim").expect("held");
        let later = Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(
                crate::ask::LEASE_TTL.as_secs() as i64 + 5,
            ))
            .expect("later");
        let new = b
            .claim_turn_at("t1", later)
            .expect("claim")
            .expect("a stale lease is taken over");
        drop(old);
        assert!(a.turn_held("t1"), "the old guard left the new lease alone");
        assert!(new.beat().expect("beat"), "the new owner still beats");
        drop(new);
        assert!(!a.turn_held("t1"));
    }

    #[tokio::test]
    async fn a_turn_whose_lease_was_taken_over_is_stopped() {
        let (_tmp, a, b) = lease_store();
        let old = a.claim_turn("t1").expect("claim").expect("held");
        let later = Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(
                crate::ask::LEASE_TTL.as_secs() as i64 + 5,
            ))
            .expect("later");
        let _new = b
            .claim_turn_at("t1", later)
            .expect("claim")
            .expect("taken over");
        let out = old
            .beating_every(Duration::from_millis(10), std::future::pending::<()>())
            .await;
        assert!(out.is_err(), "the displaced turn must stop, not run on");
    }

    #[tokio::test]
    async fn a_turn_that_finishes_is_returned_and_keeps_its_lease_beating() {
        let (_tmp, a, _b) = lease_store();
        let lease = a.claim_turn("t1").expect("claim").expect("held");
        let out = lease
            .beating_every(Duration::from_millis(5), async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                7
            })
            .await
            .expect("still ours");
        assert_eq!(out, 7);
        assert!(a.turn_held("t1"));
    }

    #[test]
    fn an_unreadable_lease_counts_as_stale() {
        let (_tmp, a, b) = lease_store();
        std::fs::create_dir_all(a.root()).expect("dir");
        std::fs::write(a.turn_path("t1"), "not json").expect("write");
        assert!(!a.turn_held("t1"));
        assert!(b.claim_turn("t1").expect("claim").is_some());
    }

    #[test]
    fn without_hard_links_a_claim_is_still_exclusive_and_a_young_placeholder_blocks() {
        let (_tmp, a, b) = lease_store();
        failpoint::force_no_link(true);
        let held = a.claim_turn("t1").expect("claim").expect("first wins");
        assert!(b.claim_turn("t1").expect("claim").is_none());
        assert!(a.turn_held("t1"));
        drop(held);
        assert!(!a.turn_held("t1"));
        // A writer between its exclusive create and its write: an empty file.
        let path = a.turn_path("t1");
        assert!(create_exclusive(&path, "").expect("placeholder"));
        assert!(b.claim_turn("t1").expect("claim").is_none(), "young: held");
        age_file(&path);
        assert!(b.claim_turn("t1").expect("claim").is_some(), "old: stale");
        // The lock publishes through the same path.
        let lease = a.turn_path("t2");
        let lock = TurnLock::take(&lease).expect("take").expect("lock");
        assert!(TurnLock::take(&lease).expect("take").is_none());
        drop(lock);
        assert!(TurnLock::take(&lease).expect("take").is_some());
        failpoint::force_no_link(false);
    }

    #[test]
    fn remove_if_carries_removes_only_the_expected_content() {
        let (_tmp, a, _b) = lease_store();
        std::fs::create_dir_all(a.root()).expect("dir");
        let p = a.root().join("x.turn.lock");
        std::fs::write(&p, "mine").expect("write");
        assert!(!remove_if_carries(&p, "other"));
        assert_eq!(std::fs::read_to_string(&p).expect("kept"), "mine");
        assert!(remove_if_carries(&p, "mine"));
        assert!(!p.exists());
        assert!(!remove_if_carries(&p, "mine"), "absent is not a removal");
    }

    #[test]
    fn a_dropped_lock_does_not_remove_a_lock_taken_over_since() {
        let (_tmp, a, _b) = lease_store();
        std::fs::create_dir_all(a.root()).expect("dir");
        let lease = a.turn_path("t1");
        let lock = TurnLock::take(&lease).expect("take").expect("lock");
        let path = lock.path.clone();
        std::fs::write(&path, "someone-else").expect("replace");
        drop(lock);
        assert_eq!(
            std::fs::read_to_string(&path).expect("kept"),
            "someone-else"
        );
    }

    #[test]
    fn concurrent_takeovers_of_a_stale_lease_have_one_winner() {
        let (_tmp, a, _b) = lease_store();
        drop(a.claim_turn("t1").expect("claim").expect("held"));
        std::fs::write(
            a.turn_path("t1"),
            serde_json::to_string(&TurnRecord {
                token: "gone".into(),
                pid: 1,
                beat_at: Timestamp::from_second(1).expect("ts"),
            })
            .expect("json"),
        )
        .expect("write");
        let wins: Vec<_> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|_| {
                    let s = a.clone();
                    sc.spawn(move || s.claim_turn("t1").expect("claim"))
                })
                .collect();
            hs.into_iter().map(|h| h.join().expect("join")).collect()
        });
        assert_eq!(wins.iter().filter(|w| w.is_some()).count(), 1);
    }

    fn age_file(path: &Path) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open");
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .expect("age");
    }

    #[test]
    fn a_late_taker_of_a_broken_lock_cannot_disturb_its_replacement() {
        let (_tmp, a, _b) = lease_store();
        let lease = a.turn_path("t1");
        let lock = lease.with_extension("turn.lock");
        std::fs::create_dir_all(lock.parent().expect("dir")).expect("dir");
        std::fs::write(&lock, "t1-dead").expect("dead lock");
        age_file(&lock);
        // B breaks the dead lock and holds a fresh one.
        let b = TurnLock::take(&lease).expect("take").expect("b wins");
        let fresh = std::fs::read_to_string(&lock).expect("read");
        assert_eq!(fresh, b.token);
        // C read the same dead token earlier: its ticket is already spent, and
        // the fresh lock is never moved, so a newcomer cannot slip in.
        let ticket = std::fs::read_dir(lock.parent().expect("dir"))
            .expect("dir")
            .flatten()
            .map(|e| e.path())
            .find(|p| p.to_string_lossy().ends_with(".break.0"))
            .expect("ticket");
        assert!(!create_exclusive(&ticket, "").expect("ticket"));
        assert!(TurnLock::take(&lease).expect("take").is_none());
        assert_eq!(std::fs::read_to_string(&lock).expect("read"), fresh);
        // A later generation is breakable once the old ticket is stale too.
        age_file(&lock);
        age_file(&ticket);
        std::mem::forget(b);
        let c = TurnLock::take(&lease)
            .expect("take")
            .expect("next generation");
        assert_ne!(c.token, fresh);
    }

    #[test]
    fn a_live_ticket_blocks_and_a_stale_one_hands_over_to_the_next_generation() {
        let (_tmp, a, _b) = lease_store();
        let lease = a.turn_path("t1");
        let lock = lease.with_extension("turn.lock");
        std::fs::create_dir_all(lock.parent().expect("dir")).expect("dir");
        std::fs::write(&lock, "t1-dead").expect("dead lock");
        age_file(&lock);
        let t0 = lock.with_extension("lock.t1-dead.break.0");
        assert!(create_exclusive(&t0, "").expect("ticket"));
        // A fresh ticket 0 blocks the same dead token, whatever the clock says.
        assert!(TurnLock::take(&lease).expect("take").is_none());
        assert_eq!(std::fs::read_to_string(&lock).expect("read"), "t1-dead");
        // Once ticket 0 is stale, exactly one taker gets through via ticket 1.
        age_file(&t0);
        let c = TurnLock::take(&lease).expect("take").expect("generation 1");
        assert_eq!(std::fs::read_to_string(&lock).expect("read"), c.token);
        assert!(lock.with_extension("lock.t1-dead.break.1").exists());
        assert!(TurnLock::take(&lease).expect("take").is_none());
    }

    #[test]
    fn exhausted_ticket_generations_recover_once_the_sweep_ages_them_out() {
        let (_tmp, a, _b) = lease_store();
        let lease = a.turn_path("t1");
        let lock = lease.with_extension("turn.lock");
        std::fs::create_dir_all(lock.parent().expect("dir")).expect("dir");
        std::fs::write(&lock, "t1-dead").expect("dead lock");
        age_file(&lock);
        let tickets: Vec<_> = (0..TICKET_GENERATIONS)
            .map(|n| lock.with_extension(format!("lock.t1-dead.break.{n}")))
            .collect();
        for t in &tickets {
            assert!(create_exclusive(t, "").expect("ticket"));
            age_file(t);
        }
        // Stale but not yet swept: no recovery, and no ticket is unlinked.
        assert!(TurnLock::take(&lease).expect("take").is_none());
        assert!(tickets.iter().all(|t| t.exists()));
        // Past the sweep age they go, and the next attempt recovers.
        for t in &tickets {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(t)
                .expect("open");
            f.set_modified(std::time::SystemTime::now() - TICKET_SWEEP_AGE * 2)
                .expect("age");
        }
        assert!(TurnLock::take(&lease).expect("take").is_none());
        assert!(TurnLock::take(&lease).expect("take").is_some());
    }

    #[test]
    fn an_aged_empty_lock_is_broken() {
        let (_tmp, a, _b) = lease_store();
        let lease = a.turn_path("t1");
        let lock = lease.with_extension("turn.lock");
        std::fs::create_dir_all(lock.parent().expect("dir")).expect("dir");
        std::fs::write(&lock, "").expect("empty lock");
        age_file(&lock);
        assert!(TurnLock::take(&lease).expect("take").is_some());
    }

    #[test]
    fn queued_text_is_durable_combined_and_drained_as_one_operator_turn() {
        let (tmp, talks) = store();
        let cfg = config(mock_agent(tmp.path(), REPLY, env("reply")));
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        queue(&mut talk, &talks, "first", Vec::new()).expect("queue first");
        queue(&mut talk, &talks, "second", Vec::new()).expect("queue second");
        let saved = talks.get(&talk.id).expect("reload queued talk");
        assert_eq!(saved.pending, "first\n\nsecond");
        assert_eq!(saved.pending_breaks, Some(vec!["first\n\n".len()]));
        assert!(saved.turns.is_empty(), "a draft is not a transcript turn");

        let drained = drain(&mut talk, &talks).expect("drain");
        assert_eq!(drained.as_deref(), Some("first\n\nsecond"));
        let saved = talks.get(&talk.id).expect("reload drained talk");
        assert!(saved.pending.is_empty());
        assert_eq!(saved.turns.len(), 1);
        assert_eq!(saved.turns[0].body, "first\n\nsecond");
        assert_eq!(saved.turns[0].breaks, Some(vec!["first\n\n".len()]));
        assert_eq!(saved.pending_breaks, None);
    }

    #[test]
    fn editing_a_queued_draft_preserves_its_attachments_and_rejects_a_stale_snapshot() {
        let (tmp, talks) = store();
        let cfg = config(mock_agent(tmp.path(), REPLY, env("reply")));
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        let attachment = Attachment {
            id: "a".repeat(32),
            name: "shot.png".to_owned(),
            mime: "image/png".to_owned(),
            bytes: 3,
        };

        queue(&mut talk, &talks, "first", vec![attachment.clone()]).expect("queue");
        assert!(
            edit_pending_text(
                &mut talk,
                &talks,
                "corrected",
                "first",
                std::slice::from_ref(&attachment.id),
            )
            .expect("edit")
        );
        let saved = talks.get(&talk.id).expect("reload edited draft");
        assert_eq!(saved.pending, "corrected");
        assert_eq!(saved.pending_attachments, vec![attachment]);

        queue(&mut talk, &talks, "later", Vec::new()).expect("queue concurrent draft");
        assert!(
            !edit_pending_text(
                &mut talk,
                &talks,
                "stale edit",
                "corrected",
                &["a".repeat(32)],
            )
            .expect("stale edit is a conflict")
        );
        assert_eq!(
            talks.get(&talk.id).expect("reload after conflict").pending,
            "corrected\n\nlater"
        );
        assert!(
            !clear_pending_if_matches(&mut talk, &talks, "corrected", &["a".repeat(32)])
                .expect("stale clear is a conflict")
        );
        assert_eq!(
            talks
                .get(&talk.id)
                .expect("reload after stale clear")
                .pending,
            "corrected\n\nlater"
        );
    }

    #[tokio::test]
    async fn a_reply_save_preserves_pending_accepted_while_the_cli_runs() {
        let (tmp, talks) = store();
        let slow = "#!/bin/sh\ncat >/dev/null\nsleep 0.1\nprintf reply\n";
        let cfg = config(mock_agent(tmp.path(), slow, BTreeMap::new()));
        let mut running = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        let id = running.id.clone();
        let first = record(&mut running, &talks, "first", Vec::new()).expect("record");

        let response_talks = talks.clone();
        let response_cfg = cfg.clone();
        let reply = tokio::spawn(async move {
            respond(
                &response_talks.claim_turn(&running.id).unwrap().unwrap(),
                &mut running,
                &response_talks,
                &response_cfg,
                &first,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let mut queued = talks.get(&id).expect("queued handle");
        queue(&mut queued, &talks, "next", Vec::new()).expect("queue");
        reply.await.expect("join").expect("reply");

        let saved = talks.get(&id).expect("reload");
        assert_eq!(saved.pending, "next");
        assert_eq!(saved.pending_breaks, Some(Vec::new()));
        assert_eq!(saved.turns.len(), 2, "operator message and reply remain");
    }

    /// A mock whose script counts its own calls in `<dir>/<id>.calls` before
    /// running `body`.
    fn counting_agent(dir: &Path, id: &str, body: &str) -> AgentSpec {
        let calls = dir.join(format!("{id}.calls"));
        let script = format!(
            "#!/bin/sh\necho x >> '{}'\n{body}\n",
            calls.to_string_lossy()
        );
        let path = dir.join(format!("mock-{id}.sh"));
        std::fs::write(&path, script).expect("write mock");
        AgentSpec {
            id: id.to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["sh".to_owned(), path.to_string_lossy().into_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }
    }

    fn calls(dir: &Path, id: &str) -> usize {
        std::fs::read_to_string(dir.join(format!("{id}.calls"))).map_or(0, |s| s.lines().count())
    }

    fn chain_config(specs: Vec<AgentSpec>, ids: &[&str]) -> Config {
        let mut cfg = config(specs[0].clone());
        cfg.agents = specs;
        cfg.roles.chatter = Some(AgentChoice::Chain(
            ids.iter().map(|s| (*s).to_owned()).collect(),
        ));
        cfg
    }

    #[tokio::test]
    async fn a_chatter_chain_falls_back_resends_the_transcript_and_sticks() {
        let (tmp, talks) = store();
        let a = counting_agent(tmp.path(), "a", "cat >/dev/null\nexit 3");
        let b = counting_agent(tmp.path(), "b", "cat");
        let cfg = chain_config(vec![a, b], &["a", "b"]);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        assert_eq!(talk.agent, "a");

        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "hello there",
            Vec::new(),
        )
        .await
        .expect("turn");
        assert_eq!(calls(tmp.path(), "a"), 1, "each id is tried once");
        assert_eq!(calls(tmp.path(), "b"), 1);
        assert_eq!(talk.agent, "b", "the switch persists");
        assert!(talks.get(&talk.id).unwrap().agent == "b");
        let reply = talk.turns.last().unwrap();
        assert!(reply.body.contains("hello there"));
        assert!(
            reply.body.contains("magi task add --solo"),
            "a fresh seat gets the full briefing"
        );
        assert!(
            talk.turns
                .iter()
                .any(|t| t.body.contains("agent changed from a to b")),
            "the switch is noted"
        );
    }

    #[tokio::test]
    async fn an_exhausted_chatter_chain_fails_like_a_single_seat_and_stays_put() {
        let (tmp, talks) = store();
        let a = counting_agent(tmp.path(), "a", "cat >/dev/null\nexit 3");
        let b = counting_agent(tmp.path(), "b", "cat >/dev/null\nexit 4");
        let cfg = chain_config(vec![a, b], &["a", "b", "a"]);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let err = say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "hi",
            Vec::new(),
        )
        .await
        .expect_err("every agent failed");
        assert!(err.to_string().contains("`a`"), "{err:#}");
        assert_eq!(calls(tmp.path(), "a"), 1);
        assert_eq!(calls(tmp.path(), "b"), 1);
        assert_eq!(talk.agent, "a", "an exhausted chain leaves the agent alone");
    }

    #[test]
    fn a_chatter_chain_skips_an_unknown_id_at_begin() {
        let (tmp, talks) = store();
        let b = counting_agent(tmp.path(), "b", "cat");
        let cfg = chain_config(vec![b], &["ghost", "b"]);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        assert_eq!(talk.agent, "b");
    }

    #[tokio::test]
    async fn an_explicit_agent_inside_the_chatter_chain_stays_pinned() {
        let (tmp, talks) = store();
        let a = counting_agent(tmp.path(), "a", "cat >/dev/null\nexit 3");
        let b = counting_agent(tmp.path(), "b", "cat");
        let cfg = chain_config(vec![a, b], &["a", "b"]);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), Some("a")).expect("begin");
        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "hi",
            Vec::new(),
        )
        .await
        .expect_err("a alone, and it fails");
        assert_eq!(calls(tmp.path(), "b"), 0);
        assert_eq!(talk.agent, "a");
    }

    #[tokio::test]
    async fn an_explicit_agent_does_not_borrow_the_chatter_chain() {
        let (tmp, talks) = store();
        let a = counting_agent(tmp.path(), "a", "cat >/dev/null\nexit 3");
        let b = counting_agent(tmp.path(), "b", "cat");
        let c = counting_agent(tmp.path(), "c", "cat >/dev/null\nexit 3");
        let cfg = chain_config(vec![a, b, c], &["a", "b"]);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), Some("c")).expect("begin");
        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "hi",
            Vec::new(),
        )
        .await
        .expect_err("c alone, and it fails");
        assert_eq!(calls(tmp.path(), "b"), 0);
    }

    #[test]
    fn briefing_names_the_operator_with_or_without_a_persona() {
        let plain = briefing(Path::new("/repo"), "en", false);
        let named = briefing_with(Path::new("/repo"), "en", false, None, Some("Commander"));
        assert!(named.starts_with(&plain));
        assert!(named.contains("# Addressing the operator"));
        assert!(named.contains("\"Commander\""));
        let rei = crate::persona::builtin_catalog()
            .into_iter()
            .find(|p| p.id == "rei")
            .expect("rei");
        let with = briefing_with(
            Path::new("/repo"),
            "en",
            false,
            Some(&rei),
            Some("Commander"),
        );
        assert!(with.contains("\"Commander\""));
        assert!(!with.contains("# Addressing the operator"));
    }

    #[test]
    fn briefing_carries_a_persona_section_only_when_one_is_chosen() {
        let plain = briefing(Path::new("/repo"), "en", false);
        assert_eq!(
            plain,
            briefing_with(Path::new("/repo"), "en", false, None, None)
        );
        assert!(!plain.contains("Persona"));
        let rei = crate::persona::builtin_catalog()
            .into_iter()
            .find(|p| p.id == "rei")
            .expect("rei");
        let with = briefing_with(Path::new("/repo"), "en", false, Some(&rei), None);
        assert!(with.starts_with(&plain), "the plain briefing is untouched");
        assert!(with.contains("# Persona (tone only)"));
        assert!(with.contains("TONE ONLY"));
        assert!(with.contains("task ids"));
        assert!(with.contains("write policy"));
        assert!(with.contains("`magi task add`"));
        assert!(with.contains("Rei Ayanami"));
    }

    #[test]
    fn a_talk_written_before_personas_still_loads() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let cfg = config(spec);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        let path = talks.root.join(format!("{}.json", talk.id));
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("persona");
        v.as_object_mut().unwrap().remove("persona_dirty");
        std::fs::write(&path, v.to_string()).unwrap();
        let loaded = talks.get(&talk.id).expect("old record loads");
        assert_eq!(loaded.persona, "");
        assert!(!loaded.persona_dirty);
    }

    #[test]
    fn the_briefing_files_with_solo_or_the_chosen_implementer_count() {
        let repo = Path::new("/repo");
        let solo = briefing_for(repo, "", false, None, None, 1);
        assert_eq!(solo, briefing(repo, "", false));
        assert!(solo.contains("magi task add --solo --repo"));
        assert!(solo.contains("magi task add --solo --attach"));
        for n in [2u8, 3] {
            let b = briefing_for(repo, "", false, None, None, n);
            assert!(
                b.contains(&format!("magi task add --implementers {n} --repo")),
                "{b}"
            );
            assert!(b.contains(&format!("magi task add --implementers {n} --attach")));
            assert!(!b.contains("task add --solo"), "{b}");
            assert!(!b.contains("Use --solo"), "{b}");
        }
    }

    #[test]
    fn begin_with_applies_remembered_choices_and_drops_unfit_ones_alone() {
        let (tmp, talks) = store();
        let first = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let mut second = first.clone();
        second.id = "second".to_owned();
        let mut cfg = config(first.clone());
        cfg.agents.push(second.clone());
        let open = |p: &Preferred| {
            begin_with(&talks, &cfg, tmp.path().to_owned(), None, p).expect("begin")
        };

        let talk = open(&Preferred {
            agent: Some("second".into()),
            persona: Some("rei".into()),
            implementers: Some(i64::from(check_implementers(3, &cfg).map_or(1, |n| n))),
        });
        assert_eq!(talk.agent, "second");
        assert!(
            !talk.fallback,
            "a remembered non-head agent is an explicit choice"
        );
        assert_eq!(talk.persona, "rei");
        assert_eq!(talk.implementers, check_implementers(3, &cfg).unwrap_or(1));
        assert!(!talk.persona_dirty && !talk.implementers_dirty);

        // The chain head chosen again stays a fallback-capable talk.
        let head = open(&Preferred {
            agent: Some(first.id.clone()),
            ..Preferred::default()
        });
        assert!(head.fallback);

        // Each unfit field falls back by itself.
        for implementers in [0, 4, -1, 1000] {
            let t = open(&Preferred {
                agent: Some("gone".into()),
                persona: Some("no-such-persona".into()),
                implementers: Some(implementers),
            });
            assert_eq!(t.agent, first.id);
            assert!(t.fallback);
            assert_eq!(t.persona, "");
            assert_eq!(t.implementers, 1);
        }

        // `default` stays the empty string; a good field survives a bad one.
        let t = open(&Preferred {
            agent: Some("second".into()),
            persona: Some("default".into()),
            implementers: Some(9),
        });
        assert_eq!(
            (t.agent.as_str(), t.persona.as_str(), t.implementers),
            ("second", "", 1)
        );

        // An explicit agent still wins and keeps its strict error.
        let strict = begin_with(
            &talks,
            &cfg,
            tmp.path().to_owned(),
            Some("first-unknown"),
            &Preferred {
                agent: Some("second".into()),
                ..Preferred::default()
            },
        );
        assert!(strict.is_err());
    }

    #[test]
    fn implementers_are_validated_and_old_records_load_as_solo() {
        let (tmp, talks) = store();
        let cfg = config(mock_agent(tmp.path(), ECHO, BTreeMap::new()));
        assert_eq!(check_implementers(1, &cfg), Ok(1));
        assert_eq!(check_implementers(3, &cfg), Ok(3));
        assert!(check_implementers(0, &cfg).is_err());
        assert!(check_implementers(4, &cfg).is_err());
        let mut empty = cfg.clone();
        empty.agents.clear();
        assert!(check_implementers(2, &empty).is_err());

        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        let path = talks.path_of(&talk.id);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("implementers");
        v.as_object_mut().unwrap().remove("implementers_dirty");
        std::fs::write(&path, v.to_string()).unwrap();
        let loaded = talks.get(&talk.id).expect("load");
        assert_eq!(loaded.implementers, 1);
        assert!(!loaded.implementers_dirty);
    }

    #[tokio::test]
    async fn an_implementers_switch_is_noted_once_and_kept_across_a_failed_turn() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        let lease = talks.claim_turn(&talk.id).unwrap().unwrap();
        say(&lease, &mut talk, &talks, &cfg, "hello", Vec::new())
            .await
            .expect("first turn");

        assert!(switch_implementers(&mut talk, &talks, 2).expect("switch"));
        assert!(!switch_implementers(&mut talk, &talks, 2).expect("same"));
        assert!(talk.implementers_dirty);
        say(&lease, &mut talk, &talks, &cfg, "next", Vec::new())
            .await
            .expect("turn");
        let prompt = &talk.turns.last().unwrap().body;
        assert!(prompt.contains("# Task filing update"), "{prompt}");
        assert!(prompt.contains("--implementers 2"));
        assert!(!talk.implementers_dirty, "cleared after a successful turn");

        say(&lease, &mut talk, &talks, &cfg, "again", Vec::new())
            .await
            .expect("turn");
        assert!(
            !talk
                .turns
                .last()
                .unwrap()
                .body
                .contains("# Task filing update")
        );

        // Without a resumable session the current policy rides on every turn.
        let mut no_sessions = cfg.clone();
        no_sessions.graph.sessions = false;
        say(&lease, &mut talk, &talks, &no_sessions, "one", Vec::new())
            .await
            .expect("turn");
        say(&lease, &mut talk, &talks, &no_sessions, "two", Vec::new())
            .await
            .expect("turn");
        assert!(talk.turns.last().unwrap().body.contains("--implementers 2"));
    }

    #[tokio::test]
    async fn a_persona_switch_notes_marks_dirty_and_updates_the_next_turn_once() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        let lease = talks.claim_turn(&talk.id).unwrap().unwrap();
        say(&lease, &mut talk, &talks, &cfg, "hello", Vec::new())
            .await
            .expect("first turn");
        assert!(!talk.turns[1].body.contains("Persona"));

        assert!(switch_persona(&mut talk, &talks, "misato").expect("switch"));
        assert!(!switch_persona(&mut talk, &talks, "misato").expect("same"));
        assert!(talk.persona_dirty);
        assert!(talk.turns.last().unwrap().body.contains("persona changed"));

        say(&lease, &mut talk, &talks, &cfg, "next", Vec::new())
            .await
            .expect("turn");
        let prompt = &talk.turns.last().unwrap().body;
        assert!(prompt.contains("# Persona update"), "{prompt}");
        assert!(prompt.contains("Misato Katsuragi"));
        assert!(!talk.persona_dirty, "cleared after a successful turn");

        say(&lease, &mut talk, &talks, &cfg, "again", Vec::new())
            .await
            .expect("turn");
        assert!(!talk.turns.last().unwrap().body.contains("# Persona update"));

        assert!(switch_persona(&mut talk, &talks, "default").expect("back"));
        assert_eq!(talk.persona, "");
        say(&lease, &mut talk, &talks, &cfg, "plain", Vec::new())
            .await
            .expect("turn");
        assert!(
            talk.turns
                .last()
                .unwrap()
                .body
                .contains("turned the persona off")
        );

        // Without a resumable session every turn re-sends the persona.
        assert!(switch_persona(&mut talk, &talks, "rei").expect("rei"));
        let mut no_sessions = cfg.clone();
        no_sessions.graph.sessions = false;
        say(&lease, &mut talk, &talks, &no_sessions, "one", Vec::new())
            .await
            .expect("turn");
        say(&lease, &mut talk, &talks, &no_sessions, "two", Vec::new())
            .await
            .expect("turn");
        let last = &talk.turns.last().unwrap().body;
        assert!(!talk.persona_dirty);
        assert!(last.contains("# Persona (tone only)"), "{last}");
        assert!(last.contains("Rei Ayanami"));
    }

    #[tokio::test]
    async fn the_first_turn_carries_the_briefing_and_later_turns_do_not() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "what does the queue module do?",
            Vec::new(),
        )
        .await
        .expect("first turn");
        let first_prompt = &talk.turns[1].body;
        assert!(first_prompt.contains("magi task add --solo"));
        assert!(first_prompt.contains("what does the queue module do?"));

        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "and how is it locked?",
            Vec::new(),
        )
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
    async fn switching_agent_resets_the_seat_notes_it_and_resends_the_transcript() {
        let (tmp, talks) = store();
        let a = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let mut b = a.clone();
        b.id = "other".to_owned();
        let mut cfg = config(a.clone());
        cfg.agents.push(b.clone());
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), Some(&a.id)).expect("begin");
        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "remember the walrus",
            Vec::new(),
        )
        .await
        .expect("first turn");
        let old_session = talk.seat.claude_session.clone();
        assert_eq!(talk.seat.turns, 1);

        assert!(switch_agent(&mut talk, &talks, &b).expect("switch"));
        assert_eq!(talk.agent, "other");
        assert_eq!(talk.seat.turns, 0);
        assert_eq!(talk.seat.agent, "other");
        assert_ne!(talk.seat.claude_session, old_session);
        let note = talk.turns.last().expect("note");
        assert_eq!(note.who, Who::Agent);
        assert!(note.body.starts_with(MAGI_NOTE), "{}", note.body);
        assert!(note.body.contains("changed from"), "{}", note.body);
        assert_eq!(talks.get(&talk.id).expect("reload").agent, "other");

        let before = talk.turns.len();
        assert!(!switch_agent(&mut talk, &talks, &b).expect("same agent"));
        assert_eq!(talk.turns.len(), before, "a no-op writes no note");

        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "what did I say?",
            Vec::new(),
        )
        .await
        .expect("turn after switch");
        let prompt = &talk.turns.last().expect("reply").body;
        assert!(prompt.contains("remember the walrus"), "{prompt}");
        assert!(prompt.contains("## magi"), "{prompt}");
        assert!(prompt.contains("what did I say?"), "{prompt}");
    }

    #[tokio::test]
    async fn say_appends_the_operator_turn_then_the_agent_turn() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("go ahead"));
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "can I rename this function?",
            Vec::new(),
        )
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

        let err = say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "check the tests",
            Vec::new(),
        )
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

    /// The failure this stands in for: a reader elsewhere briefly has the
    /// talk file open right when `turn` tries to save the reply, and the
    /// write-then-rename fails once or twice before the reader lets go.
    /// `write_atomic`'s own retries must absorb that with nobody the wiser -
    /// no gap in the transcript, no dropped turn.
    #[tokio::test]
    async fn a_passing_write_failure_while_saving_the_reply_does_not_lose_it() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("go ahead"));
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let text =
            record(&mut talk, &talks, "can I rename this function?", Vec::new()).expect("record");
        // One fewer failure than `write_atomic` will retry through, so the
        // very last attempt must succeed.
        failpoint::force_put_failures(PUT_RETRIES - 1);
        respond(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            &text,
        )
        .await
        .expect("respond must survive a write failure its own retries can outlast");

        assert_eq!(talk.turns.len(), 2);
        assert_eq!(talk.turns[1].who, Who::Agent);
        assert_eq!(talk.turns[1].body, "go ahead");
        let on_disk = talks.get(&talk.id).expect("get");
        assert_eq!(
            on_disk.turns, talk.turns,
            "the reply must reach disk despite the early write failures"
        );
    }

    /// When the write-then-rename never recovers - standing in for a disk
    /// that stays unwritable rather than a reader that eventually lets go -
    /// the reply must not disappear without a trace the way it did in the
    /// real incident this repository saw: no error on the phone, no note in
    /// the transcript, and the turn simply gone from `talks/<id>.json`.
    #[tokio::test]
    async fn a_persistent_write_failure_while_saving_the_reply_is_never_silent() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("go ahead"));
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let text = record(&mut talk, &talks, "check the tests", Vec::new()).expect("record");
        // Exactly enough forced failures to exhaust the reply's own retries;
        // the shorter note that replaces it then saves cleanly, which is the
        // common case this exercises - a large write racing something,
        // followed by a small one that does not.
        failpoint::force_put_failures(PUT_RETRIES);
        let err = respond(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            &text,
        )
        .await
        .expect_err("a reply that cannot be saved must be reported, not swallowed");
        assert!(err.to_string().contains("could not be saved"), "{err}");

        let on_disk = talks.get(&talk.id).expect("get");
        assert_eq!(
            on_disk.turns.len(),
            2,
            "the operator turn plus a visible note"
        );
        assert_eq!(on_disk.turns[0].body, "check the tests");
        let note = &on_disk.turns[1];
        assert_eq!(note.who, Who::Agent);
        assert!(note.body.starts_with(MAGI_NOTE), "{}", note.body);
        assert!(
            note.body.contains("could not be saved"),
            "the operator must be told the reply is missing, not left staring \
             at a gap with no explanation: {}",
            note.body
        );
        assert_eq!(
            talk.turns, on_disk.turns,
            "the in-memory talk must match what actually landed on disk"
        );

        // The generated answer itself must still be recoverable, not merely
        // reported as lost.
        let artifacts = talks.artifacts_of(&talk.id);
        let stash = std::fs::read_dir(&artifacts)
            .expect("artifacts dir")
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().ends_with("-lost.txt"))
            .expect("a stash file for the lost reply");
        let stashed = std::fs::read_to_string(stash.path()).expect("read stash");
        assert_eq!(stashed, "go ahead");

        // Losing the reply must not also lose the seat. The CLI took a turn
        // and consumed this seat's session id; if the note's write left the
        // record claiming otherwise, the next turn would re-open a session
        // the CLI is already holding - the `20260907-011805-fb57` desync -
        // and would re-send the whole briefing besides. Both decisions read
        // the seat straight off disk (`agent::has_session` and `turn`'s own
        // `seat.turns == 0` branch), so this is the field that has to match.
        assert_eq!(
            on_disk.seat.turns, 1,
            "the note's write must carry the turn the CLI actually took"
        );
        assert_eq!(
            on_disk.seat.claude_session, talk.seat.claude_session,
            "the session id handed to the CLI must survive the failed reply"
        );
        assert_eq!(on_disk.seat.captured_session, talk.seat.captured_session);
        assert!(
            agent::has_session(AgentKind::Command, &on_disk.seat, cfg.graph.sessions),
            "the next turn must resume, not open the same session id twice"
        );
    }

    /// Even the note can fail to save, if the disk stays unwritable for long
    /// enough. `respond` must still report the failure rather than pretend
    /// the turn succeeded, and must not leave the in-memory `talk` claiming
    /// a turn that never reached disk.
    #[tokio::test]
    async fn a_write_failure_that_also_loses_the_note_still_reports_it() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("go ahead"));
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let text = record(&mut talk, &talks, "check the tests", Vec::new()).expect("record");
        // Enough forced failures to exhaust the retries for both the reply
        // and the note that would have replaced it.
        failpoint::force_put_failures(PUT_RETRIES * 2);
        let err = respond(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            &text,
        )
        .await
        .expect_err("neither the reply nor the note could be saved");
        assert!(err.to_string().contains("could not be saved"), "{err}");

        assert_eq!(talk.turns.len(), 1, "only the operator's own turn");
        let on_disk = talks.get(&talk.id).expect("get");
        assert_eq!(on_disk.turns.len(), 1);

        // Nothing at all reached disk, so the seat could not either: the CLI
        // took a turn the record does not know about. That is pinned here as
        // the known cost of a file that cannot be written twice over, not as
        // something this branch could do better - the only way to record the
        // seat is the write that just failed. It is also the point where
        // this meets `20260907-011805-fb57`: a turn taken before some later
        // write lands would re-open a session id the CLI already holds. The
        // in-memory seat keeps the truth the CLI reported, which is why it is
        // not wound back to match.
        assert_eq!(
            on_disk.seat.turns, 0,
            "an unwritable file cannot record the turn the CLI took"
        );
        assert_eq!(
            talk.seat.turns, 1,
            "the in-memory seat still reports the turn the CLI actually took"
        );
        assert_eq!(
            on_disk.seat.claude_session, talk.seat.claude_session,
            "the session id was minted at `begin` and never changes here"
        );
    }

    /// An attachment lets the operator send an otherwise-empty message, and
    /// its absolute path is what actually reaches the agent's prompt - here
    /// on the very first turn, where it has to share the briefing.
    #[tokio::test]
    async fn attachments_reach_the_prompt_and_an_empty_body_is_still_a_turn() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let att = talks
            .put_attachment(
                &talk.id,
                "image/png",
                "screenshot.png",
                b"pretend-png-bytes",
            )
            .expect("put attachment");

        say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "",
            vec![att.clone()],
        )
        .await
        .expect("an empty body with an attachment is still a turn");

        let operator_turn = &talk.turns[0];
        assert_eq!(operator_turn.who, Who::Operator);
        assert_eq!(operator_turn.body, "");
        assert_eq!(operator_turn.attachments, vec![att.clone()]);

        let prompt = &talk.turns[1].body;
        let expected_path = talks
            .attachments_dir(&talk.id)
            .join(format!("{}.png", att.id));
        assert!(
            prompt.contains(&expected_path.display().to_string()),
            "the agent must be told the attachment's absolute path: {prompt}"
        );
        assert!(prompt.contains("image/png"), "and its mime: {prompt}");
    }

    /// See `chat`'s test of the same name: `run::home()` returns a bare
    /// relative `PathBuf` verbatim when `MAGI_HOME` is set to a relative
    /// path, so a `Talks` store built on it has a relative `root` too. That
    /// is fine for this store's own I/O, which runs in this process against
    /// this process's cwd, but `attachment_path` hands its result to a
    /// *different* process invoked with `cwd: &talk.repo` - an uncorrected
    /// relative path would resolve against the repository instead of
    /// wherever the attachment actually landed.
    #[test]
    fn attachment_path_is_absolute_even_when_the_store_root_is_relative() {
        let talks = Talks::at(PathBuf::from("relative-talks-root-for-this-test"));
        let att = Attachment {
            id: "0".repeat(32),
            name: "shot.png".to_owned(),
            mime: "image/png".to_owned(),
            bytes: 3,
        };
        let path = talks
            .attachment_path("some-talk-id", &att)
            .expect("a supported mime always yields a path");
        assert!(
            path.is_absolute(),
            "must be absolute even off a relative store root: {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn a_turn_past_the_configured_talk_timeout_is_reported_with_that_timeout() {
        // `[graph] timeout_talk` must be the number this module actually
        // waits, not a leftover hardcoded fifteen minutes - so the mock
        // sleeps past a deliberately tiny override and the failure note is
        // checked against that same override, not the old default.
        let (tmp, talks) = store();
        let slow = mock_agent(
            tmp.path(),
            "#!/bin/sh\ncat >/dev/null\nsleep 2\n",
            BTreeMap::new(),
        );
        let mut cfg = config(slow);
        cfg.graph.timeout_talk = 1;
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let err = say(
            &talks.claim_turn(&talk.id).unwrap().unwrap(),
            &mut talk,
            &talks,
            &cfg,
            "check the tests",
            Vec::new(),
        )
        .await
        .expect_err("a turn that never answers is an error");
        assert!(
            err.to_string().contains("did not answer within 1s"),
            "{err}"
        );

        let on_disk = talks.get(&talk.id).expect("get");
        let note = on_disk.turns.last().expect("a note turn was recorded");
        assert!(
            note.body.contains("did not answer within 1s"),
            "the transcript must show the configured timeout: {}",
            note.body
        );
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

        let err =
            record(&mut talk, &talks, "still there?", Vec::new()).expect_err("closed talks refuse");
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
        respond(
            &talks.claim_turn(&in_flight.id).unwrap().unwrap(),
            &mut in_flight,
            &talks,
            &cfg,
            "one more question",
        )
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
        let err = record(&mut stale, &talks, "still there?", Vec::new())
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
        let held = talks.guard().unwrap();

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
    fn reopening_a_closed_talk_lets_it_take_turns_again_and_reopening_twice_is_not_an_error() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        close(&mut talk, &talks).expect("close");
        assert_eq!(talk.status, TalkStatus::Closed);

        reopen(&mut talk, &talks).expect("reopen");
        assert_eq!(talk.status, TalkStatus::Open);
        assert_eq!(
            talks.get(&talk.id).expect("reread").status,
            TalkStatus::Open
        );

        // Idempotent: reopening an already-open talk is not an error.
        reopen(&mut talk, &talks).expect("reopening an open talk is not an error");
        assert_eq!(talk.status, TalkStatus::Open);

        record(&mut talk, &talks, "one more thing", Vec::new())
            .expect("a reopened talk takes turns again");
        let _ = &cfg; // config kept only to build the agent above
    }

    #[test]
    fn removing_a_talk_deletes_its_record_and_artifacts_and_refuses_an_unknown_id() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let artifacts = talks.artifacts_of(&talk.id);
        std::fs::create_dir_all(&artifacts).expect("create artifacts dir");
        std::fs::write(artifacts.join("turn-1.txt"), "hello").expect("write artifact");

        talks.remove(&talk.id).expect("remove");
        assert!(!talks.path_of(&talk.id).is_file(), "the record is gone");
        assert!(!artifacts.is_dir(), "the artifacts directory is gone");
        assert!(
            talks.get(&talk.id).is_err(),
            "a removed talk cannot be read back"
        );

        let err = talks
            .remove("nonexistent-id")
            .expect_err("unknown id refused");
        assert!(err.to_string().contains("no talk matches"), "{err}");
        let _ = &cfg; // config kept only to build the agent above
    }

    #[tokio::test]
    async fn a_delete_that_lands_while_a_turn_is_in_flight_is_not_undone_by_the_reply() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("here you go"));
        let cfg = config(spec);
        // The in-flight turn's own handle, loaded before the delete lands -
        // the same shape as the matching close test above.
        let mut in_flight = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        talks.remove(&in_flight.id).expect("remove");
        assert!(
            talks.get(&in_flight.id).is_err(),
            "the delete landed on disk before the turn finished"
        );

        // The turn's own handle has no way to know the record is gone -
        // finishing it must not write the file back into existence.
        respond(
            &talks.claim_turn(&in_flight.id).unwrap().unwrap(),
            &mut in_flight,
            &talks,
            &cfg,
            "one more question",
        )
        .await
        .expect("the turn itself still completes rather than erroring");

        assert!(
            talks.get(&in_flight.id).is_err(),
            "a delete must stick even when a turn that started before it finishes after it"
        );
    }

    #[test]
    fn a_delete_that_lands_before_record_is_called_is_not_undone_by_it() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        // The handle `web::talk_say` would have read before awaiting config
        // discovery, then carried across that await into `record`.
        let mut stale = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        talks.remove(&stale.id).expect("remove");

        // The stale handle has no way to know the record is gone - a
        // `record` that trusted it would append a turn and write the
        // conversation back into existence.
        let err = record(&mut stale, &talks, "still there?", Vec::new())
            .expect_err("a delete that landed first must be honored, not overwritten");
        assert!(err.to_string().contains("deleted"), "{err}");

        assert!(
            talks.get(&stale.id).is_err(),
            "record must not resurrect a conversation deleted while its snapshot was stale"
        );
        let _ = &cfg; // config kept only to build the agent above
    }

    #[test]
    fn a_delete_that_lands_before_close_is_called_is_not_undone_by_it() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        // `web::talk_close` loads `talk` and calls `close` right after - this
        // stands in for a delete landing in that gap.
        let mut stale = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        talks.remove(&stale.id).expect("remove");

        // The stale handle has no way to know the record is gone - a `close`
        // that fell back to it would write the conversation back into
        // existence, closed.
        let err = close(&mut stale, &talks)
            .expect_err("a delete that landed first must be honored, not overwritten");
        assert!(err.to_string().contains("deleted"), "{err}");

        assert!(
            talks.get(&stale.id).is_err(),
            "close must not resurrect a conversation deleted while its snapshot was stale"
        );
        let _ = &cfg; // config kept only to build the agent above
    }

    #[test]
    fn a_delete_that_lands_before_reopen_is_called_is_not_undone_by_it() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        // `web::talk_reopen` loads `talk` and calls `reopen` right after -
        // this stands in for a delete landing in that gap.
        let mut stale = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");
        close(&mut stale, &talks).expect("close");

        talks.remove(&stale.id).expect("remove");

        // The stale handle has no way to know the record is gone - a
        // `reopen` that fell back to it would write the conversation back
        // into existence, open.
        let err = reopen(&mut stale, &talks)
            .expect_err("a delete that landed first must be honored, not overwritten");
        assert!(err.to_string().contains("deleted"), "{err}");

        assert!(
            talks.get(&stale.id).is_err(),
            "reopen must not resurrect a conversation deleted while its snapshot was stale"
        );
        let _ = &cfg; // config kept only to build the agent above
    }

    #[test]
    fn list_puts_open_talks_before_closed_ones() {
        let (tmp, talks) = store();
        let make = |id: &str, status: TalkStatus| {
            let mut t = Talk {
                pending_breaks: None,
                schema: SCHEMA,
                id: id.to_owned(),
                repo: tmp.path().to_owned(),
                agent: "mock".to_owned(),
                status,
                turns: Vec::new(),
                pending: String::new(),
                pending_attachments: Vec::new(),
                fallback: false,
                persona: String::new(),
                persona_dirty: false,
                implementers: 1,
                implementers_dirty: false,
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
    fn the_briefing_names_solo_task_add() {
        let brief = briefing(Path::new("/repo"), "en", false);
        assert!(brief.contains("magi task add --solo"));
        assert!(brief.contains("/repo"));
        assert!(!brief.contains("Hold this conversation in"));
    }

    /// Talk fixes `repo` at the directory the conversation was opened in, so
    /// an agent asked to change some other checkout has no path to it unless
    /// the briefing itself says `--repo` can take a short name - see
    /// `resolve_repo_by_name` in `src/main.rs`, which is what actually
    /// resolves it.
    #[test]
    fn the_briefing_explains_targeting_a_different_repository_by_name() {
        let brief = briefing(Path::new("/repo"), "en", false);
        assert!(brief.contains("--repo does not have to be a full path"));
        assert!(brief.contains("owner/repo"));
        assert!(brief.contains("magi repos"));
        assert!(brief.contains("ask the operator"));
    }

    #[test]
    fn the_briefing_tells_the_assistant_to_pass_images_with_attach() {
        let brief = briefing(Path::new("/repo"), "en", false);
        assert!(brief.contains("--attach <path>"), "{brief}");
        assert!(brief.contains("deleting this conversation"), "{brief}");
    }

    #[test]
    fn the_briefing_names_the_language_when_it_is_not_english() {
        let brief = briefing(Path::new("/repo"), "Japanese", false);
        assert!(brief.contains("Hold this conversation in Japanese"));
    }

    #[test]
    fn the_briefing_forbids_writes_unless_the_repository_opted_in() {
        let read_only = briefing(Path::new("/repo"), "en", false);
        assert!(read_only.contains("Do not write files"));
        assert!(!read_only.contains("allow_write"));

        let writable = briefing(Path::new("/repo"), "en", true);
        assert!(!writable.contains("Do not write files"));
        assert!(writable.contains("allow_write = true"));
        // Still names the queue for anything past a small named edit, and
        // still tells the agent to report what it changed.
        assert!(writable.contains("magi task add --solo"));
        assert!(writable.contains("say plainly what you"));
    }
}
