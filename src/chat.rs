//! The browser interview: `magi plan` for somebody holding a phone.
//!
//! [`crate::plan`] is an interview that works by *handing over the terminal* -
//! stdin, stdout and stderr inherited, the agent's own UI in front of the
//! operator, no timeout. That is the right design and it is not changing. It is
//! also unavailable to the operator who is away from the machine, which is most
//! of the time this repository's operator wants to plan something: there is no
//! terminal in a browser to hand over.
//!
//! So this module is the same interview, arrived at from the other side. magi
//! does host the conversation here, because there is nothing else that can:
//! each operator message is one *headless* [`crate::agent::invoke`], and the
//! transcript lives in a JSON file the phone reads. The end state is identical
//! to `magi plan`'s - a task file checked by [`plan::review_draft`] and filed
//! in [`crate::queue`] - which is deliberate. Two planning paths that accept
//! different task files would be two products.
//!
//! # A turn is cheap because the CLI remembers
//!
//! The thing that makes a turn-per-request affordable is [`SeatState`]: a
//! second [`crate::agent::invoke`] with the same seat resumes the CLI's own
//! conversation (`claude --resume`, `opencode run -s`, `agy --conversation`),
//! so a turn sends the operator's new sentence and nothing else. The model
//! already has the repository it read and the questions it asked. magi does
//! *not* re-send the transcript when the CLI can resume - that would pay for
//! the whole conversation again on every message, and it would let magi's idea
//! of the history drift from the model's. [`transcript`] exists only for the
//! case where resuming is genuinely impossible, and [`turn`] says when.
//!
//! # Shape
//!
//! The same split as [`crate::queue`] and [`crate::ask`]: [`Chat`] is data plus
//! pure helpers, [`Chats`] owns all I/O and is constructed with its root, so
//! every test below drives a real store in a temp directory and none of them
//! touch the operator's home. One conversation is one JSON file, written
//! atomically, because `magi web` and a future `magi chat` are separate
//! processes and a rename is the only cross-process atomic write that needs no
//! coordination between them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::config::Config;
use crate::plan;
use crate::queue::{self, Queue, Source, Task};

/// On-disk format for a conversation. Bumped when a field's meaning changes.
///
/// The web UI is written against this shape by hand, so a field that changes
/// meaning without a bump here is a front end that lies silently.
pub const SCHEMA: u32 = 1;

/// Wall-clock limit for one agent turn.
///
/// Five minutes, and the number is borrowed rather than invented: it is `agy`'s
/// own default `--print-timeout`, the one place a CLI vendor has published an
/// opinion about how long a single non-interactive answer should take. It fits
/// what a turn actually is - read a few files, ask one question - and it is far
/// below an implementation node's budget, which is correct: nobody is watching
/// an implementer, whereas here an operator is holding a phone with a spinner
/// on it. A turn that has not answered in five minutes is a wedged CLI, not a
/// thinking one, and the operator needs to be told that while they are still
/// looking at the screen.
const TURN_TIMEOUT: Duration = Duration::from_secs(300);

/// Seat name for the interviewing agent.
///
/// One seat per conversation, so the CLI-side conversation is scoped to this
/// chat and nothing else - the same rule [`crate::agent`] applies to judges.
const SEAT: &str = "plan";

/// Prefix on an agent turn that magi wrote rather than an agent.
///
/// A failed turn has to be *visible*, and the transcript is the only surface
/// the phone renders, so the failure goes in as an agent turn carrying this
/// marker. Two turn authors is what the wire shape allows (`operator` /
/// `agent`), and inventing a third would break every client written against
/// it; a stable prefix the UI can key on costs nothing and loses no
/// information.
pub const MAGI_NOTE: &str = "magi: ";

/// Who said something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    /// The person magi is planning for.
    Operator,
    /// The interviewing agent - or magi itself, reporting that the agent
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

/// Where a conversation is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatStatus {
    /// Still being talked through.
    Open,
    /// A task was filed from its draft.
    Filed,
    /// Given up on. Kept on disk, because an abandoned interview is still the
    /// record of a decision the operator made.
    Abandoned,
}

impl ChatStatus {
    /// Is this conversation still live?
    pub fn open(self) -> bool {
        matches!(self, Self::Open)
    }

    /// Wire form, for the phone and for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Filed => "filed",
            Self::Abandoned => "abandoned",
        }
    }
}

/// One planning conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chat {
    /// On-disk format version.
    pub schema: u32,
    /// Conversation id, e.g. `20260903-014455-ab12`.
    pub id: String,
    /// Repository the task will be filed against.
    pub repo: PathBuf,
    /// The chat this one was derived from, when it began as a fork into a
    /// different repository. See [`derived_background`]. `#[serde(default)]`
    /// so a conversation recorded before this field existed still reads.
    #[serde(default)]
    pub from: Option<String>,
    /// Roster agent id doing the interviewing.
    pub agent: String,
    /// Current state.
    pub status: ChatStatus,
    /// Everything said, oldest first.
    pub turns: Vec<Turn>,
    /// The task file, once the agent has written one.
    pub draft: Option<String>,
    /// Queue task id, once filed.
    pub task: Option<String>,
    /// When the conversation was opened.
    pub created_at: Timestamp,
    /// Last change to this file.
    pub updated_at: Timestamp,
    /// The CLI-side conversation, which is what makes turn N+1 cost one
    /// sentence instead of the whole transcript.
    ///
    /// Not `pub`: it is magi's bookkeeping, not part of the interview, and a
    /// caller that edited it would silently detach the record from the
    /// conversation the model is actually holding. It is still serialized,
    /// because a chat that survives a restart without its session id resumes
    /// nothing.
    seat: SeatState,
}

