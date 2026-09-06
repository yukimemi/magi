//! Run state: what happened, where it is stored, and how a run is resumed.
//!
//! Every node writes its result into [`RunState`] and the whole struct is
//! flushed to `run.json` before the next node starts. That is what makes a run
//! resumable: a competition can take an hour, and dying in review round four
//! should not throw away three implementations, nine judge reads and a
//! deliberation.
//!
//! Patches and raw agent transcripts are *not* in `run.json` — they live beside
//! it under `artifacts/`, so the state file stays small enough to read by hand.
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};

use crate::agent::SeatState;
use crate::blind::Leak;
use crate::config::{Config, MergeMode};
use crate::verdict::{Finding, Rejection};

/// On-disk format version. Bumped when a field changes meaning, so a resumed
/// run never half-reads a state file written by a different magi.
///
/// 2: added `RunStatus::Stalled`, `RunState::quota` (rate-limit losses), and
/// the quorum fields on `Tally`. `RunState::load` already fails loudly and
/// clearly on a schema mismatch; an old `run.json` from schema 1 now says so
/// instead of silently half-reading.
pub const SCHEMA: u32 = 2;

/// Where a run got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Worktrees being prepared.
    Prep,
    /// Candidates being implemented.
    Implementing,
    /// Judges ranking blind.
    Judging,
    /// Judges deliberating after a split.
    Deliberating,
    /// Final votes being collected privately.
    Voting,
    /// Winner in the review + verification loop.
    Reviewing,
    /// Gate commands running.
    Gating,
    /// Winner merged.
    Merged,
    /// Winner passed the gate; merge was not requested.
    Ready,
    /// The judgement did not gather enough judges (e.g. rate limiting took out
    /// seats), so the verdict is not trustworthy. The run stopped and kept its
    /// work so it can be resumed or folded — it must never be confused with a
    /// healthy `Ready`.
    Stalled,
    /// Review rounds exhausted with findings still open, or the gate failed.
    Blocked,
    /// The graph could not complete.
    Failed,
}

impl RunStatus {
    /// Is this a terminal state?
    pub fn done(self) -> bool {
        matches!(
            self,
            Self::Merged | Self::Ready | Self::Stalled | Self::Blocked | Self::Failed
        )
    }

    /// The name this status is written and shown under, matching the
    /// `snake_case` serde spelling so a log line, an error message and the
    /// JSON a phone reads all say the same word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prep => "prep",
            Self::Implementing => "implementing",
            Self::Judging => "judging",
            Self::Deliberating => "deliberating",
            Self::Voting => "voting",
            Self::Reviewing => "reviewing",
            Self::Gating => "gating",
            Self::Merged => "merged",
            Self::Ready => "ready",
            Self::Stalled => "stalled",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }

    /// Can this run be carried on from where it stopped?
    ///
    /// Everything except a finished run and a failed one. `execute` skips
    /// nodes already recorded, so re-entering is cheap wherever the run
    /// stopped, and the alternative is always a fresh competition against
    /// work that already exists.
    ///
    /// - `Stalled` re-asks only the seats whose absence collapsed the panel,
    ///   keeping the candidates that were already paid for.
    /// - `Blocked` re-enters the review loop against a branch that is built.
    /// - **A non-terminal status** means the run was interrupted: a parked
    ///   run waiting for its upgrade, or one whose daemon was killed. This
    ///   used to be excluded, which left run 4043 stuck at `reviewing` with
    ///   the deck telling the operator it could not be resumed - the one
    ///   state where resuming is the only sensible answer.
    ///
    /// `Failed` does not qualify: the graph could not complete and there is
    /// no established point to continue from. Nor does a finished run, whose
    /// answer is a new competition.
    ///
    /// Whether anything is *already* driving the run is a separate question,
    /// answered by `daemon::is_working_on` at the callers that need it.
    pub fn resumable(self) -> bool {
        !matches!(self, Self::Merged | Self::Ready | Self::Failed)
    }
}

