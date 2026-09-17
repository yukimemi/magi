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
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::ask;
use crate::config::Config;
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

/// The longest [`relay_conductor_question`] will wait for the chat's own
/// turn before giving up on it for this call.
///
/// A world away from [`turn_timeout`]'s hour-long budget for an ordinary
/// conversational turn - deliberately, because `relay_conductor_question`
/// itself runs *inside* `magi ask`, before that command ever reaches
/// `ask::ask_and_wait`'s own slicing. `ask::WAIT_SLICE`'s doc explains why no
/// `magi ask` call may run long: an agent CLI's shell tool kills a command
/// well before an hour is up, and the child it kills is `magi ask` itself -
/// the one thing that would have read the owner's answer. Spending minutes
/// here before `ask_and_wait` even starts its own four-minute slice would
/// reintroduce exactly that failure by a different door. Two minutes leaves
/// comfortable room under the shell tool's own limit once `ask_and_wait`'s
/// slice runs on top; a chat agent that has not answered by then is one this
/// call stops waiting on, not one it keeps blocking `magi ask` for - the
/// question falls back to the ordinary Queue panel and answer_timeout wait
/// exactly as if no relay had been attempted.
const RELAY_TIMEOUT: Duration = Duration::from_secs(120);

/// Who said something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    /// The operator.
    Operator,
    /// The conversation's agent - or magi itself, reporting that a turn
    /// failed. See [`MAGI_NOTE`].
    Agent,
    /// A question relayed in from a run's `magi ask` - see
    /// [`relay_conductor_question`]. Distinct from [`Who::Operator`] even
    /// though the run being asked is exactly the same shape of question a
    /// human answers, because the transcript must say plainly that this turn
    /// came from the graph rather than from the person on the other end of
    /// the phone.
    Conductor,
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

/// How long a talk's lock file may sit unclaimed-by-its-owner before
/// [`TalkGuard::acquire`] treats it as abandoned by a crashed process rather
/// than genuine contention, and breaks it.
///
/// The section every guard actually protects is a handful of small
/// filesystem calls - a read, a status check, a write - never the agent
/// invocation itself (see [`turn`]'s own doc on why its guard is taken only
/// for its tail), so anything held this long was left behind by a process
/// that died mid-guard, not one still working.
const TALK_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

/// How long [`TalkGuard::acquire`] retries before giving up entirely.
/// Comfortably longer than [`TALK_LOCK_STALE_AFTER`], so ordinary contention
/// between two processes racing the same conversation is always resolved by
/// the staleness check rather than by this call failing outright.
const TALK_LOCK_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to sleep between retries while spinning on a lock file held by a
/// still-live process. Short enough that the caller notices the moment the
/// lock is dropped; long enough not to hammer the filesystem while waiting.
const TALK_LOCK_POLL: Duration = Duration::from_millis(20);

/// Cross-process mutual exclusion for one conversation, held for the
/// duration of a read-modify-write cycle. See [`Talks::guard`] for why this
/// has to work across process boundaries rather than only within one.
struct TalkGuard {
    path: PathBuf,
}

impl TalkGuard {
    /// Block until `<root>/<id>.lock` can be created exclusively, or until
    /// [`TALK_LOCK_TIMEOUT`] has passed with no progress.
    ///
    /// `create_new` is the same atomic-on-every-target-platform primitive
    /// [`crate::queue::Queue::claim`] uses for the same reason: it is the one
    /// filesystem operation every process sharing this directory can use to
    /// agree on who goes first, with no coordination beyond the directory
    /// itself.
    fn acquire(root: &Path, id: &str) -> Result<Self> {
        std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let path = root.join(format!("{id}.lock"));
        let start = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Ok(age) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                        if age.elapsed().unwrap_or_default() > TALK_LOCK_STALE_AFTER {
                            // Whatever held this died before it could clean up
                            // after itself - a crash mid-guard, not a live
                            // competitor. `remove_file` racing another
                            // process's own break-and-retry is harmless: at
                            // most one of them wins the `create_new` just
                            // below, and the other simply loops again.
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                    }
                    if start.elapsed() > TALK_LOCK_TIMEOUT {
                        bail!(
                            "talk {id} is locked by another process ({}); giving up after {}s",
                            path.display(),
                            TALK_LOCK_TIMEOUT.as_secs()
                        );
                    }
                    std::thread::sleep(TALK_LOCK_POLL);
                }
                Err(e) => return Err(e).with_context(|| format!("lock {}", path.display())),
            }
        }
    }
}

impl Drop for TalkGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A conversation store on disk.
#[derive(Debug, Clone)]
pub struct Talks {
    root: PathBuf,
}

impl Talks {
    /// The operator's conversations, `<home>/talks`.
    pub fn open() -> Self {
        Self::at(crate::run::home().join("talks"))
    }