impl Chat {
    /// Short form used in lists and notifications, matching a run's short id.
    pub fn short(&self) -> &str {
        short(&self.id)
    }

    /// How many turns the interviewing agent has actually taken.
    ///
    /// Read off the seat rather than counted from [`Chat::turns`], because a
    /// failed turn appends a [`MAGI_NOTE`] message that no agent wrote. The
    /// number names artifacts, so it has to match what was invoked.
    pub fn agent_turns(&self) -> usize {
        self.seat.turns
    }
}

/// A conversation store on disk.
#[derive(Debug, Clone)]
pub struct Chats {
    root: PathBuf,
}

impl Chats {
    /// The operator's conversations, `<home>/chats`.
    pub fn open() -> Self {
        Self::at(crate::run::home().join("chats"))
    }

    /// A store at an explicit root. Tests use this, which is why none of them
    /// need the operator's real home.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Directory holding the conversation files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path for one conversation id.
    pub fn path_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// Where one conversation's prompts and CLI output are kept.
    ///
    /// Beside the record rather than inside it, with the same stem convention a
    /// run's nodes use, so a conversation that went wrong can be read back
    /// turn by turn - which is the only way to tell "the agent said nothing"
    /// apart from "magi never asked it".
    pub fn artifacts_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.artifacts"))
    }

    /// Write a conversation, atomically, so a process killed mid-write leaves
    /// the previous state readable rather than a truncated file that would lose
    /// the whole interview.
    pub fn put(&self, c: &mut Chat) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        c.updated_at = Timestamp::now();
        let body = serde_json::to_string_pretty(c).context("serialize chat")?;
        let path = self.path_of(&c.id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// Load a conversation by id or unambiguous id prefix.
    pub fn get(&self, id: &str) -> Result<Chat> {
        let resolved = self.resolve_id(id)?;
        read_path(&self.path_of(&resolved))
    }

    /// Every conversation on disk: open first, then newest first.
    ///
    /// Open first because that ordering is the product - the list exists to
    /// show the operator what is still being talked through, and a filed
    /// interview is history underneath it. Unreadable files are skipped rather
    /// than fatal: one corrupt record must not take the web UI down, and must
    /// certainly not hide the open conversation the operator came back for.
    pub fn list(&self) -> Vec<Chat> {
        let mut all: Vec<Chat> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| read_path(&p).ok())
            .collect();
        all.sort_unstable_by(|a, b| {
            let rank = |c: &Chat| u8::from(!c.status.open());
            rank(a).cmp(&rank(b)).then_with(|| b.id.cmp(&a.id))
        });
        all
    }

    /// Expand an id prefix to exactly one conversation id. The short id the
    /// phone shows is a suffix, so that is accepted too.
    pub fn resolve_id(&self, prefix: &str) -> Result<String> {
        if self.path_of(prefix).is_file() {
            return Ok(prefix.to_owned());
        }
        let hits: Vec<String> = self
            .list()
            .into_iter()
            .map(|c| c.id)
            .filter(|id| id.starts_with(prefix) || id.ends_with(prefix))
            .collect();
        match hits.len() {
            1 => Ok(hits.into_iter().next().expect("exactly one hit")),
            0 => bail!("no chat matches `{prefix}`"),
            _ => bail!(
                "`{prefix}` matches {} chats: {}",
                hits.len(),
                hits.join(", ")
            ),
        }
    }

    /// Newest modification time in the store, in milliseconds, for change
    /// detection. The web UI compares this instead of re-reading every
    /// conversation, so an idle phone on a slow link costs one `stat` per file.
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

    /// How many conversations are still open. The badge on the phone.
    pub fn count_open(&self) -> usize {
        self.list().iter().filter(|c| c.status.open()).count()
    }
}