/// One candidate implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Position in the implementer list.
    pub index: usize,
    /// Blind label as presented to judges.
    pub label: char,
    /// Which agent wrote it. Recorded for the stats tables, never shown to a
    /// judge.
    pub agent: String,
    /// Branch, named after the label so judges can inspect it without learning
    /// the author.
    pub branch: String,
    /// Worktree path.
    pub worktree: PathBuf,
    /// Sanitized author summary.
    #[serde(default)]
    pub summary: String,
    /// `git diff --stat`.
    #[serde(default)]
    pub stat: String,
    /// Files touched.
    #[serde(default)]
    pub files: usize,
    /// Commits ahead of base.
    #[serde(default)]
    pub commits: usize,
    /// True when the agent produced no change at all.
    #[serde(default)]
    pub empty: bool,
    /// Why this candidate is not in the running.
    #[serde(default)]
    pub failed: Option<String>,
    /// Wall-clock time for the implementation.
    #[serde(default)]
    pub duration_ms: u64,
    /// Whether the worktree has been folded away.
    #[serde(default)]
    pub folded: bool,
}

impl Candidate {
    /// Can this candidate be judged?
    pub fn viable(&self) -> bool {
        self.failed.is_none() && !self.empty
    }
}

/// One judge's independent ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Judgement {
    /// Judge seat number, 1-based.
    pub judge: usize,
    /// Seat key.
    pub seat: String,
    /// Agent occupying the seat.
    pub agent: String,
    /// Best-first labels.
    #[serde(default)]
    pub ranking: Vec<char>,
    /// Per-label justification.
    #[serde(default)]
    pub reasons: BTreeMap<String, String>,
    /// Self-reported confidence.
    #[serde(default)]
    pub confidence: Option<u8>,
    /// Order the candidates were presented in, as candidate indices.
    #[serde(default)]
    pub order: Vec<usize>,
    /// Why this judge has no ranking.
    #[serde(default)]
    pub failed: Option<String>,
    /// Wall-clock time.
    #[serde(default)]
    pub duration_ms: u64,
}

/// One judge's turn in a deliberation round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliberationTurn {
    /// Judge seat number, 1-based.
    pub judge: usize,
    /// Agent occupying the seat.
    pub agent: String,
    /// The argument, as written.
    pub body: String,
    /// Where the judge stood at the end of the turn.
    #[serde(default)]
    pub tentative: Option<char>,
}

/// A deliberation round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliberationRound {
    /// 1-based round number.
    pub round: usize,
    /// Turns, in the order they were taken.
    pub turns: Vec<DeliberationTurn>,
}

/// A final vote, collected privately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteRecord {
    /// Judge seat number, 1-based.
    pub judge: usize,
    /// Agent occupying the seat.
    pub agent: String,
    /// The vote.
    #[serde(default)]
    pub vote: Option<char>,
    /// Why.
    #[serde(default)]
    pub reason: String,
    /// Did this judge move from its initial first choice?
    #[serde(default)]
    pub changed: bool,
}

/// A seat that was taken out by a CLI rate limit / quota, recorded so a run
/// whose panel collapsed does not masquerade as a healthy one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaLoss {
    /// Seat key, e.g. `judge-1` or `review-2`.
    pub seat: String,
    /// Node that was running, e.g. `judge`, `vote`, `review`.
    pub node: String,
    /// When the CLI reported the limit.
    pub at: Timestamp,
    /// Reset hint if the CLI printed one, free text.
    #[serde(default)]
    pub reset: Option<String>,
}

/// The mechanical count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tally {
    /// First-choice votes per label.
    pub first_choice: BTreeMap<char, usize>,
    /// Borda points from the initial rankings, used only to break a tie.
    pub borda: BTreeMap<char, usize>,
    /// The winning label.
    pub winner: char,
    /// How many judges produced a usable ranking. A panel of one is not a
    /// consensus and must not be reported as a split.
    #[serde(default)]
    pub rankings: usize,
    /// Did every judge's *initial* first choice agree?
    pub unanimous_initial: bool,
    /// Was deliberation run?
    pub deliberated: bool,
    /// Judges who moved between their initial ranking and their final vote.
    pub changed_votes: usize,
    /// Did the final votes agree?
    pub unanimous_final: bool,
    /// How the tie was broken, when it had to be.
    #[serde(default)]
    pub tie_break: Option<String>,
    /// Configured judge count — the size of the full panel.
    #[serde(default)]
    pub judges: usize,
    /// Judges who actually contributed to the decision (not taken out by a
    /// rate limit and producing a usable rank or vote).
    #[serde(default)]
    pub present: usize,
    /// How many judges are required for a trustworthy verdict. Chosen as a
    /// strict majority (`judges / 2 + 1`): a verdict backed by a minority must
    /// never be presented as a healthy one, while a bare majority is still
    /// real signal. A one-candidate run needs no quorum.
    #[serde(default)]
    pub quorum: usize,
    /// `present >= quorum`, or no quorum was required.
    #[serde(default)]
    pub met_quorum: bool,
}