    /// A store at an explicit root. Tests use this, which is why none of them
    /// need the operator's real home.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Claim the right to read-modify-write one talk's record.
    ///
    /// Backed by an exclusively-created lock file, not an in-process mutex:
    /// `magi web` is no longer the only process that mutates a conversation.
    /// [`relay_conductor_question`] runs inside `magi ask`, a fresh OS
    /// process an agent's own shell tool spawns directly, with no channel
    /// back to whatever `magi web` process might be serving the same
    /// conversation to the operator's phone at that exact moment - an
    /// in-memory `Arc<Mutex<_>>` only ever serialized callers that shared
    /// its address space, which the two are not guaranteed to. A lock file
    /// under the store's own root is the one thing every process sharing
    /// this conversation can see. Scoped to one talk id, not the whole
    /// store, so two different conversations never contend for a lock
    /// neither of them needs.
    fn guard(&self, id: &str) -> Result<TalkGuard> {
        TalkGuard::acquire(&self.root, id)
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

    /// Every conversation on disk: open first, then newest first, so what the
    /// operator is still using belongs above what they are done with.
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

    /// Change detection token: the newest modification time in the store, in
    /// milliseconds.
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
        let resolved = self.resolve_id(id)?;
        let _guard = self.guard(&resolved)?;
        let path = self.path_of(&resolved);
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        let artifacts = self.artifacts_of(&resolved);
        if artifacts.is_dir() {
            std::fs::remove_dir_all(&artifacts)
                .with_context(|| format!("remove {}", artifacts.display()))?;
        }
        Ok(())
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
    // Absolute: a relative path means the wrong repository once anything
    // other than this process reads it back.
    let repo = repo.canonicalize().unwrap_or(repo);
    let want = agent.or(cfg.roles.chatter.as_deref());
    let spec = agent::pick(&cfg.agents, want, &agent::installed)?;

    let now = Timestamp::now();
    let mut talk = Talk {
        schema: SCHEMA,
        id: new_id(),
        repo,
        agent: spec.id.clone(),
        status: TalkStatus::Open,
        turns: Vec::new(),
        pending: String::new(),
        pending_attachments: Vec::new(),
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
    let _guard = store.guard(&talk.id)?;
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
    let _guard = store.guard(&talk.id)?;
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
        } else {
            fresh.pending.push_str("\n\n");
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
    let _guard = store.guard(&talk.id)?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if !fresh.status.open() || (fresh.pending.is_empty() && fresh.pending_attachments.is_empty()) {
        *talk = fresh;
        return Ok(None);
    }
    let text = std::mem::take(&mut fresh.pending);
    let attachments = std::mem::take(&mut fresh.pending_attachments);
    fresh.turns.push(Turn {
        who: Who::Operator,
        body: text.clone(),
        at: Timestamp::now(),
        attachments,
    });
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(Some(text))
}

/// One operator turn and one agent turn, appended - the synchronous form, used
/// by tests and by anything that is fine waiting out the turn itself.
pub async fn say(
    talk: &mut Talk,
    store: &Talks,
    cfg: &Config,
    text: &str,
    attachments: Vec<Attachment>,
) -> Result<()> {
    let text = record(talk, store, text, attachments)?;
    turn(talk, store, cfg, &text).await
}

/// The agent's half of a turn: invoke, append, flush. Pairs with [`record`].
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
///
/// If the re-read fails, this errors rather than falling back to the
/// caller's stale copy: `talk::begin` always `put`s the record before handing
/// out a `Talk`, so the only way a re-read can fail is a concurrent
/// [`Talks::remove`] having deleted it, and writing the stale copy back would
/// resurrect exactly what that delete removed.
pub fn close(talk: &mut Talk, store: &Talks) -> Result<()> {
    let _guard = store.guard(&talk.id)?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    fresh.status = TalkStatus::Closed;
    // A closed conversation must not replay a draft if it is reopened later.
    fresh.pending.clear();
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
    let _guard = store.guard(&talk.id)?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    fresh.status = TalkStatus::Open;
    store.put(&mut fresh)?;
    *talk = fresh;
    Ok(())
}

/// Discard the durable draft without adding a transcript turn.
pub fn clear_pending(talk: &mut Talk, store: &Talks) -> Result<()> {
    let _guard = store.guard(&talk.id)?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    fresh.pending.clear();
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
    let _guard = store.guard(&talk.id)?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if !pending_matches(&fresh, expected_text, expected_attachments) {
        *talk = fresh;
        return Ok(false);
    }
    fresh.pending.clear();
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
    let _guard = store.guard(&talk.id)?;
    let mut fresh = store
        .get(&talk.id)
        .with_context(|| format!("talk {} was deleted", talk.short()))?;
    if !pending_matches(&fresh, expected_text, expected_attachments) {
        *talk = fresh;
        return Ok(false);
    }
    fresh.pending = text.trim().to_owned();
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

    let resuming = agent::has_session(spec.kind, &talk.seat, cfg.graph.sessions);
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
    let body = if talk.seat.turns == 0 {
        format!(
            "{}\n\n# Operator\n\n{text}{last_note}",
            briefing(&talk.repo, &cfg.graph.language, cfg.talk.allow_write)
        )
    } else if resuming {
        format!("{text}{last_note}")
    } else {
        format!("{}\n\n{text}{last_note}", transcript(talk, store))
    };

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

    let artifacts = store.artifacts_of(&talk.id);
    let stem = format!("turn-{}", talk.seat.turns + 1);
    // The chat's build cache is the same shared one the graph's seats get, so
    // a conversation that compiles does not mint another multi-GB target dir.
    let cache_dir = cfg.cache_dir();
    let inv = Invocation {
        cwd: &talk.repo,
        prompt: &body,
        timeout: turn_timeout(cfg),
        // Off unless this repository's own config opts in - see
        // `crate::config::Talk::allow_write` and this module's doc for why
        // the default keeps a conversational edit from landing in a checkout
        // no run or review can claim.
        allow_write: cfg.talk.allow_write,
        sessions: cfg.graph.sessions,
        artifacts: &artifacts,
        stem: &stem,
        // The conversation's own id, so `magi task add` run from inside it is
        // attributed to this conversation - see `Source::Agent`.
        run: &talk.id,
        node: "chat",
        cache_dir: cache_dir.as_deref(),
        attachments: &attachment_paths,
    };

    let outcome = agent::invoke(spec, &mut talk.seat, &inv).await;
    let note = |why: String| Turn {
        who: Who::Agent,
        body: format!("{MAGI_NOTE}{why}"),
        at: Timestamp::now(),
        attachments: Vec::new(),
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
                who: Who::Agent,
                body: out.text.trim().to_owned(),
                at: Timestamp::now(),
                attachments: Vec::new(),
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
    let _guard = store.guard(&talk.id)?;
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
    talk.pending_attachments = fresh.pending_attachments;
    talk.turns.push(reply);
    store.put(talk)?;

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
            Who::Agent => "you",
            Who::Conductor => "conductor",
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
         magi task add --solo --repo {repo} <instruction>\n\n\
         and tell the operator the task id it prints, so they can follow it \
         from the Queue. Write <instruction> so that an implementer who has \
         never seen this conversation can act on it alone - it is everything \
         they get. Use --solo: it runs the task through one implementer \
         straight into review instead of the usual multi-agent competition, \
         which is the right shape for a change this conversation has already \
         settled, rather than one still worth several independent takes.\n\n\
         If the operator asks for something in a different repository, \
         --repo does not have to be a full path: --repo owner/repo (or just \
         repo, when that is unambiguous) is resolved against local checkouts \
         the same way `magi repos` lists them. If the command fails because \
         nothing matches or more than one checkout shares that name, ask the \
         operator which repository they mean (or run `magi repos` yourself \
         to see the candidates) rather than guessing.\n",
        repo = repo.display(),
    );
    out.push_str(&language_note(language));
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

/// If `run_id` was minted for a task the standing chat filed - a task whose
/// [`Source::Agent`] names `"chat"` as its node - return that conversation's
/// id. `None` covers every other case: a human-filed task, a task filed by
/// some other node (`implement`, `review`), or a `run_id` no task on disk
/// remembers at all - and is the ordinary case, since only a task born from
/// `magi task add --solo` run inside a live conversation ever matches.
///
/// `queue.list()` rather than a dedicated index: the queue is small enough
/// that [`tasks_of`] already scans it the same way for the same reason, and
/// this runs once per question, not once per poll.
fn chat_origin(queue: &Queue, run_id: &str) -> Option<String> {
    if run_id.is_empty() {
        return None;
    }
    let task = queue
        .list()
        .into_iter()
        .find(|t| t.runs.iter().any(|r| r == run_id))?;
    match task.source {
        Source::Agent { run, node } if node == "chat" => Some(run),
        _ => None,
    }
}

/// Append a conductor's question as a turn, the same way [`record`] appends
/// the operator's - so a call to [`turn`] right after this answers it exactly
/// as though the operator had asked. Not built on [`record`] itself: that
/// function trims to "nothing to say" and hands the text back for [`say`]'s
/// convenience, neither of which applies to a message this module built
/// itself and knows is never empty.
fn record_conductor(talk: &mut Talk, store: &Talks, text: &str) -> Result<()> {
    let _guard = store.guard(&talk.id)?;
    let Ok(fresh) = store.get(&talk.id) else {
        bail!("talk {} was deleted", talk.short());
    };
    talk.status = fresh.status;
    talk.pending = fresh.pending;
    talk.pending_attachments = fresh.pending_attachments;
    if !talk.status.open() {
        bail!(
            "talk {} is {} and takes no more turns",
            talk.short(),
            talk.status.as_str()
        );
    }
    talk.turns.push(Turn {
        who: Who::Conductor,
        body: text.to_owned(),
        at: Timestamp::now(),
        attachments: Vec::new(),
    });
    store.put(talk)?;
    Ok(())
}

/// The prompt sent to the chat's own agent when a conductor's question is
/// relayed into its conversation.
///
/// The `ANSWER:`/`ASK:` markers exist because [`apply_bridge`] has to act on
/// what comes back without a person reading it first: an agent that answers
/// in ordinary prose leaves the caller guessing whether it decided or was
/// only thinking out loud, and a wrong guess there would put words in the
/// operator's mouth. Asking for the marker in English even when the rest of
/// the reply is not is what keeps that parse independent of
/// `[graph] language`.
fn conductor_prompt(question: &ask::Question, message: &str) -> String {
    let mut out = String::from(
        "# Conductor\n\n\
         The implementer working on a task this conversation filed has \
         stopped and is asking something. Decide on the operator's behalf \
         if you are confident; otherwise say so and ask the operator back \
         instead of guessing - the run is waiting on this either way, and a \
         wrong guess is worse than a short delay.\n\n",
    );
    out.push_str(message.trim());
    out.push('\n');
    if !question.choices.is_empty() {
        out.push_str("\nChoices offered:\n");
        for c in &question.choices {
            out.push_str(&format!("- {c}\n"));
        }
    }
    out.push_str(
        "\nStart your reply with exactly one of the two literal markers \
         below, in English even if the rest of your reply is not - this is \
         read by a program, not a person:\n\n\
         - `ANSWER: <text>` - resolves the question. If choices were \
         offered, `<text>` must be one of them, verbatim; otherwise it is \
         free text.\n\
         - `ASK: <text>` - use this only when you are not confident. It \
         sends `<text>` back to the implementer instead of deciding, and \
         the implementer will follow up here once it has more to say.\n",
    );
    out
}

/// Which of the two markers [`conductor_prompt`] asked for came back.
enum BridgeTag {
    Answer,
    Ask,
}

/// Case-insensitive `line.strip_prefix(tag)`. Safe to slice `line` at
/// `tag.len()` on a case-insensitive match because every tag here is pure
/// ASCII, and ASCII case-folding never changes byte length.
fn strip_tag<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    // `get`, not slicing, because `line` is arbitrary model output that may
    // not even be ASCII - a byte offset that lands mid-character would panic
    // rather than simply fail to match.
    let head = line.get(..tag.len())?;
    head.eq_ignore_ascii_case(tag).then(|| &line[tag.len()..])
}

/// Pull a marker and its text out of the chat agent's reply. `None` means
/// neither marker opened the first line - an agent that ignored the format
/// [`conductor_prompt`] asked for - and [`apply_bridge`] leaves the question
/// exactly as it found it in that case, falling back to the Queue panel and
/// the operator's own hands.
fn split_bridge_tag(reply: &str) -> Option<(BridgeTag, String)> {
    let trimmed = reply.trim();
    let mut lines = trimmed.lines();
    let first = lines.next()?.trim();
    let (tag, first_rest) = strip_tag(first, "ANSWER:")
        .map(|rest| (BridgeTag::Answer, rest))
        .or_else(|| strip_tag(first, "ASK:").map(|rest| (BridgeTag::Ask, rest)))?;
    let mut text = first_rest.trim().to_owned();
    let remainder = lines.collect::<Vec<_>>().join("\n");
    let remainder = remainder.trim();
    if !remainder.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(remainder);
    }
    Some((tag, text))
}

/// Strip one layer of matching quotes or backticks - the shape a model tends
/// to wrap a quoted choice in - before comparing it against the choices a
/// question actually offered.
fn strip_wrapping(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\'', '`'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Turn the chat agent's structured reply into a resolution of `question`,
/// exactly as `magi answer` would if the operator had typed it - `ANSWER:`
/// becomes [`ask::Question::answer`], `ASK:` becomes [`ask::Question::say`],
/// the owner's own way of asking back rather than deciding (see that
/// function's doc for why this module is allowed to stand in for the owner
/// here at all).
///
/// Does nothing - not an error - when the reply carries neither marker, is a
/// [`MAGI_NOTE`] failure note, names a choice the question never offered, or
/// the question was answered by someone else in the meantime. Every one of
/// those falls back to the question sitting open for the operator to settle
/// by hand, through the Queue panel this call never touches.
fn apply_bridge(questions: &ask::Questions, question: &ask::Question, reply: &str) -> Result<()> {
    if reply.trim_start().starts_with(MAGI_NOTE) {
        return Ok(());
    }
    let Some((tag, text)) = split_bridge_tag(reply) else {
        return Ok(());
    };
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut fresh = questions.get(&question.id)?;
    if !fresh.status.open() {
        return Ok(());
    }
    match tag {
        BridgeTag::Answer => {
            let answer = if fresh.free_text() {
                ask::Answer::Text(text)
            } else {
                // A choice is one of the question's own strings, verbatim -
                // never more than that. Matching against the whole of `text`
                // would refuse an otherwise-confident answer the moment the
                // model adds so much as a reason on the next line, e.g.
                // `ANSWER: Redis\n\nWe already depend on it.`; only the
                // marker line itself is ever a choice, so only it is
                // compared, and the rest is simply not carried into the
                // recorded answer.
                let first_line = text.lines().next().unwrap_or("").trim();
                let Some(choice) = fresh
                    .choices
                    .iter()
                    .find(|c| c.eq_ignore_ascii_case(strip_wrapping(first_line)))
                else {
                    return Ok(());
                };
                ask::Answer::Choice(choice.clone())
            };
            fresh.answer(answer)?;
        }
        BridgeTag::Ask => fresh.say(text)?,
    }
    questions.put(&mut fresh)
}

/// If the run behind `question` was started for a task the standing chat
/// filed, forward `message` into that conversation as a turn from the
/// conductor, trigger the chat agent's own turn to answer it exactly as
/// though the operator had spoken, and fold whatever it decided back into
/// `question` - see [`apply_bridge`]. `question` is always left holding the
/// current truth on disk when this returns, whatever happened above: a
/// caller that went on to `put` a stale copy over a real answer - the chat's
/// or a human's - would throw the answer away.
///
/// `cfg` is the caller's own config for the run behind `question`, reused
/// rather than rediscovered from `talk.repo`: the two name the same
/// repository in the shape this module expects a chat-filed task to take
/// (`magi task add --solo --repo {repo}` inside [`briefing`], `{repo}`
/// being the conversation's own), and rediscovering would cost a second
/// filesystem walk for a config that has to come out identical.
///
/// Ordinary and silent for every task not filed from a chat: [`chat_origin`]
/// finds nothing, and the caller's own `Question::answer`/`Question::say`
/// path proceeds exactly as it always has, notification and Queue panel
/// included - this is additive, never a replacement for either.
///
/// Meant to be called again for the same still-open question - `ask_wait_cmd`
/// does exactly that on every `magi ask --wait` - because [`RELAY_TIMEOUT`]
/// bounds any single attempt well under a chat turn's own hour-long budget,
/// and a chat that has not answered yet by then has not necessarily failed,
/// only not finished. A repeat call for the same question and message does
/// not duplicate the conductor's turn in the transcript; see the check
/// against the talk's last turn inside [`relay_conductor_question_inner`].
pub async fn relay_conductor_question(
    queue: &Queue,
    talks: &Talks,
    cfg: &Config,
    questions: &ask::Questions,
    question: &mut ask::Question,
    message: &str,
) -> Result<()> {
    let result = relay_conductor_question_inner(
        queue,
        talks,
        cfg,
        questions,
        question,
        message,
        RELAY_TIMEOUT,
    )
    .await;
    if let Ok(fresh) = questions.get(&question.id) {
        *question = fresh;
    }
    result
}

/// [`relay_conductor_question`] with the timeout injected, the same way
/// `ask::wait_for_owner` injects its own poll interval: production has
/// exactly one value ([`RELAY_TIMEOUT`]), and this is what lets a test drive
/// the timeout path in milliseconds instead of actually waiting two minutes.
async fn relay_conductor_question_inner(
    queue: &Queue,
    talks: &Talks,
    cfg: &Config,
    questions: &ask::Questions,
    question: &ask::Question,
    message: &str,
    timeout: Duration,
) -> Result<()> {
    let Some(talk_id) = chat_origin(queue, &question.run) else {
        return Ok(());
    };
    let mut talk = talks.get(&talk_id)?;
    if !talk.status.open() {
        return Ok(());
    }

    let prompt = conductor_prompt(question, message);
    // `magi ask --wait` retries this same relay from a fresh process every
    // time its own slice runs out with no answer yet - see the doc on the
    // call in `ask_wait_cmd` - so a prompt identical to the talk's own last
    // turn means an earlier attempt already recorded this exact question and
    // was cut off (by `RELAY_TIMEOUT` below) before the chat replied. Record
    // it again and the transcript gains a duplicate conductor turn every few
    // minutes for as long as the chat keeps thinking; skip straight to
    // giving it another turn instead.
    let already_recorded =
        matches!(talk.turns.last(), Some(t) if t.who == Who::Conductor && t.body == prompt);
    if !already_recorded {
        record_conductor(&mut talk, talks, &prompt)?;
    }
    // Bounded well under `magi ask`'s own safety margin - see
    // [`RELAY_TIMEOUT`] - rather than `respond`'s own hour-long
    // `turn_timeout`: this call has to return to `ask_cmd` in time for
    // `ask::ask_and_wait` to still get its own slice out of the same shell
    // tool budget. `respond`'s underlying agent invocation is killed on drop
    // (see `agent::build_command`'s `kill_on_drop`), so a timeout here
    // leaves no orphaned process behind - only a conductor turn in the
    // transcript with no reply yet, exactly as if the operator's own
    // message had not been answered yet either.
    tokio::time::timeout(timeout, respond(&mut talk, talks, cfg, &prompt))
        .await
        .with_context(|| {
            format!(
                "chat did not answer within {}s; falling back to the Queue panel",
                timeout.as_secs()
            )
        })??;

    let Some(reply) = talk.turns.last() else {
        return Ok(());
    };
    if reply.who != Who::Agent {
        return Ok(());
    }
    apply_bridge(questions, question, &reply.body)
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
            pending: String::new(),
            pending_attachments: Vec::new(),
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
        cfg.roles.chatter = Some(chatter_spec.id.clone());

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

    #[test]
    fn queued_text_is_durable_combined_and_drained_as_one_operator_turn() {
        let (tmp, talks) = store();
        let cfg = config(mock_agent(tmp.path(), REPLY, env("reply")));
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        queue(&mut talk, &talks, "first", Vec::new()).expect("queue first");
        queue(&mut talk, &talks, "second", Vec::new()).expect("queue second");
        let saved = talks.get(&talk.id).expect("reload queued talk");
        assert_eq!(saved.pending, "first\n\nsecond");
        assert!(saved.turns.is_empty(), "a draft is not a transcript turn");

        let drained = drain(&mut talk, &talks).expect("drain");
        assert_eq!(drained.as_deref(), Some("first\n\nsecond"));
        let saved = talks.get(&talk.id).expect("reload drained talk");
        assert!(saved.pending.is_empty());
        assert_eq!(saved.turns.len(), 1);
        assert_eq!(saved.turns[0].body, "first\n\nsecond");
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
            respond(&mut running, &response_talks, &response_cfg, &first).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let mut queued = talks.get(&id).expect("queued handle");
        queue(&mut queued, &talks, "next", Vec::new()).expect("queue");
        reply.await.expect("join").expect("reply");

        let saved = talks.get(&id).expect("reload");
        assert_eq!(saved.pending, "next");
        assert_eq!(saved.turns.len(), 2, "operator message and reply remain");
    }

    #[tokio::test]
    async fn the_first_turn_carries_the_briefing_and_later_turns_do_not() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let cfg = config(spec);
        let mut talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        say(
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

        say(&mut talk, &talks, &cfg, "and how is it locked?", Vec::new())
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

        say(
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

        let err = say(&mut talk, &talks, &cfg, "check the tests", Vec::new())
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

        say(&mut talk, &talks, &cfg, "", vec![att.clone()])
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

        let err = say(&mut talk, &talks, &cfg, "check the tests", Vec::new())
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
        let held = talks.guard(&talk.id).expect("acquire");

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
        respond(&mut in_flight, &talks, &cfg, "one more question")
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
                schema: SCHEMA,
                id: id.to_owned(),
                repo: tmp.path().to_owned(),
                agent: "mock".to_owned(),
                status,
                turns: Vec::new(),
                pending: String::new(),
                pending_attachments: Vec::new(),
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

    #[test]
    fn chat_origin_finds_the_talk_that_filed_the_task_and_nothing_else() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let queue = Queue::at(tmp.path().join("queue"));

        let mut from_chat = Task::new(
            "cache backend".to_owned(),
            "cache backend".to_owned(),
            tmp.path().to_owned(),
            Source::Agent {
                run: "20260904-014455-ab12".to_owned(),
                node: "chat".to_owned(),
            },
        );
        from_chat.runs.push("20260904-090000-r001".to_owned());
        queue.put(&mut from_chat).expect("put from_chat");

        let mut from_run = Task::new(
            "unrelated".to_owned(),
            "unrelated".to_owned(),
            tmp.path().to_owned(),
            Source::Agent {
                run: "20260904-090000-zz99".to_owned(),
                node: "implement".to_owned(),
            },
        );
        from_run.runs.push("20260904-090000-r002".to_owned());
        queue.put(&mut from_run).expect("put from_run");

        assert_eq!(
            chat_origin(&queue, "20260904-090000-r001"),
            Some("20260904-014455-ab12".to_owned())
        );
        assert_eq!(
            chat_origin(&queue, "20260904-090000-r002"),
            None,
            "filed by `implement`, not `chat`"
        );
        assert_eq!(chat_origin(&queue, "no-such-run"), None);
        assert_eq!(chat_origin(&queue, ""), None);
    }

    #[test]
    fn split_bridge_tag_reads_the_marker_case_insensitively() {
        let (tag, text) = split_bridge_tag("answer: Redis").expect("tag");
        assert!(matches!(tag, BridgeTag::Answer));
        assert_eq!(text, "Redis");

        let (tag, text) = split_bridge_tag("ASK: not sure").expect("tag");
        assert!(matches!(tag, BridgeTag::Ask));
        assert_eq!(text, "not sure");

        assert!(
            split_bridge_tag("Redis seems right").is_none(),
            "prose with neither marker must not be read as a decision"
        );
    }

    #[test]
    fn split_bridge_tag_keeps_lines_after_the_marker() {
        let (tag, text) =
            split_bridge_tag("ANSWER: Redis\n\nWe already depend on it.").expect("tag");
        assert!(matches!(tag, BridgeTag::Answer));
        assert_eq!(text, "Redis\nWe already depend on it.");
    }

    fn choice_question(choices: Vec<String>) -> ask::Question {
        ask::Question::new(
            "20260904-090000-r001".to_owned(),
            "implement".to_owned(),
            "impl-A".to_owned(),
            "Which storage backend should the cache use?".to_owned(),
            String::new(),
            choices,
        )
    }

    #[test]
    fn apply_bridge_answers_a_matching_choice_case_insensitively() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let mut q = choice_question(vec!["SQLite".to_owned(), "Redis".to_owned()]);
        questions.put(&mut q).expect("file question");

        apply_bridge(&questions, &q, "ANSWER: redis").expect("apply");

        let after = questions.get(&q.id).expect("reload");
        assert_eq!(
            after.resolution().as_deref(),
            Some("Redis"),
            "stored verbatim from the question's own choices, not the model's casing"
        );
    }

    #[test]
    fn apply_bridge_matches_a_choice_even_with_a_reason_on_the_next_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let mut q = choice_question(vec!["SQLite".to_owned(), "Redis".to_owned()]);
        questions.put(&mut q).expect("file question");

        apply_bridge(
            &questions,
            &q,
            "ANSWER: Redis\n\nWe already depend on it elsewhere.",
        )
        .expect("apply");

        let after = questions.get(&q.id).expect("reload");
        assert_eq!(
            after.resolution().as_deref(),
            Some("Redis"),
            "an explanation on the following line must not break the choice match"
        );
    }

    #[test]
    fn apply_bridge_ignores_a_choice_the_question_never_offered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let mut q = choice_question(vec!["SQLite".to_owned(), "Redis".to_owned()]);
        questions.put(&mut q).expect("file question");

        apply_bridge(&questions, &q, "ANSWER: Postgres").expect("apply is a no-op, not an error");

        let after = questions.get(&q.id).expect("reload");
        assert!(
            after.status.open(),
            "an unrecognised choice must not be guessed into an answer"
        );
    }