/// Open a conversation and take the first agent turn.
///
/// The record is written to disk *before* the agent is invoked, so an agent
/// that fails on the very first turn still leaves the operator a conversation
/// they can look at, retry into, or abandon - rather than nothing at all.
///
/// `agent` is resolved by [`plan::pick`], the same policy `magi plan` uses: an
/// explicit id wins and is an error rather than a fallback when it is not
/// runnable, otherwise a `claude` seat, otherwise the first runnable agent in
/// roster order. Called rather than copied, because two copies of a preference
/// order drift and the copy that drifts is the one nobody reads.
///
/// `from` is the conversation this one was derived from, when the operator
/// asked to continue an existing interview in a different repository (see
/// [`derived_background`]). It is read, never written: the source chat's
/// `status`, `turns` and `draft` are left exactly as they were.
pub async fn start(
    store: &Chats,
    cfg: &Config,
    repo: PathBuf,
    idea: &str,
    agent: Option<&str>,
    from: Option<&Chat>,
) -> Result<Chat> {
    let idea = idea.trim();
    if idea.is_empty() {
        bail!("an interview needs something to start from: say what you want to change");
    }
    // Absolute, because the daemon that eventually runs the filed task has its
    // own working directory and a relative path would mean the wrong
    // repository.
    let repo = repo.canonicalize().unwrap_or(repo);
    // The API's `agent` beats the config, the config beats the built-in order.
    // On a phone there is no flag to pass, so `[roles] planner` is the only
    // way an operator states who they want to be interviewed by.
    let want = agent.or(cfg.roles.planner.as_deref());
    let spec = plan::pick(&cfg.agents, want, &plan::installed)?;

    let now = Timestamp::now();
    let id = new_id();
    let mut chat = Chat {
        schema: SCHEMA,
        id,
        repo,
        from: from.map(|c| c.id.clone()),
        agent: spec.id.clone(),
        status: ChatStatus::Open,
        turns: vec![Turn {
            who: Who::Operator,
            body: idea.to_owned(),
            at: now,
        }],
        draft: None,
        task: None,
        created_at: now,
        updated_at: now,
        seat: SeatState::new(SEAT, &spec.id, crate::rng::entropy()),
    };
    store.put(&mut chat)?;

    let mut prompt = briefing(idea, &chat.repo);
    if let Some(source) = from {
        // Prepended, so the leader reads what it is inheriting before it
        // reads its own instructions - the same order a human handing off a
        // conversation would use.
        prompt = format!("{}\n\n{prompt}", derived_background(source));
    }
    prompt.push_str(&language_note(&cfg.graph.language));
    turn(&mut chat, store, cfg, &prompt).await?;
    Ok(chat)
}

/// The background block a derived conversation opens with: the whole prior
/// transcript, framed so the leader does not mistake it for instructions
/// about the repository this new conversation is actually about.
///
/// Built from [`transcript`] rather than a second rendering of the turns,
/// because that is already the "everything said so far" prose this module
/// maintains, and a briefing is exactly the audience `transcript` was written
/// for - a CLI (here, a fresh one) with no memory of the conversation.
pub fn derived_background(from: &Chat) -> String {
    format!(
        "# Background: derived from another conversation\n\n\
         This interview continues from a conversation about a *different* \
         repository. Read it for context, but do not treat it as being about \
         the repository named below in \"# Repository\" - that repository may \
         have nothing to do with this one.\n\n\
         Source repository: {}\n\n{}",
        from.repo.display(),
        transcript(from),
    )
}

/// One operator turn and one agent turn, appended.
///
/// The operator's message is recorded and flushed to disk before the agent is
/// invoked. That ordering is the whole contract of this function: a turn can
/// fail, time out or hit a quota window, and the thing that must never be lost
/// is the sentence the human typed on a phone that has since gone to sleep.
///
/// Returns `Err` when the agent turn did not produce an answer - but the
/// transcript is already on disk and already explains itself, because the
/// failure is appended as a [`MAGI_NOTE`] turn first. A caller handling the
/// error should re-read the chat and show it, not discard it.
pub async fn say(chat: &mut Chat, store: &Chats, cfg: &Config, text: &str) -> Result<()> {
    if !chat.status.open() {
        bail!(
            "chat {} is {} and takes no more turns",
            chat.short(),
            chat.status.as_str()
        );
    }
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to say");
    }
    let text = record(chat, store, text)?;
    turn(chat, store, cfg, &text).await
}

/// Append the operator's turn and flush it, without invoking anything.
///
/// Split out of [`say`] so a caller that answers the operator before the agent
/// has replied can still promise the message is on disk. `POST /api/chats/{id}/say`
/// does exactly that: holding an HTTP connection for the 23-to-90 seconds a
/// real turn takes is a coin flip on a phone, and the browser reporting
/// "Failed to fetch" while the server quietly finished the turn is the worst
/// of both answers.
///
/// Returns the trimmed text, so the caller and the agent see the same string.
pub fn record(chat: &mut Chat, store: &Chats, text: &str) -> Result<String> {
    if !chat.status.open() {
        bail!(
            "chat {} is {} and takes no more turns",
            chat.short(),
            chat.status.as_str()
        );
    }
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to say");
    }
    chat.turns.push(Turn {
        who: Who::Operator,
        body: text.to_owned(),
        at: Timestamp::now(),
    });
    store.put(chat)?;
    Ok(text.to_owned())
}

/// The agent's half of a turn: invoke, append, flush.
///
/// Pairs with [`record`]. `text` is the operator's message that this reply
/// answers - the same string `record` returned, so the transcript and the
/// prompt cannot disagree.
pub async fn respond(chat: &mut Chat, store: &Chats, cfg: &Config, text: &str) -> Result<()> {
    turn(chat, store, cfg, text).await
}