/// One reviewer's report in a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRecord {
    /// Reviewer seat number, 1-based.
    pub reviewer: usize,
    /// Agent occupying the seat.
    pub agent: String,
    /// Reviewer prose.
    #[serde(default)]
    pub summary: String,
    /// Findings, with magi-assigned ids.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Why this reviewer produced nothing.
    #[serde(default)]
    pub failed: Option<String>,
    /// Wall-clock time.
    #[serde(default)]
    pub duration_ms: u64,
}

/// The fixer's response to a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixRecord {
    /// Agent that applied the fixes.
    pub agent: String,
    /// Finding ids acted on.
    #[serde(default)]
    pub addressed: Vec<String>,
    /// Findings declined, with reasons.
    #[serde(default)]
    pub rejected: Vec<Rejection>,
    /// What changed.
    #[serde(default)]
    pub notes: String,
    /// Did the fix produce a commit?
    #[serde(default)]
    pub committed: bool,
    /// Why the fix step produced nothing.
    #[serde(default)]
    pub failed: Option<String>,
    /// Wall-clock time.
    #[serde(default)]
    pub duration_ms: u64,
}

/// Outcome of one shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutcome {
    /// The command, as configured.
    pub command: String,
    /// Exit code, `None` on timeout or signal.
    pub code: Option<i32>,
    /// Tail of the combined output, for the report and the fix prompt.
    #[serde(default)]
    pub output_tail: String,
    /// Wall-clock time.
    #[serde(default)]
    pub duration_ms: u64,
}

impl CommandOutcome {
    /// Did it pass?
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// One review + verify + fix round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRound {
    /// 1-based round number.
    pub round: usize,
    /// Commit the round reviewed.
    pub head: String,
    /// Reviewer reports.
    pub reviews: Vec<ReviewRecord>,
    /// E2E command outcomes for this round.
    #[serde(default)]
    pub e2e: Vec<CommandOutcome>,
    /// Fixer response, absent when the round was already clean.
    #[serde(default)]
    pub fix: Option<FixRecord>,
    /// Findings that hold the merge.
    #[serde(default)]
    pub blocking: usize,
    /// Round ended with no blocking findings and green verification.
    #[serde(default)]
    pub clean: bool,
}

/// What happened to the winning branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOutcome {
    /// Requested mode.
    pub mode: MergeMode,
    /// Did it land?
    pub ok: bool,
    /// Command output, or the command the operator should run.
    #[serde(default)]
    pub detail: String,
}

/// A timestamped note about a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// When.
    pub at: Timestamp,
    /// Node name.
    pub node: String,
    /// What happened.
    pub message: String,
}

/// What the land loop saw last time it looked at the pull request.
///
/// Strings for `state` and `checks` on purpose: they are `gh`'s vocabulary, and
/// pinning them into an enum here would mean a new GitHub check conclusion
/// turns a readable status into a deserialisation error on a run someone is
/// trying to look at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrRecord {
    /// Pull request url.
    pub url: String,
    /// Pull request number.
    pub number: u64,
    /// `open`, `merged` or `closed`.
    pub state: String,
    /// `pending`, `green`, `red` or `unknown`.
    pub checks: String,
    /// Land round, 1-based, or 0 before the first fix.
    pub round: usize,
    /// Land round budget.
    pub rounds: usize,
}