    #[test]
    fn apply_bridge_turns_ask_into_a_say() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let mut q = choice_question(Vec::new());
        questions.put(&mut q).expect("file question");

        apply_bridge(&questions, &q, "ASK: what does the operator prefer?").expect("apply");

        let after = questions.get(&q.id).expect("reload");
        assert!(after.status.open());
        assert!(after.waiting_on_agent());
        assert_eq!(
            after.thread.last().expect("a turn was appended").body,
            "what does the operator prefer?"
        );
    }

    #[test]
    fn apply_bridge_ignores_a_magi_note_and_unmarked_prose() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let mut q = choice_question(Vec::new());
        questions.put(&mut q).expect("file question");

        apply_bridge(
            &questions,
            &q,
            &format!("{MAGI_NOTE}could not run agent `mock`: boom"),
        )
        .expect("apply");
        apply_bridge(&questions, &q, "I think Redis, but let me check").expect("apply");

        let after = questions.get(&q.id).expect("reload");
        assert!(after.status.open());
        assert!(
            after.thread.is_empty(),
            "neither a failure note nor unmarked prose is a decision"
        );
    }

    #[tokio::test]
    async fn relay_conductor_question_resolves_a_confident_answer() {
        let (tmp, talks) = store();
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let queue = Queue::at(tmp.path().join("queue"));
        let spec = mock_agent(tmp.path(), REPLY, env("ANSWER: Redis"));
        let cfg = config(spec);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let run_id = "20260904-090000-r001".to_owned();
        let mut task = Task::new(
            "cache backend".to_owned(),
            "cache backend".to_owned(),
            tmp.path().to_owned(),
            Source::Agent {
                run: talk.id.clone(),
                node: "chat".to_owned(),
            },
        );
        task.runs.push(run_id.clone());
        queue.put(&mut task).expect("put task");

        let mut question = ask::Question::new(
            run_id,
            "implement".to_owned(),
            "impl-A".to_owned(),
            "Which storage backend should the cache use?".to_owned(),
            String::new(),
            vec!["SQLite".to_owned(), "Redis".to_owned()],
        );
        questions.put(&mut question).expect("file question");

        relay_conductor_question(
            &queue,
            &talks,
            &cfg,
            &questions,
            &mut question,
            "Which storage backend should the cache use?",
        )
        .await
        .expect("relay");

        assert_eq!(question.resolution().as_deref(), Some("Redis"));

        let after = talks.get(&talk.id).expect("reload talk");
        assert_eq!(after.turns.len(), 2, "the question and the chat's answer");
        assert_eq!(after.turns[0].who, Who::Conductor);
        assert!(after.turns[0].body.contains("storage backend"));
        assert_eq!(after.turns[1].who, Who::Agent);
        assert_eq!(after.turns[1].body, "ANSWER: Redis");
    }

    #[tokio::test]
    async fn relay_conductor_question_relays_an_unsure_reply_as_a_say() {
        let (tmp, talks) = store();
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let queue = Queue::at(tmp.path().join("queue"));
        let spec = mock_agent(
            tmp.path(),
            REPLY,
            env("ASK: not sure, please check with the operator"),
        );
        let cfg = config(spec);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let run_id = "20260904-090000-r003".to_owned();
        let mut task = Task::new(
            "cache backend".to_owned(),
            "cache backend".to_owned(),
            tmp.path().to_owned(),
            Source::Agent {
                run: talk.id.clone(),
                node: "chat".to_owned(),
            },
        );
        task.runs.push(run_id.clone());
        queue.put(&mut task).expect("put task");

        let mut question = choice_question(vec!["SQLite".to_owned(), "Redis".to_owned()]);
        question.run = run_id;
        questions.put(&mut question).expect("file question");

        relay_conductor_question(
            &queue,
            &talks,
            &cfg,
            &questions,
            &mut question,
            "Which storage backend should the cache use?",
        )
        .await
        .expect("relay");

        assert!(question.status.open());
        assert!(question.waiting_on_agent());
        assert_eq!(
            question.thread.last().expect("a turn was appended").body,
            "not sure, please check with the operator"
        );
    }

    #[tokio::test]
    async fn relay_conductor_question_is_a_no_op_for_a_task_not_filed_from_chat() {
        let (tmp, talks) = store();
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let queue = Queue::at(tmp.path().join("queue"));
        // A script that would fail loudly if it were ever run: a task not
        // filed from a chat must never reach the talk's own agent at all.
        let spec = mock_agent(tmp.path(), BROKEN, BTreeMap::new());
        let cfg = config(spec);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let run_id = "20260904-090000-r004".to_owned();
        let mut task = Task::new(
            "typed by hand".to_owned(),
            "typed by hand".to_owned(),
            tmp.path().to_owned(),
            Source::Human,
        );
        task.runs.push(run_id.clone());
        queue.put(&mut task).expect("put task");

        let mut question = choice_question(Vec::new());
        question.run = run_id;
        questions.put(&mut question).expect("file question");

        relay_conductor_question(
            &queue,
            &talks,
            &cfg,
            &questions,
            &mut question,
            "what should the error message say?",
        )
        .await
        .expect("relay is a no-op, not an error");

        assert!(question.status.open());
        assert!(question.thread.is_empty());
        let after = talks.get(&talk.id).expect("reload talk");
        assert!(
            after.turns.is_empty(),
            "nothing should have been said into an unrelated talk"
        );
    }

    /// `relay_conductor_question` runs *inside* `magi ask`, before that
    /// command ever reaches `ask::ask_and_wait`'s own slicing - see
    /// `RELAY_TIMEOUT`'s doc. A chat agent slower than the injected timeout
    /// must not be waited out: the call gives up, the conductor's question is
    /// left in the transcript with no reply, and the question itself is
    /// untouched, so `ask_cmd` can fall back to its ordinary wait exactly as
    /// if no relay had been attempted.
    #[tokio::test]
    async fn relay_conductor_question_gives_up_on_a_slow_chat_rather_than_blocking_ask() {
        let (tmp, talks) = store();
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let queue = Queue::at(tmp.path().join("queue"));
        let slow = mock_agent(
            tmp.path(),
            "#!/bin/sh\ncat >/dev/null\nsleep 2\nprintf 'ANSWER: Redis'\n",
            BTreeMap::new(),
        );
        let cfg = config(slow);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let run_id = "20260904-090000-r005".to_owned();
        let mut task = Task::new(
            "cache backend".to_owned(),
            "cache backend".to_owned(),
            tmp.path().to_owned(),
            Source::Agent {
                run: talk.id.clone(),
                node: "chat".to_owned(),
            },
        );
        task.runs.push(run_id.clone());
        queue.put(&mut task).expect("put task");

        let mut question = choice_question(vec!["SQLite".to_owned(), "Redis".to_owned()]);
        question.run = run_id;
        questions.put(&mut question).expect("file question");

        let err = relay_conductor_question_inner(
            &queue,
            &talks,
            &cfg,
            &questions,
            &question,
            "Which storage backend should the cache use?",
            Duration::from_millis(100),
        )
        .await
        .expect_err("a chat slower than the injected timeout must not be waited out");
        assert!(err.to_string().contains("did not answer within"), "{err}");

        assert!(
            question.status.open(),
            "the timeout must not touch the question at all"
        );
        let after = talks.get(&talk.id).expect("reload talk");
        assert_eq!(
            after.turns.len(),
            1,
            "the conductor's question is recorded, but no reply arrived in time: {:?}",
            after.turns
        );
        assert_eq!(after.turns[0].who, Who::Conductor);
    }

    /// `magi ask --wait` retries the same relay from a fresh process every
    /// time its own slice runs out - see the call in `ask_wait_cmd` - and
    /// must not pile up a fresh conductor turn in the transcript on every
    /// one of those retries, only ever the one turn the chat is actually
    /// meant to answer.
    #[tokio::test]
    async fn a_retried_relay_does_not_duplicate_the_conductor_turn_and_can_still_succeed() {
        let (tmp, talks) = store();
        let questions = ask::Questions::at(tmp.path().join("questions"));
        let queue = Queue::at(tmp.path().join("queue"));
        let slow = mock_agent(
            tmp.path(),
            "#!/bin/sh\ncat >/dev/null\nsleep 0.2\nprintf 'ANSWER: Redis'\n",
            BTreeMap::new(),
        );
        let cfg = config(slow);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let run_id = "20260904-090000-r006".to_owned();
        let mut task = Task::new(
            "cache backend".to_owned(),
            "cache backend".to_owned(),
            tmp.path().to_owned(),
            Source::Agent {
                run: talk.id.clone(),
                node: "chat".to_owned(),
            },
        );
        task.runs.push(run_id.clone());
        queue.put(&mut task).expect("put task");

        let mut question = choice_question(vec!["SQLite".to_owned(), "Redis".to_owned()]);
        question.run = run_id;
        questions.put(&mut question).expect("file question");

        let message = "Which storage backend should the cache use?";

        // The first attempt is cut off before the (identical, every time)
        // mock agent finishes - standing in for a chat that needed longer
        // than `RELAY_TIMEOUT` allowed.
        relay_conductor_question_inner(
            &queue,
            &talks,
            &cfg,
            &questions,
            &question,
            message,
            Duration::from_millis(20),
        )
        .await
        .expect_err("first attempt times out");
        assert_eq!(talks.get(&talk.id).expect("reload").turns.len(), 1);

        // `magi ask --wait` calling this again with the exact same message -
        // nothing new was said - must not add a second conductor turn, and
        // this time the agent gets long enough to actually answer.
        relay_conductor_question_inner(
            &queue,
            &talks,
            &cfg,
            &questions,
            &question,
            message,
            Duration::from_secs(5),
        )
        .await
        .expect("second attempt succeeds");

        let after = talks.get(&talk.id).expect("reload talk");
        assert_eq!(
            after.turns.len(),
            2,
            "one conductor turn, one reply - not a duplicated conductor turn: {:?}",
            after.turns
        );
        assert_eq!(after.turns[0].who, Who::Conductor);
        assert_eq!(after.turns[1].who, Who::Agent);

        let resolved = questions.get(&question.id).expect("reload question");
        assert_eq!(resolved.resolution().as_deref(), Some("Redis"));
    }

    /// [`Talks::guard`] has to serialize a read-modify-write cycle across
    /// independent processes, not only within one - `relay_conductor_question`
    /// runs inside a freshly spawned `magi ask`, which shares no address
    /// space with whatever `magi web` process might be serving the same
    /// conversation. Two entirely separate `Talks` values built with
    /// `Talks::at` (never `.clone`d from one another) stand in for that: if
    /// the guard were still the old in-process `Arc<Mutex<_>>`, this would
    /// not block at all, since each `Talks` would own its own private lock.
    #[test]
    fn the_guard_serializes_two_independently_constructed_stores_over_the_same_root() {
        let (tmp, talks) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("hi"));
        let cfg = config(spec);
        let talk = begin(&talks, &cfg, tmp.path().to_owned(), None).expect("begin");

        let root = talks.root().to_owned();
        let process_a = Talks::at(root.clone());
        let process_b = Talks::at(root);

        let held = process_a.guard(&talk.id).expect("acquire");

        let id = talk.id.clone();
        let closing = std::thread::spawn(move || {
            let mut talk = process_b.get(&id).expect("get");
            close(&mut talk, &process_b).expect("close");
        });

        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !closing.is_finished(),
            "a second, independently-constructed `Talks` must still wait for the \
             lock file, not race the first one's read-then-write"
        );

        drop(held);
        closing.join().expect("close thread panicked");

        assert_eq!(
            talks.get(&talk.id).expect("reread").status,
            TalkStatus::Closed,
            "once the lock file is free, close still lands"
        );
        let _ = &cfg; // config kept only to build the agent above
    }
}
