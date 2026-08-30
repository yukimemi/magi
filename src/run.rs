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
pub const SCHEMA: u32 = 1;

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
            Self::Merged | Self::Ready | Self::Blocked | Self::Failed
        )
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
    /// Per-seat conversation state.
    #[serde(default)]
    pub seats: BTreeMap<String, SeatState>,
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
            id: new_id(seed),
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
            seats: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    /// Directory holding this run's state and artifacts.
    pub fn dir(&self) -> PathBuf {
        run_dir(&self.id)
    }

    /// Short form used in branch names and reports.
    pub fn short(&self) -> &str {
        self.id.split('-').next_back().unwrap_or(&self.id)
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
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("wt")
                    .join("magi")
            })
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
}

/// Where magi keeps its runs.
///
/// `MAGI_HOME` overrides the default, and [`set_home`] overrides both — which
/// is what lets the integration tests drive a whole graph without writing into
/// the operator's real history.
pub fn home() -> PathBuf {
    if let Some(dir) = HOME.get() {
        return dir.clone();
    }
    if let Some(dir) = std::env::var_os("MAGI_HOME") {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("magi")
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

/// Directory for one run id.
pub fn run_dir(id: &str) -> PathBuf {
    runs_root().join(id)
}

/// Every run id on disk, newest first.
pub fn list_ids() -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(runs_root())
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

/// Expand an id prefix to exactly one run id.
pub fn resolve_id(prefix: &str) -> Result<String> {
    if run_dir(prefix).join("run.json").is_file() {
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
fn new_id(seed: u64) -> String {
    let stamp = Zoned::now().strftime("%Y%m%d-%H%M%S");
    format!("{stamp}-{:04x}", (seed ^ (seed >> 32)) & 0xffff)
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

    #[test]
    fn seed_from_config_makes_the_run_reproducible() {
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
        assert_eq!(a.seed, 1234);
        assert_eq!(a.seed, b.seed);
        assert_eq!(a.short(), b.short());
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
}