/// The whole run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    /// On-disk format version.
    pub schema: u32,
    /// Run id, e.g. `20260830-153012-a1b2`.
    pub id: String,
    /// Repository the run operates on.
    pub repo: PathBuf,
    /// Branch the run started from.
    pub base_branch: String,
    /// Commit the run started from.
    pub base_commit: String,
    /// The task, verbatim.
    pub instruction: String,
    /// When the run was created.
    pub created_at: Timestamp,
    /// Last state flush.
    pub updated_at: Timestamp,
    /// Current status.
    pub status: RunStatus,
    /// Seed for labels and session ids.
    pub seed: u64,
    /// Config snapshot, so a resumed run behaves like the original.
    pub config: Config,
    /// Did magi enable `extensions.worktreeConfig`? If so, cleanup turns it off.
    #[serde(default)]
    pub enabled_worktree_config: bool,
    /// Candidates.
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    /// Initial blind rankings.
    #[serde(default)]
    pub judgements: Vec<Judgement>,
    /// Deliberation, if it happened.
    #[serde(default)]
    pub deliberation: Vec<DeliberationRound>,
    /// Private final votes.
    #[serde(default)]
    pub votes: Vec<VoteRecord>,
    /// The count.
    #[serde(default)]
    pub tally: Option<Tally>,
    /// Review rounds.
    #[serde(default)]
    pub reviews: Vec<ReviewRound>,
    /// Final gate.
    #[serde(default)]
    pub gate: Vec<CommandOutcome>,
    /// Merge outcome.
    #[serde(default)]
    pub merge: Option<MergeOutcome>,
    /// Vendor tokens seen in judged material.
    #[serde(default)]
    pub leaks: Vec<Leak>,
    /// Seats lost to a CLI rate limit / quota, in the order they hit.
    #[serde(default)]
    pub quota: Vec<QuotaLoss>,
    /// Parked at a node boundary, waiting to be resumed.
    ///
    /// A run that is neither finished nor being worked on is otherwise
    /// indistinguishable from one whose daemon was killed, and the two want
    /// opposite things from an operator: the first is expected to be resumed,
    /// the second is a leftover. Cleared by the resume that carries it on.
    #[serde(default)]
    pub parked: bool,
    /// Per-seat conversation state.
    #[serde(default)]
    pub seats: BTreeMap<String, SeatState>,
    /// Last observation of the winner's pull request, when a land loop ran.
    ///
    /// Persisted rather than derived from the event log because the phone asks
    /// two questions about a run that has opened a PR - how are its checks and
    /// which round is it on - and parsing prose out of events to answer them
    /// would break the first time an event message was reworded.
    #[serde(default)]
    pub pr: Option<PrRecord>,
    /// Node log.
    #[serde(default)]
    pub events: Vec<Event>,
}

impl RunState {
    /// A fresh run.
    pub fn new(
        repo: PathBuf,
        base_branch: String,
        base_commit: String,
        instruction: String,
        config: Config,
    ) -> Self {
        let now = Timestamp::now();
        let seed = config.blind.seed.unwrap_or_else(crate::rng::entropy);
        Self {
            schema: SCHEMA,
            id: new_id(),
            repo,
            base_branch,
            base_commit,
            instruction,
            created_at: now,
            updated_at: now,
            status: RunStatus::Prep,
            seed,
            config,
            enabled_worktree_config: false,
            candidates: Vec::new(),
            judgements: Vec::new(),
            deliberation: Vec::new(),
            votes: Vec::new(),
            tally: None,
            reviews: Vec::new(),
            gate: Vec::new(),
            merge: None,
            leaks: Vec::new(),
            quota: Vec::new(),
            parked: false,
            seats: BTreeMap::new(),
            pr: None,
            events: Vec::new(),
        }
    }

    /// Directory holding this run's state and artifacts.
    pub fn dir(&self) -> PathBuf {
        run_dir(&self.id)
    }

    /// Short form used in branch names and reports.
    pub fn short(&self) -> &str {
        short_of(&self.id)
    }

    /// Branch name for a label.
    pub fn branch_for(&self, label: char) -> String {
        format!("magi/{}/{}", self.short(), label)
    }

    /// Root of this run's worktrees.
    pub fn worktree_root(&self) -> PathBuf {
        self.config
            .graph
            .worktree_root
            .clone()
            .unwrap_or_else(default_worktree_root)
            .join(self.short())
    }