/// Invoke the interviewing agent once and append what it said.
///
/// `prompt` is only the new material. Whether that is enough depends on the
/// CLI: [`agent::has_session`] answers honestly - it is `false` when sessions
/// are switched off, before the first turn, or for a CLI that never reported an
/// id back - and only then is the transcript prepended, because a model with no
/// memory of the interview would otherwise answer the last sentence in a
/// vacuum. When the CLI *can* resume, magi sends nothing extra: paying for the
/// whole conversation on every message is the cost this design exists to avoid,
/// and a magi-authored replay of history is also a second, divergent version of
/// it.
async fn turn(chat: &mut Chat, store: &Chats, cfg: &Config, prompt: &str) -> Result<()> {
    let spec = cfg
        .agents
        .iter()
        .find(|a| a.id == chat.agent)
        .with_context(|| {
            format!(
                "chat {} was interviewed by agent `{}`, which is no longer in \
                 the roster; restore it in magi.toml or start a new chat",
                chat.short(),
                chat.agent
            )
        })?;

    let resuming = agent::has_session(spec.kind, &chat.seat, cfg.graph.sessions);
    let body = if resuming {
        prompt.to_owned()
    } else {
        format!("{}\n\n{prompt}", transcript(chat))
    };

    let artifacts = store.artifacts_of(&chat.id);
    let stem = format!("turn-{}", chat.seat.turns + 1);
    let cache_dir = cfg.cache_dir();
    let inv = Invocation {
        cwd: &chat.repo,
        prompt: &body,
        timeout: TURN_TIMEOUT,
        // The interviewer writes a task file into its reply, never into the
        // repository: the competing agents do the implementation, and a
        // repository the planner has already edited makes their diffs
        // unjudgeable.
        allow_write: false,
        sessions: cfg.graph.sessions,
        artifacts: &artifacts,
        stem: &stem,
        run: &chat.id,
        node: "chat",
        cache_dir: cache_dir.as_deref(),
    };

    let outcome = agent::invoke(spec, &mut chat.seat, &inv).await;
    let note = |why: String| Turn {
        who: Who::Agent,
        body: format!("{MAGI_NOTE}{why}"),
        at: Timestamp::now(),
    };
    let (reply, failure) = match outcome {
        Err(e) => (
            note(format!("could not run agent `{}`: {e}", chat.agent)),
            Some(format!("could not run agent `{}`: {e}", chat.agent)),
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
                chat.agent
            );
            (note(why.clone()), Some(why))
        }
        Ok(out) if out.timed_out => {
            let why = format!(
                "agent `{}` did not answer within {}s; your message is saved",
                chat.agent,
                TURN_TIMEOUT.as_secs()
            );
            (note(why.clone()), Some(why))
        }
        Ok(out) if !out.usable() => {
            let why = format!(
                "agent `{}` produced no answer (exit {}); your message is saved",
                chat.agent,
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

    // A reply carrying no fenced draft leaves the existing one alone. The agent
    // asking one more follow-up question must not erase the task file it
    // already wrote, which the operator may well be reading at that moment.
    if let Some(draft) = extract_draft(&reply.body) {
        chat.draft = Some(draft);
    }
    chat.turns.push(reply);
    store.put(chat)?;

    match failure {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
}

/// Everything said so far, as prose, for a CLI that cannot resume.
///
/// Only reached when [`agent::has_session`] says the conversation cannot be
/// continued on the CLI's side. It is a fallback and not the design: it re-pays
/// for the history on every turn and it is magi's rendering of the
/// conversation rather than the model's own.
fn transcript(chat: &Chat) -> String {
    let mut out = String::from(
        "You are mid-interview. This CLI cannot resume its own conversation, \
         so here is everything said so far; answer only the last message.\n",
    );
    for t in &chat.turns {
        let who = match t.who {
            Who::Operator => "operator",
            Who::Agent => "you",
        };
        out.push_str(&format!("\n## {who}\n\n{}\n", t.body.trim()));
    }
    out
}

/// Validate the draft with [`plan::review_draft`] and queue it.
///
/// Returns the queued task's id. The conversation is left on disk either way:
/// a refused draft is a conversation to continue, not an error to recover
/// from, and the operator's next message can ask for the missing section.
pub fn file_draft(chat: &mut Chat, store: &Chats, queue: &Queue, priority: i32) -> Result<String> {
    if let Err(problems) = draft_problems(chat) {
        bail!(
            "this draft is not fileable yet:\n- {}",
            problems.join("\n- ")
        );
    }
    let body = chat
        .draft
        .clone()
        .expect("draft_problems accepted a chat with a draft");

    // `title_from` rather than a title the agent was asked to supply
    // separately: the task file's first line already is the title, and asking
    // for it twice is how the two come to disagree.
    let title = queue::title_from(&body, 72);
    // `Human`, not `Agent`: the agent conducted the interview, but the change
    // being asked for is the operator's, and "who asked for this" is the
    // question `source` exists to answer.
    let mut task = Task::new(title, body, chat.repo.clone(), Source::Human);
    task.priority = priority;
    queue.put(&mut task)?;

    chat.task = Some(task.id.clone());
    chat.status = ChatStatus::Filed;
    store.put(chat)?;
    Ok(task.id)
}

/// Is this conversation's draft fileable, and if not, what is wrong with it?
///
/// Every problem is returned, not the first: an operator about to ask the agent
/// for a fix wants the whole list, and a validator that reveals one defect per
/// round turns one follow-up message into three.
///
/// [`plan::SHORT_DRAFT`] alone does not refuse. Length is a smell, not a
/// defect, and a genuinely small change deserves a small task file - which is
/// exactly the judgement `magi plan` makes, so the browser path makes it too.
/// It is still reported, because a two-line draft is usually an interview that
/// ended early.
pub fn draft_problems(chat: &Chat) -> Result<(), Vec<String>> {
    let Some(body) = chat.draft.as_deref() else {
        return Err(vec![
            "this chat has no draft yet: the agent has not written a task file".to_owned(),
        ]);
    };
    match plan::review_draft(body) {
        Ok(()) => Ok(()),
        Err(problems) => {
            if problems.iter().all(|p| p == plan::SHORT_DRAFT) {
                Ok(())
            } else {
                Err(problems)
            }
        }
    }
}

/// The briefing the agent is opened with.
///
/// Pure, so the one property that matters can be asserted without an
/// interview: it carries [`plan::TASK_FILE_SPEC`] verbatim. The spec and
/// [`plan::review_draft`] are checked against each other by `plan`'s own tests,
/// so including it here is what keeps this path from asking for a shape the
/// validator will refuse - a twenty-message interview rejected for a reason the
/// operator was never told is the worst outcome this module has.
///
/// The output contract is the other half. `magi plan` tells the agent to write
/// a file, which works because that agent has a terminal and a filesystem the
/// operator is watching. Here the reply *is* the channel: the task file comes
/// back inside a fenced block tagged `task`, and [`extract_draft`] is the only
/// thing that reads it.
pub fn briefing(idea: &str, repo: &Path) -> String {
    format!(
        "You are the planning leader for magi, which runs a blind \
         multi-agent implementation competition: several agents will implement \
         the task file you write, in isolated worktrees, unaware of each other, \
         and judges will rank the results without knowing who wrote what.\n\n\
         Your job is not to implement anything. It is to interview the operator \
         until the change is pinned down, and then write one task file.\n\n\
         The operator is on a phone. Every message you send is read on a small \
         screen, so keep it short: no preamble, no restating what they just \
         said.\n\n\
         # Repository\n\n{repo}\n\n\
         Read it before you start asking. Questions the code already answers \
         spend the operator's patience for nothing. Do not modify it: the \
         competing agents do the implementation, and a repository you have \
         already edited makes their diffs unjudgeable.\n\n\
         # The idea\n\n{idea}\n\n\
         # How to run the interview\n\n\
         - Ask about what you cannot determine yourself: intent, scope, which \
         of several defensible designs the operator wants, what must not \
         change.\n\
         - Ask about ONE thing per message and wait for the answer. This is a \
         phone, not a form: a message with five questions in it gets one of \
         them answered.\n\
         - Do not produce the task file after one exchange.\n\
         - Disagree when you have grounds. A leader that agrees with everything \
         adds nothing to what the operator already typed.\n\
         - Confirm the plan in your own words and get an explicit yes before \
         writing.\n\n\
         # How to deliver the task file\n\n\
         When the operator agrees the plan is right, put the whole task file in \
         your reply inside a fenced block tagged `task`, like this:\n\n\
         ```task\n\
         # <the task file>\n\
         ```\n\n\
         Nothing else goes in that block, and there is exactly one of them per \
         message. magi extracts it and files it; a task file written to a file \
         on disk, or pasted without the fence, is one magi cannot see. You may \
         send a revised version later in the same conversation - the newest \
         `task` block wins - and while you are still asking questions, send no \
         `task` block at all.\n\n\
         magi will refuse a task file with no completion criteria, so those are \
         not optional.\n\n\
         # Task file specification\n\n{spec}",
        repo = repo.display(),
        spec = plan::TASK_FILE_SPEC,
    )
}

/// The interview is the operator talking, so their language matters more here
/// than in any prompt the graph sends: an agent that answers a Japanese
/// question in English makes the conversation slower for exactly the person
/// magi is trying to help.
fn language_note(language: &str) -> String {
    if language.trim().is_empty() || language.eq_ignore_ascii_case("en") {
        String::new()
    } else {
        format!("\n\nConduct the interview in {language}, and write the task file in {language}.")
    }
}

/// Pull the task draft out of an agent reply, if it wrote one.
///
/// The *last* fenced `task` block, not the first. A conversation revises: an
/// agent that rewrites the task file after one more answer sends both versions
/// over the course of the interview, and within one message it may quote what
/// it had before changing it. The newest block is the one the operator has been
/// reading and the one they are about to approve.
///
/// Blocks tagged anything else - ```` ```rust ````, ```` ```json ```` - are
/// ignored, so an agent illustrating its plan with code does not overwrite the
/// draft with a snippet. An unterminated block is still taken: a reply cut off
/// mid-draft is worth showing the operator, who can then just ask for it again.
pub fn extract_draft(reply: &str) -> Option<String> {
    let mut last: Option<String> = None;
    let mut open: Option<(usize, Vec<&str>)> = None;
    for line in reply.lines() {
        let trimmed = line.trim_start();
        // Backticks are one byte each, so the count is also the byte offset of
        // the info string.
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        match &mut open {
            Some((width, body)) => {
                if ticks >= *width && trimmed[ticks..].trim().is_empty() {
                    last = Some(joined(body));
                    open = None;
                } else {
                    body.push(line);
                }
            }
            None => {
                if ticks >= 3 && trimmed[ticks..].trim().eq_ignore_ascii_case("task") {
                    open = Some((ticks, Vec::new()));
                }
            }
        }
    }
    if let Some((_, body)) = open {
        last = Some(joined(&body));
    }
    last.filter(|s| !s.trim().is_empty())
}

/// A fenced block's lines as one document, newline-terminated the way a file
/// would be, because [`plan::review_draft`] reads it as a task file.
fn joined(lines: &[&str]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn read_path(path: &Path) -> Result<Chat> {
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

    use super::*;

    /// A store of its own, with no process-global state - which is the point of
    /// [`Chats::at`], and why these can run in parallel.
    fn store() -> (tempfile::TempDir, Chats) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chats = Chats::at(tmp.path().join("chats"));
        (tmp, chats)
    }

    /// A task file of the shape [`plan::TASK_FILE_SPEC`] describes, long enough
    /// that length is not one of the problems under test.
    fn good_draft() -> String {
        "# Report per-node durations in `magi show`\n\
         \n\
         ## Context\n\
         \n\
         `magi show` prints a run's nodes but not how long any of them took, so \
         the operator cannot see which seat is expensive. The data is already \
         in `run.events`.\n\
         \n\
         ## Change\n\
         \n\
         Add a duration column to the node table in `src/report.rs`.\n\
         \n\
         ## Constraints\n\
         \n\
         Do not change the JSON shape of a run record.\n\
         \n\
         ## Completion criteria\n\
         \n\
         - [ ] `magi show <run>` prints a duration for every completed node.\n\
         - [ ] A node with no end event prints nothing rather than zero.\n\
         \n\
         ## Out of scope\n\
         \n\
         The TUI's detail pane.\n"
            .to_owned()
    }

    /// A `kind = "command"` agent whose whole behaviour is a POSIX shell
    /// script. No test in this module may spawn a real agent CLI: they are the
    /// operator's paid subscriptions, they reach the network, and they are not
    /// installed on CI.
    fn mock_agent(dir: &Path, script: &str, env: BTreeMap<String, String>) -> AgentSpec {
        let path = dir.join("mock-chat-agent.sh");
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

    /// A config whose only agent is `spec`, with the graph left at its
    /// defaults except for the language, so `language_note` stays out of the
    /// prompt assertions.
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
    /// the leader received on stdin.
    const ECHO: &str = "#!/bin/sh\ncat\n";

    fn env(reply: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("MOCK_REPLY".to_owned(), reply.to_owned())])
    }

    #[test]
    fn the_frozen_json_field_names_round_trip_through_disk() {
        let (tmp, chats) = store();
        let mut chat = Chat {
            schema: SCHEMA,
            id: "20260903-014455-ab12".to_owned(),
            repo: tmp.path().to_owned(),
            from: None,
            agent: "sonnet".to_owned(),
            status: ChatStatus::Open,
            turns: vec![Turn {
                who: Who::Operator,
                body: "rework the config loader".to_owned(),
                at: Timestamp::now(),
            }],
            draft: None,
            task: None,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            seat: SeatState::new(SEAT, "sonnet", 7),
        };
        chats.put(&mut chat).expect("put");

        // Asserted literally, against the text on disk. The web UI is written
        // against these names by hand, so a rename that only round-trips
        // through serde would break the phone silently.
        let raw = std::fs::read_to_string(chats.path_of(&chat.id)).expect("read back");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        for field in [
            "schema",
            "id",
            "repo",
            "from",
            "agent",
            "status",
            "turns",
            "draft",
            "task",
            "created_at",
            "updated_at",
        ] {
            assert!(v.get(field).is_some(), "missing field `{field}`");
        }
        assert_eq!(v["schema"], 1);
        assert_eq!(v["status"], "open");
        assert_eq!(v["turns"][0]["who"], "operator");
        assert_eq!(v["turns"][0]["body"], "rework the config loader");
        assert!(v["turns"][0].get("at").is_some());
        assert!(v["draft"].is_null());
        assert!(v["task"].is_null());
        assert!(v["from"].is_null());

        let back = chats.get(&chat.id).expect("get");
        assert_eq!(back.id, chat.id);
        assert_eq!(back.turns, chat.turns);
        assert_eq!(back.status, ChatStatus::Open);
        assert_eq!(back.from, None);
    }

    /// A conversation recorded before `from` existed must still read: the
    /// `#[serde(deny_unknown_fields)]` on [`Chat`] would otherwise make this
    /// field's addition a breaking change for every chat already on disk.
    #[test]
    fn a_chat_recorded_without_a_from_field_still_reads() {
        let (tmp, chats) = store();
        let path = chats.path_of("20260903-014455-ab12");
        std::fs::create_dir_all(chats.root()).expect("chats dir");
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": SCHEMA,
                "id": "20260903-014455-ab12",
                "repo": tmp.path(),
                "agent": "sonnet",
                "status": "open",
                "turns": [],
                "draft": null,
                "task": null,
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string(),
                "seat": SeatState::new(SEAT, "sonnet", 7),
            })
            .to_string(),
        )
        .expect("write pre-`from` chat");

        let chat = chats.get("20260903-014455-ab12").expect("must still read");
        assert_eq!(chat.from, None);
    }

    #[test]
    fn derived_background_names_the_source_repository_and_carries_the_transcript() {
        let chat = Chat {
            schema: SCHEMA,
            id: "20260903-014455-ab12".to_owned(),
            repo: PathBuf::from("/repo/other"),
            from: None,
            agent: "sonnet".to_owned(),
            status: ChatStatus::Open,
            turns: vec![
                Turn {
                    who: Who::Operator,
                    body: "rework the queue drain".to_owned(),
                    at: Timestamp::now(),
                },
                Turn {
                    who: Who::Agent,
                    body: "which part of the drain?".to_owned(),
                    at: Timestamp::now(),
                },
            ],
            draft: None,
            task: None,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            seat: SeatState::new(SEAT, "sonnet", 7),
        };
        let background = derived_background(&chat);
        assert!(background.contains("/repo/other"));
        assert!(background.contains("rework the queue drain"));
        assert!(background.contains("which part of the drain?"));
        assert!(background.contains("different"));
    }

    #[tokio::test]
    async fn starting_a_derived_chat_carries_the_source_transcript_and_leaves_it_untouched() {
        let (tmp, chats) = store();
        let source_spec = mock_agent(tmp.path(), REPLY, env("which module?"));
        let source_cfg = config(source_spec);
        let source = start(
            &chats,
            &source_cfg,
            tmp.path().to_owned(),
            "rework the queue drain",
            None,
            None,
        )
        .await
        .expect("start source");
        let before = source.clone();

        let other_repo = tmp.path().join("other-repo");
        std::fs::create_dir_all(&other_repo).expect("other repo dir");
        // Overwrites the script `source_spec` pointed at: the source's own
        // turn already ran, so only the derived chat's invocation sees this.
        let echo_spec = mock_agent(tmp.path(), ECHO, BTreeMap::new());
        let derived_cfg = config(echo_spec);
        let derived = start(
            &chats,
            &derived_cfg,
            other_repo,
            "same idea, different repository",
            None,
            Some(&source),
        )
        .await
        .expect("start derived");

        assert_eq!(derived.from.as_deref(), Some(source.id.as_str()));

        let prompt = &derived.turns.last().expect("agent reply").body;
        assert!(prompt.contains("Background: derived from another conversation"));
        assert!(prompt.contains(&source.repo.display().to_string()));
        assert!(prompt.contains("rework the queue drain"));
        assert!(prompt.contains("same idea, different repository"));

        // Deriving a chat must not touch the one it came from.
        let reread = chats.get(&source.id).expect("source still on disk");
        assert_eq!(reread.status, before.status);
        assert_eq!(reread.turns, before.turns);
        assert_eq!(reread.draft, before.draft);
    }

    #[test]
    fn extract_draft_takes_the_last_task_block_and_ignores_other_fences() {
        let reply = "here is a sketch\n\
                     \n\
                     ```rust\n\
                     fn not_the_draft() {}\n\
                     ```\n\
                     \n\
                     ```task\n\
                     # first version\n\
                     ```\n\
                     \n\
                     ```json\n\
                     {\"also\": \"not it\"}\n\
                     ```\n\
                     \n\
                     revised:\n\
                     \n\
                     ```task\n\
                     # second version\n\
                     ## Completion criteria\n\
                     ```\n";
        assert_eq!(
            extract_draft(reply).as_deref(),
            Some("# second version\n## Completion criteria\n")
        );
    }

    #[test]
    fn extract_draft_returns_none_when_there_is_no_task_block() {
        assert_eq!(extract_draft("which storage backend do you want?"), None);
        assert_eq!(extract_draft("```rust\nfn f() {}\n```\n"), None);
        // An empty block is not a draft: filing it would produce a task with
        // nothing in it.
        assert_eq!(extract_draft("```task\n```\n"), None);
    }

    #[tokio::test]
    async fn a_reply_with_no_draft_leaves_the_existing_draft_in_place() {
        let (tmp, chats) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("one more thing: which module?"));
        let cfg = config(spec);
        let mut chat = start(
            &chats,
            &cfg,
            tmp.path().to_owned(),
            "add durations",
            None,
            None,
        )
        .await
        .expect("start");
        chat.draft = Some(good_draft());
        chats.put(&mut chat).expect("put");

        say(&mut chat, &chats, &cfg, "the report module")
            .await
            .expect("say");

        assert_eq!(chat.draft.as_deref(), Some(good_draft().as_str()));
        assert_eq!(
            chats.get(&chat.id).expect("get").draft.as_deref(),
            Some(good_draft().as_str())
        );
    }

    #[test]
    fn the_briefing_carries_the_task_file_spec_and_the_task_fence() {
        let brief = briefing("rework the config loader", Path::new("/repo"));
        // The spec verbatim, so the shape asked for cannot drift from the shape
        // `plan::review_draft` enforces.
        assert!(brief.contains(plan::TASK_FILE_SPEC));
        assert!(brief.contains("```task"));
        assert!(brief.contains("rework the config loader"));
        assert!(brief.contains("/repo"));
        assert!(brief.contains("completion criteria"));
    }

    #[test]
    fn file_draft_refuses_a_bad_draft_with_every_problem() {
        let (tmp, chats) = store();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut chat = Chat {
            schema: SCHEMA,
            id: "20260903-014455-ab12".to_owned(),
            repo: tmp.path().to_owned(),
            from: None,
            agent: "mock".to_owned(),
            status: ChatStatus::Open,
            turns: Vec::new(),
            // Short *and* missing completion criteria: both must be reported,
            // or the operator asks for one fix and gets refused again.
            draft: Some("# do the thing\n\nsome context.\n".to_owned()),
            task: None,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            seat: SeatState::new(SEAT, "mock", 7),
        };

        let problems = draft_problems(&chat).expect_err("a draft with no criteria is not fileable");
        assert!(
            problems.len() >= 2,
            "expected every problem, got {problems:?}"
        );
        assert!(problems.iter().any(|p| p.contains("completion criteria")));
        assert!(problems.iter().any(|p| p == plan::SHORT_DRAFT));

        let err = file_draft(&mut chat, &chats, &queue, 0)
            .expect_err("file_draft must refuse it too")
            .to_string();
        for p in &problems {
            assert!(err.contains(p.as_str()), "`{p}` missing from `{err}`");
        }
        assert_eq!(chat.status, ChatStatus::Open);
        assert!(chat.task.is_none());
        assert!(queue.list().is_empty());
    }

    #[test]
    fn file_draft_queues_a_good_draft_and_records_the_task() {
        let (tmp, chats) = store();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut chat = Chat {
            schema: SCHEMA,
            id: "20260903-014455-cd34".to_owned(),
            repo: tmp.path().to_owned(),
            from: None,
            agent: "mock".to_owned(),
            status: ChatStatus::Open,
            turns: Vec::new(),
            draft: Some(good_draft()),
            task: None,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            seat: SeatState::new(SEAT, "mock", 7),
        };

        let id = file_draft(&mut chat, &chats, &queue, 5).expect("file");

        assert_eq!(chat.status, ChatStatus::Filed);
        assert_eq!(chat.task.as_deref(), Some(id.as_str()));
        assert_eq!(
            chats.get(&chat.id).expect("get").task.as_deref(),
            Some(id.as_str()),
            "the task id must survive on disk, or the phone shows an unfiled chat"
        );

        let task = queue.get(&id).expect("queued task");
        assert_eq!(task.title, queue::title_from(&good_draft(), 72));
        assert_eq!(task.instruction, good_draft());
        assert_eq!(task.priority, 5);
        assert_eq!(task.source, Source::Human);
    }

    #[tokio::test]
    async fn say_appends_the_operator_turn_then_the_agent_turn() {
        let (tmp, chats) = store();
        let spec = mock_agent(tmp.path(), REPLY, env("which module?"));
        let cfg = config(spec);
        let mut chat = start(
            &chats,
            &cfg,
            tmp.path().to_owned(),
            "add durations",
            None,
            None,
        )
        .await
        .expect("start");
        // start is one operator turn (the idea) plus one agent turn.
        assert_eq!(chat.turns.len(), 2);
        assert_eq!(chat.turns[0].who, Who::Operator);
        assert_eq!(chat.turns[1].who, Who::Agent);

        say(&mut chat, &chats, &cfg, "the report module")
            .await
            .expect("say");

        assert_eq!(chat.turns.len(), 4);
        assert_eq!(chat.turns[2].who, Who::Operator);
        assert_eq!(chat.turns[2].body, "the report module");
        assert_eq!(chat.turns[3].who, Who::Agent);
        assert_eq!(chat.turns[3].body, "which module?");
        assert_eq!(chats.get(&chat.id).expect("get").turns, chat.turns);
    }

    #[tokio::test]
    async fn a_failed_turn_keeps_the_operator_message_and_says_what_happened() {
        let (tmp, chats) = store();
        let good = mock_agent(tmp.path(), REPLY, env("which module?"));
        let cfg = config(good);
        let mut chat = start(
            &chats,
            &cfg,
            tmp.path().to_owned(),
            "add durations",
            None,
            None,
        )
        .await
        .expect("start");

        // The chat is bound to roster agent `mock`, so break what `mock`
        // actually runs: `mock_agent` rewrites the same script path, which is
        // what it looks like when that CLI stops working mid-interview.
        mock_agent(tmp.path(), BROKEN, BTreeMap::new());
        let err = say(&mut chat, &chats, &cfg, "the report module")
            .await
            .expect_err("a turn with no answer is an error");
        assert!(err.to_string().contains("no answer"), "{err}");

        let on_disk = chats.get(&chat.id).expect("get");
        assert_eq!(on_disk.turns.len(), 4);
        assert_eq!(
            on_disk.turns[2].body, "the report module",
            "the operator's message must survive the failure"
        );
        let note = &on_disk.turns[3];
        assert_eq!(note.who, Who::Agent);
        assert!(
            note.body.starts_with(MAGI_NOTE),
            "the failure must be visible in the transcript: {}",
            note.body
        );
        assert!(note.body.contains("your message is saved"));
    }

    #[test]
    fn list_puts_open_chats_before_filed_ones() {
        let (tmp, chats) = store();
        let make = |id: &str, status: ChatStatus| {
            let mut c = Chat {
                schema: SCHEMA,
                id: id.to_owned(),
                repo: tmp.path().to_owned(),
                from: None,
                agent: "mock".to_owned(),
                status,
                turns: Vec::new(),
                draft: None,
                task: None,
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                seat: SeatState::new(SEAT, "mock", 7),
            };
            chats.put(&mut c).expect("put");
        };
        // The filed one is newest, so ordering by id alone would put it first.
        make("20260901-000000-0001", ChatStatus::Open);
        make("20260902-000000-0002", ChatStatus::Open);
        make("20260903-000000-0003", ChatStatus::Filed);

        let ids: Vec<String> = chats.list().into_iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                "20260902-000000-0002",
                "20260901-000000-0001",
                "20260903-000000-0003"
            ]
        );
        assert_eq!(chats.count_open(), 2);
    }
}