    /// Note something in the run log and on the tracing stream.
    pub fn event(&mut self, node: &str, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(node, "{message}");
        self.events.push(Event {
            at: Timestamp::now(),
            node: node.to_owned(),
            message,
        });
    }

    /// Flush to `run.json`, atomically.
    pub fn save(&mut self) -> Result<()> {
        self.updated_at = Timestamp::now();
        let dir = self.dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let body = serde_json::to_string_pretty(self).context("serialize run state")?;
        let tmp = dir.join("run.json.tmp");
        std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join("run.json")).with_context(|| "replace run.json")?;
        Ok(())
    }

    /// Load a run by id or unambiguous id prefix.
    pub fn load(id: &str) -> Result<Self> {
        let resolved = resolve_id(id)?;
        let path = run_dir(&resolved).join("run.json");
        let body =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let state: Self =
            serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
        if state.schema != SCHEMA {
            bail!(
                "run {} was written by a different magi (schema {}, this build \
                 speaks {SCHEMA})",
                state.id,
                state.schema
            );
        }
        Ok(state)
    }

    /// The winning candidate, once the tally has run.
    pub fn winner(&self) -> Option<&Candidate> {
        let label = self.tally.as_ref()?.winner;
        self.candidates.iter().find(|c| c.label == label)
    }

    /// Candidates eligible for judging.
    pub fn viable(&self) -> Vec<&Candidate> {
        self.candidates.iter().filter(|c| c.viable()).collect()
    }

    /// Local-time creation stamp for reports.
    pub fn created_local(&self) -> String {
        self.created_at
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// Assert that this run is safe to delete.
    ///
    /// Refuses a run a live daemon is working on, and refuses any run whose
    /// candidate worktrees and branches have not been folded away with `magi
    /// fold`. The fold requirement is the real protection: it is what makes
    /// "delete" mean "remove a record" rather than "throw away a worktree
    /// somebody may still be editing".
    ///
    /// `in_flight` has to come from the caller, because a run's own status
    /// cannot answer the question. A daemon killed mid-run leaves its status at
    /// `implementing` forever, and a guard that trusted that would make every
    /// interrupted run permanently undeletable - the operator's only recourse
    /// being to edit `run.json` by hand, which is exactly the sort of thing
    /// this command exists to avoid. The queue already treats an orphaned
    /// `.lock` from a `SIGKILL`ed daemon the same way; this is that rule for
    /// runs.
    pub fn ensure_can_delete(&self, in_flight: bool) -> Result<()> {
        if in_flight {
            bail!(
                "run {} is being worked on by a live daemon right now",
                self.short()
            );
        }
        if self.candidates.iter().any(|c| !c.folded) {
            bail!(
                "run {} has unfolded candidates; fold first with `magi fold`",
                self.short()
            );
        }
        Ok(())
    }
}

/// The short form of a run id: the trailing block after the last `-`.
///
/// A free function as well as [`RunState::short`], because callers that have
/// only an id - an error message, a daemon status, a route handler - were
/// otherwise reimplementing the split, and two spellings of "short id" is one
/// rename away from branch names that no longer match their run.
pub fn short_of(id: &str) -> &str {
    id.split('-').next_back().unwrap_or(id)
}

/// Where magi keeps its runs.
///
/// `MAGI_HOME` overrides the default, and [`set_home`] overrides both — which
/// is what lets the integration tests drive a whole graph without writing into
/// the operator's real history.
///
/// In a unit test build (`cfg(test)`), falling through to the real
/// `<data_local>/magi` is not a fallback worth having: it is exactly how
/// three broken fixture runs ended up in the operator's actual history and
/// were counted as `unreadable` by the deck. A test that reaches this point
/// forgot to call [`set_home`] (or set `MAGI_HOME`) - that is a bug in the
/// test, not a case to serve, so it panics instead of writing anywhere.
pub fn home() -> PathBuf {
    resolve_home(HOME.get().cloned(), std::env::var_os("MAGI_HOME"))
}

/// The decision `home` makes, taking its two overrides as plain values
/// instead of reading the `OnceLock` and the environment itself.
///
/// Pulled out so the `cfg(test)` panic is asserted directly against a
/// `None, None` input, rather than racing every other unit test in the
/// binary for who touches the process-global `HOME` first.
fn resolve_home(pinned: Option<PathBuf>, magi_home_env: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = pinned {
        return dir;
    }
    if let Some(dir) = magi_home_env {
        return PathBuf::from(dir);
    }
    #[cfg(test)]
    {
        panic!(
            "run::home() was reached in a test without run::set_home() or \
             MAGI_HOME; this would write into the operator's real \
             <data_local>/magi. Call `run::set_home(temp_dir)` before any \
             code path that touches a RunState."
        );
    }
    #[cfg(not(test))]
    {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("magi")
    }
}

/// Pin the run home for this process. The first call wins.
pub fn set_home(dir: PathBuf) {
    let _ = HOME.set(dir);
}

static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// `<home>/runs`.
pub fn runs_root() -> PathBuf {
    home().join("runs")
}

/// The worktree root a run uses when the config sets none: `~/wt/magi`.
///
/// One definition of the default, so the folder the janitor folds and the
/// folder the health view sizes cannot drift apart: a run with no configured
/// [`crate::config::Graph::worktree_root`] lays its worktrees exactly here.
pub fn default_worktree_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wt")
        .join("magi")
}

/// Directory for one run id.
pub fn run_dir(id: &str) -> PathBuf {
    runs_root().join(id)
}

/// Every run id on disk, newest first.
///
/// A directory is a run because of its **name**, not because it holds a
/// readable `run.json`. A run whose very first save lost the machine's last
/// free bytes leaves `<id>/run.json.tmp` and nothing else, and filtering on
/// `run.json` made that run invisible everywhere: not in `magi list`, not in
/// `runs_unreadable`, not on the phone, so nothing could report it and no
/// route could clear it. `88c0` sat like that for two days. Unreadable is
/// counted, never hidden - the readers already say why each one cannot be
/// read, and `fold_unreadable` is how a record like this leaves.
pub fn list_ids() -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(runs_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| is_run_id(name))
        .collect();
    // Ids start with a sortable timestamp.
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// Does `name` have the shape [`new_id`] mints: `YYYYMMDD-HHMMSS-xxxx`?
///
/// The test for "this directory is a run", so a stray folder under
/// `<home>/runs` is not reported as a broken run.
///
/// The tag is checked for length and for being alphanumeric, not for being
/// hex: real ids are hex, but fixtures across this crate name runs
/// `...-dead` / `...-gone` / `...-once`, and a predicate that disowned those
/// would be asserting the fixtures' spelling rather than the shape.
pub fn is_run_id(name: &str) -> bool {
    let mut parts = name.split('-');
    let (Some(day), Some(time), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    day.len() == 8
        && day.bytes().all(|b| b.is_ascii_digit())
        && time.len() == 6
        && time.bytes().all(|b| b.is_ascii_digit())
        && tag.len() == 4
        && tag.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Expand an id prefix to exactly one run id.
pub fn resolve_id(prefix: &str) -> Result<String> {
    // A whole id names its directory, readable state or not: the run whose
    // `run.json` never landed still has to be reachable by `magi show` and
    // by the fold route, which is the only way its record ever leaves.
    if is_run_id(prefix) && run_dir(prefix).is_dir() {
        return Ok(prefix.to_owned());
    }
    let hits: Vec<String> = list_ids()
        .into_iter()
        .filter(|id| id.starts_with(prefix) || id.ends_with(prefix))
        .collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().expect("exactly one hit")),
        0 => bail!("no run matches `{prefix}`"),
        _ => bail!(
            "`{prefix}` matches {} runs: {}",
            hits.len(),
            hits.join(", ")
        ),
    }
}

/// The most recent run, if any.
pub fn latest_id() -> Option<String> {
    list_ids().into_iter().next()
}

/// `YYYYMMDD-HHMMSS-xxxx`, sortable and short enough for a branch name.
///
/// The four hex digits are fresh entropy, **not** `blind.seed`. They were the
/// seed, and a pinned seed then made the whole id a function of the second it
/// started in: two runs a second apart were distinguishable, two in the same
/// second were not. Everything keyed on the id collided with them - the run
/// directory, `artifacts/`, and the candidate worktrees under
/// `wt/magi/<short>/`.
///
/// `tests/common` pins the seed on purpose, so its integration tests all share
/// one suffix. On Windows the suite is slow enough that the seconds differ and
/// nothing showed; on Linux `graph_dropped_stream`'s three tests run inside
/// 16s, so two of them shared a run directory and the second read an artifact
/// the first had written (`impl-B-resume.out`) - a failure that looked like the
/// resume logic misbehaving and was really two runs in one directory.
///
/// A seed exists to make the *blind* decisions reproducible: label assignment
/// and per-judge presentation order. It was never meant to name the run, and
/// `RunState::seed` still carries it for what it is for.
fn new_id() -> String {
    let stamp = Zoned::now().strftime("%Y%m%d-%H%M%S");
    let entropy = crate::rng::entropy();
    format!("{stamp}-{:04x}", (entropy ^ (entropy >> 32)) & 0xffff)
}

/// Keep the last `max` bytes of `text`, on a line boundary.
pub fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = text.len() - max;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    let slice = &text[cut..];
    let start = slice.find('\n').map_or(0, |i| i + 1);
    format!(
        "[... {} earlier bytes omitted ...]\n{}",
        cut,
        &slice[start..]
    )
}

/// Path of a run artifact.
pub fn artifact_path(run: &RunState, name: &str) -> PathBuf {
    run.dir().join("artifacts").join(name)
}

/// Write an artifact, creating the directory if needed.
pub fn write_artifact(run: &RunState, name: &str, body: &str) -> Result<PathBuf> {
    let path = artifact_path(run, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Read an artifact back, e.g. a stored patch on resume.
pub fn read_artifact(run: &RunState, name: &str) -> Option<String> {
    std::fs::read_to_string(artifact_path(run, name)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> RunState {
        RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234def".to_owned(),
            "add retries".to_owned(),
            Config::default(),
        )
    }

    #[test]
    fn resolve_home_prefers_the_pin_then_the_env_var() {
        let pinned = PathBuf::from("/pinned");
        assert_eq!(
            resolve_home(Some(pinned.clone()), Some("/env".into())),
            pinned,
            "a pin wins even over MAGI_HOME"
        );
        assert_eq!(
            resolve_home(None, Some("/env".into())),
            PathBuf::from("/env")
        );
    }

    #[test]
    #[should_panic(expected = "run::set_home()")]
    fn resolve_home_refuses_to_fall_back_to_the_operators_real_home() {
        // Neither override present is exactly the state a test reaches by
        // forgetting `set_home`/`MAGI_HOME` - the accident that put three
        // broken fixture runs into the operator's real history. Asserted
        // against the pure decision directly, not `home()` itself, because
        // `HOME` is a process-wide `OnceLock` another test may have already
        // set - this must not depend on test execution order.
        resolve_home(None, None);
    }

    #[test]
    fn a_run_is_named_by_shape_so_a_state_less_directory_is_still_a_run() {
        // The shape `new_id` mints. A directory answering to it is a run even
        // with no readable `run.json`: that is how a save that ran out of
        // disk stays visible instead of vanishing from every listing.
        assert!(is_run_id(&new_id()));
        assert!(is_run_id("20260904-014540-88c0"));
        // Not runs: a stray folder, a truncated id, a non-hex tag, and an id
        // with an extra segment (a worktree label, say).
        assert!(!is_run_id("scratch"));
        assert!(!is_run_id("20260904-014540"));
        assert!(!is_run_id("20260904-014540-88c0f"));
        assert!(!is_run_id("2026090x-014540-88c0"));
        assert!(!is_run_id("20260904-014540-88c0-A"));
    }

    #[test]
    fn ids_are_sortable_and_short_suffixed() {
        let s = state();
        let parts: Vec<&str> = s.id.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 6);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(s.short(), parts[2]);
    }

    #[test]
    fn branch_names_carry_the_label_not_the_author() {
        let s = state();
        let b = s.branch_for('B');
        assert_eq!(b, format!("magi/{}/B", s.short()));
        assert!(!b.contains("claude"));
    }

    /// A pinned seed reproduces the blind decisions. It must **not** reproduce
    /// the run's identity.
    ///
    /// `assert_eq!(a.short(), b.short())` used to stand where the last
    /// assertion is now, and it was pinning the defect: with the id's suffix
    /// derived from the seed, two runs started in the same second were the
    /// same run as far as the filesystem was concerned - one directory, one
    /// `artifacts/`, one set of candidate worktrees. `tests/common` pins a
    /// seed for every integration test, so on Linux, where the suite is fast,
    /// two tests in `graph_dropped_stream` shared a directory and one read the
    /// other's artifact.
    #[test]
    fn a_pinned_seed_is_reproducible_but_never_the_run_id() {
        let mut cfg = Config::default();
        cfg.blind.seed = Some(1234);
        let a = RunState::new(
            PathBuf::from("/r"),
            "main".to_owned(),
            "c".to_owned(),
            "t".to_owned(),
            cfg.clone(),
        );
        let b = RunState::new(
            PathBuf::from("/r"),
            "main".to_owned(),
            "c".to_owned(),
            "t".to_owned(),
            cfg,
        );
        // What the seed is for: the same shuffles, run after run.
        assert_eq!(a.seed, 1234);
        assert_eq!(a.seed, b.seed);
        // What it is not for. Two runs are two runs, in the same second or
        // not, and everything keyed on the id depends on that.
        assert_ne!(
            a.id, b.id,
            "two runs sharing an id share a directory, artifacts and worktrees"
        );
    }

    #[test]
    fn status_terminality() {
        assert!(RunStatus::Merged.done());
        assert!(RunStatus::Blocked.done());
        assert!(!RunStatus::Reviewing.done());
    }

    #[test]
    fn candidate_viability_excludes_empty_and_failed() {
        let mut c = Candidate {
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
        };
        assert!(c.viable());
        c.empty = true;
        assert!(!c.viable());
        c.empty = false;
        c.failed = Some("timeout".to_owned());
        assert!(!c.viable());
    }

    #[test]
    fn tail_keeps_the_end_on_a_line_boundary() {
        let text = (0..100).map(|i| format!("line {i}\n")).collect::<String>();
        let t = tail(&text, 40);
        assert!(t.starts_with("[..."));
        assert!(t.ends_with("line 99\n"));
        assert!(t.len() < 120);
        assert_eq!(tail("short", 40), "short");
    }

    #[test]
    fn tail_survives_multibyte_cuts() {
        let text = "あ".repeat(50);
        let t = tail(&text, 10);
        assert!(t.contains("earlier bytes omitted"));
        assert!(t.ends_with('あ'));
    }

    #[test]
    fn state_round_trips_through_json() {
        let s = state();
        let body = serde_json::to_string(&s).unwrap();
        let back: RunState = serde_json::from_str(&body).unwrap();
        assert_eq!(back.id, s.id);
        assert_eq!(back.instruction, "add retries");
        assert_eq!(back.status, RunStatus::Prep);
    }

    #[test]
    fn ensure_can_delete_guards_live_and_unfolded_runs() {
        let mut s = state();
        // 1. A daemon is working on it right now.
        s.status = RunStatus::Prep;
        let err = s.ensure_can_delete(true).unwrap_err().to_string();
        assert!(err.contains("live daemon"), "{err}");

        // 2. The same unfinished run with no daemon behind it is a leftover
        // from a killed process, and deletable. Without this an interrupted
        // run could never be removed: its status stays `prep` forever.
        assert!(s.ensure_can_delete(false).is_ok());

        // 3. Unfolded candidates are refused either way — that is the guard
        // that stops a delete from discarding a worktree.
        s.status = RunStatus::Merged;
        s.candidates.push(Candidate {
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
        let err = s.ensure_can_delete(false).unwrap_err().to_string();
        assert!(
            err.contains("magi fold"),
            "error must suggest `magi fold`: {err}"
        );

        // 4. Folded and nobody working on it.
        s.candidates[0].folded = true;
        assert!(s.ensure_can_delete(false).is_ok());
    }
}
