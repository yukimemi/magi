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
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};

use crate::agent::SeatState;
use crate::blind::Leak;
use crate::config::{Config, MergeMode};
use crate::verdict::{Finding, Rejection, ReviewVote, Severity};

/// On-disk format version. Bumped when a field changes meaning, so a resumed
/// run never half-reads a state file written by a different magi.
///
/// 2: added `RunStatus::Stalled`, `RunState::quota` (rate-limit losses), and
/// the quorum fields on `Tally`. `RunState::load` already fails loudly and
/// clearly on a schema mismatch; an old `run.json` from schema 1 now says so
/// instead of silently half-reading.
///
/// 3: added `RunState::judge_skipped`. A solo candidate makes `judge` write
/// only an event, leaving `judgements` empty forever — indistinguishable from
/// "not yet judged" on every later reentry, which is what let `judge` re-run
/// on a finished run and clobber its status back to `Judging`. The flag is
/// the missing record of the fact that judging was skipped on purpose.
///
/// Also 3: a single-viable-candidate tally records `Tally::judges` as `0` and
/// fills `Tally::uncontested`, instead of leaving the full roster size sitting
/// next to a panel that never sat. A schema-2 record keeps reading as "0 of 3
/// judges present" forever, because a tally is computed once and never
/// recomputed on resume; the bump keeps that stale reading from being mixed
/// with the new meaning.
///
/// 4: added `ReviewRound::progressed`. `graph::STAGNANT_LIMIT` counts
/// consecutive rounds with `progressed == false` to decide whether the
/// review loop should give up early, and a schema-3 record's default
/// `false` would misreport a round that, at the time, actually committed a
/// real diff — the field simply did not exist yet to say so. Without the
/// bump, resuming an old multi-round review could spuriously trip the
/// stagnation check on rounds that were never stagnant.
///
/// 5: added `RunStatus::Landing`. A run inside [`crate::land`]'s post-merge
/// loop used to carry whatever status `merge` set before calling it forward
/// unchanged - `Merged`, even while still watching CI or waiting on the
/// owner's approval - which is also the one status [`RunStatus::resumable`]
/// treats as finished. A daemon that gave this run's slot back to poll
/// something else while an approval was outstanding, or one that simply
/// crashed mid-land, had no way to tell "still landing" from "actually
/// merged" and would either restart the whole competition or leave the run
/// stuck reading as done. A schema-4 record has no notion of `Landing` at
/// all, so this is a meaning a resumed old run cannot be guessed into rather
/// than a value it can default to - hence the bump, not a `#[serde(default)]`.
///
/// 6: a deferred e2e is represented by an empty outcome list plus
/// `ReviewRound::e2e_deferred`. Schema 5 treated that same empty list as an
/// unconfigured, successful check, so schema-5 records are migrated with the
/// old (not-deferred) meaning while older binaries reject schema-6 records.
///
/// 7: added `RunState::gate_ran`. An empty `RunState::gate` used to carry two
/// meanings at once — "never attempted, or the last attempt was
/// resource-blocked" (`graph::Runner::gate`'s retry case) and "attempted,
/// zero commands configured, vacuously passed" (a repo with no
/// `verify.gate`) — and nothing told them apart. `graph::Runner::merge`
/// therefore read the second case as the first and refused forever: a
/// review-only run with no gate commands configured reached `Gating` and
/// then could never leave it. A schema-6 record's non-empty `gate` is
/// migrated to `gate_ran = true` (a recorded attempt, real or historical,
/// should not be spent again); an empty one migrates to `gate_ran = false`
/// and is simply re-attempted by the next `gate()` call, which self-heals
/// instantly for the zero-commands case.
///
/// 8: `ReviewRound::verified_head` used to be `None` for the overwhelming
/// majority of rounds — every ordinary round that ran e2e against its own
/// `head` in the main review loop never set it at all, leaving only the
/// rare catch-up-on-a-different-commit case populated. A reader (a review
/// prompt, `magi show`, the web UI) had no field to ask "which commit did
/// this round's `e2e` actually check" and fell back to assuming it was
/// always `head`, which is also what let a stale round's red output get
/// quoted to a later round's reviewers as if it were about their patch, not
/// an earlier one (see `ReviewRound::verification_summary`, which now exists
/// so nowhere else has to guess). `verified_head` is now set whenever `e2e`
/// held a real attempt (`E2eStatus::Passed`/`Failed`), always naming the
/// commit actually checked instead of only the divergent case, and
/// `ReviewRound::verified_at` is new alongside it. A schema-7 round's own
/// unconditional main-loop check was always against `head` whether or not
/// this field said so, so a `None` with a non-empty `e2e` migrates to
/// `Some(head)` — a reconstruction of a fact that was always true, not a
/// guess. `verified_at` has no historical value to reconstruct and stays
/// `None`, which reads through `verification_summary` as "checked at:
/// unknown" — an honest gap, not a fabricated time.
/// 9: added [`RunState::operator_fixes`] — one record per `magi fix`
/// invocation, routing specific, already-recorded findings to a fixer as a
/// targeted, out-of-band fix outside the normal round sequence. Kept in a
/// channel of its own rather than folded into [`ReviewRound`], because a
/// reviewer's own severity and vote (copied verbatim onto
/// [`OperatorFixFinding`]) must never be rewritten to look like the operator
/// manufactured a blocking verdict — see `graph::Runner::fix_selected`. A
/// schema-8 record has no operator-fix history at all, and
/// `#[serde(default)]` reads an empty list as exactly that: "none happened",
/// not an unknown gap. Nothing about an existing field's meaning changes.
///
/// 10: added `RunStatus::VerifiedNoop` and `Candidate::verified_noop`. Before
/// this, an implementer that correctly concluded (with evidence) that a
/// task's request was already satisfied elsewhere had no way to say so: the
/// run ended the same way as one where every candidate simply failed to
/// write anything — `after_implement` bailing with "no candidate produced a
/// change; nothing to judge" and the run settling as a plain `Failed`. That
/// conflated two very different facts (investigation run 391f's audit is
/// what surfaced it: two attempts that had, correctly, found their fix
/// already on `main`). A schema-9 record has no notion of either the new
/// status or field, so a `VerifiedNoop` value is a meaning that cannot be
/// reconstructed from an old record — hence the bump, not a
/// `#[serde(default)]` for the status. `Candidate::verified_noop` alone
/// *does* default-read as `None` on an old record, which is the honest
/// reading: a run written before this schema never made the claim.
///
/// The report task 391f itself was raised from also named `6c5e`, `8df3` and
/// `e9ce` as three more tasks whose implement wave ended the same
/// diff-zero way, and the investigation traced all three — they do not
/// share one cause.
///
/// `6c5e` and `8df3` are the same already-landed pattern as `391f`, not a
/// coincidence: all three were re-queued together by a same-day audit of
/// `done`-but-unlanded magi tasks (queue talk `20260912-115153-7216`,
/// 2026-09-12 02:51–04:24), which found 17 magi tasks marked `done` with no
/// merge to show for it and re-queued 16 of them, `6c5e` (a fix for the
/// owner's `magi ask --thread` back-and-forth) and `8df3` (release
/// automation) included. A second, same-day audit (talk
/// `20260912-222053-07fe`, 13:20–13:36) then found 12 of those re-queued
/// tasks — `391f`, `6c5e` and `8df3` among them — already merged by another
/// route, and the owner had them deleted (`magi task rm`); `391f` alone
/// survived because a daemon still held its run at the moment of deletion,
/// which is the only reason any record of this group still exists to audit.
/// Quoted directly from that second audit's own turn (talk `07fe`, so this
/// reads without needing access to that talk store), naming both by id:
///
/// > 12件がマージ済み(対応不要)、3件が未実装(妥当)、2件が部分実装(要確認)でした。
/// > **マージ済み → hold/rmを推奨:** 6c5e, 1ddc, fcf5, e25b, cea2, 391f, 3202,
/// > b0a1, 5365, af85, 9f26, 8df3
///
/// — followed by the owner answering "削除！" and the agent confirming "11件
/// 削除完了。391f はいま実行中のdaemonが掴んでいて削除できませんでした."
/// `git log` independently confirms both fixes: the ask-back feature `6c5e`
/// wanted landed as `f0df474` ("let the owner ask back on a question...",
/// #93) on 2026-09-06, and the release-bump automation `8df3` wanted landed
/// as `61005dd`/`bedd925` (open a release-bump PR on merge) on 2026-09-07
/// and `116fcdc` (proportional version bump, #108) on 2026-09-08 — all
/// before the 09-12 requeue. No run record survives the deletion for either
/// task, so this schema's evidence is the audit transcript plus the
/// independently re-checked `git log`, not a `run.json`.
///
/// `e9ce` is not that pattern at all, and is the reason the adoption guard
/// below is all-or-nothing rather than "any candidate said so": its task
/// asked an implementer to merge the real repository's `main` and cut a
/// GitHub release — a destructive, out-of-worktree operation `AGENTS.md`
/// names explicitly as not something to hand to an unattended candidate.
/// Both of its runs (`20260912-053352-49ad`, `20260912-062629-bab1`)
/// correctly refused, filed `magi ask` (questions `6196`, `6c9a`), and ended
/// with an empty diff only because no answer arrived before the implement
/// node's timeout — `49ad` looped `magi ask --wait` in the foreground for
/// roughly 50 minutes as instructed before the timeout cut it off; `bab1`
/// ended its turn moments after filing its question without ever actually
/// blocking on the wait, a separate protocol slip this schema change does
/// not attempt to fix. `49ad`'s own `candidates[0].summary` (quoted here
/// because both records predate schema 10 and, separately, predate a
/// still-unrelated struct change that already makes today's `magi show`
/// refuse to parse either of them — `unknown field 'planner'` — so this is
/// read straight from `run.json` on disk, not through that command):
///
/// > タスクの内容（READY 状態の run を実リポジトリの main に `merge --no-ff`
/// > する、GitHub Release を作る）を精査した結果、これは全てこのワーカーの
/// > worktree の外にある実リポジトリと GitHub 上の共有状態に対する不可逆な
/// > 操作であり […] 私自身の運用ルール「Work only inside this worktree.
/// > Nothing outside it is yours.」と正面から矛盾すると判断しました。
///
/// Neither candidate's reply carries the
/// `NO CHANGE NEEDED` marker below, so both runs correctly stay `Failed`
/// under this schema, not `VerifiedNoop`: a run blocked on an unanswered
/// authorization question is not a verified no-op, and reading the two
/// alike is exactly the misclassification the guard's per-candidate and
/// whole-run conditions exist to refuse.
pub const SCHEMA: u32 = 10;

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
    /// Gate commands running. Transient: `graph::Runner::gate` and
    /// `graph::Runner::merge` always move a run on from here, whether or not
    /// any gate commands are configured — see [`RunState::gate_status`] and
    /// `SCHEMA`'s doc for schema 7, which fixed a repo with an empty
    /// `verify.gate` stranding a review-only run in `Gating` forever.
    Gating,
    /// Inside [`crate::land`]'s post-merge loop: watching CI, running a fix
    /// round, rebasing onto a moved base, or waiting on the owner's merge
    /// approval. A run parked here while an approval is outstanding has
    /// handed its daemon slot back — see [`crate::daemon`] — and resumes
    /// through exactly this status, not a fresh competition.
    Landing,
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
    /// Every candidate wrote nothing, and every one of them said why in a way
    /// that survived [`crate::graph::Runner`]'s adoption guard: a clean CLI
    /// exit, an actually-empty tree, no command left with an unconfirmed
    /// exit status, and non-empty evidence. Distinct from `Failed` on
    /// purpose — see `SCHEMA`'s doc for schema 10 — because the two read
    /// identically to an operator glancing at a card ("nothing happened")
    /// while meaning opposite things: one is an agent that could not do the
    /// work, the other is an agent that checked and the work was already
    /// done. Settles the task through [`crate::queue::Task::handed_off`], not
    /// [`crate::queue::Task::fail`]: a human still has to look — the claim is
    /// unverified by magi itself — and `Held` (not `Failed`-and-requeued)
    /// means nothing retries the task unattended on the same unconfirmed
    /// claim while that look is pending.
    VerifiedNoop,
}

impl RunStatus {
    /// Is this a terminal state?
    pub fn done(self) -> bool {
        matches!(
            self,
            Self::Merged
                | Self::Ready
                | Self::Stalled
                | Self::Blocked
                | Self::Failed
                | Self::VerifiedNoop
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
            Self::Landing => "landing",
            Self::Merged => "merged",
            Self::Ready => "ready",
            Self::Stalled => "stalled",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::VerifiedNoop => "verified_noop",
        }
    }

    /// Label for a human-facing listing or report — the same word as
    /// [`Self::as_str`] except where the machine spelling would read harsher
    /// than the state actually is. `VerifiedNoop` is the one case: its own
    /// `as_str` exists for logs, JSON and event messages, none of which
    /// should quietly grow a second vocabulary, but a bare "verified_noop" in
    /// a report reads like an error code, not the qualified, evidence-backed
    /// claim it actually is.
    pub fn display_label(self) -> &'static str {
        match self {
            Self::VerifiedNoop => "agent-verified no-op",
            other => other.as_str(),
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
    /// answer is a new competition. Nor does `VerifiedNoop`: every candidate
    /// already agreed nothing belongs in this worktree, and resuming would
    /// only re-ask the same question — the answer is for a human to check
    /// the evidence, not for the graph to run again.
    ///
    /// Whether anything is *already* driving the run is a separate question,
    /// answered by `daemon::is_working_on` at the callers that need it.
    pub fn resumable(self) -> bool {
        !matches!(
            self,
            Self::Merged | Self::Ready | Self::Failed | Self::VerifiedNoop
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
    /// The evidence this candidate gave for writing no change on purpose —
    /// the `NO CHANGE NEEDED:` marker `prompt::implement`'s reply format
    /// documents, verbatim. `Some` only when [`crate::graph`]'s adoption
    /// guard accepted the claim: the CLI exited cleanly, the tree really is
    /// empty, no command in the reply was left with an unconfirmed exit
    /// status, and the evidence itself is non-empty. A candidate that wrote
    /// nothing and said nothing about why — the ordinary empty loss — always
    /// reads `None` here, same as one written before schema 10 ever existed.
    #[serde(default)]
    pub verified_noop: Option<String>,
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
    /// Configured judge count — the size of the full panel. `0` when no
    /// panel was asked (see `uncontested`), not the roster size a panel that
    /// never sat would have had.
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
    /// Why no panel was asked, when none was: a single viable candidate, or
    /// a review-only run that never competed. `None` when judges actually
    /// ranked and voted — including when too few of them survived to reach
    /// quorum, which is a collapse and must keep reading as one.
    #[serde(default)]
    pub uncontested: Option<String>,
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
    /// This seat's initial vote. `None` on a record predating votes, exactly
    /// like a round that genuinely had none cast — never a stand-in for a
    /// vote that was lost.
    #[serde(default)]
    pub vote: Option<ReviewVote>,
    /// Why this reviewer produced nothing.
    #[serde(default)]
    pub failed: Option<String>,
    /// Wall-clock time.
    #[serde(default)]
    pub duration_ms: u64,
    /// How many times this seat was asked before it settled — 0 for a first
    /// answer, N after N nudges (`ask_json_wave`'s retry loop). Read this
    /// together with [`Self::failed`], never `failed` alone: `failed: Some(_)`
    /// with `attempts == 0` is a seat that never answered at all, while
    /// `failed: None` with `attempts > 0` is one that only came back after a
    /// nudge — recovered, not silent — and the two must not look the same in
    /// history. A record written before this field existed defaults to `0`,
    /// which under-reports a pre-existing retry rather than inventing one;
    /// see `ask_json_wave`'s own doc for where this is filled in.
    #[serde(default)]
    pub attempts: usize,
}

/// One seat's revote during a round's reconsideration (see
/// [`ReviewRound::reconsideration`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRevoteRecord {
    /// Reviewer seat number, 1-based.
    pub reviewer: usize,
    /// Agent occupying the seat.
    pub agent: String,
    /// The revote. `None` when the seat did not answer.
    #[serde(default)]
    pub vote: Option<ReviewVote>,
    /// Why.
    #[serde(default)]
    pub reason: String,
    /// Why this seat produced no revote.
    #[serde(default)]
    pub failed: Option<String>,
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
    /// How the fixer's own seat was made to answer when its CLI turn ended
    /// cleanly but without an addressed/rejected report — see
    /// [`graph::Runner::continue_fix_report`]. `None` for a record written
    /// before this existed, which must read as "unknown", not as
    /// [`ContinuationOutcome::NotNeeded`]: an old run really may have hit
    /// this exact gap and simply had no mechanism to say so.
    #[serde(default)]
    pub continuation: Option<ContinuationRecord>,
}

/// How a node recovered — or failed to recover — a structured report after
/// the CLI's own turn ended cleanly (a usable, non-empty, exit-0 reply)
/// without it. A clean CLI turn is not the same fact as the node's own work
/// being done — see the `fix` node's `continue_fix_report`, which is what
/// produces this.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContinuationOutcome {
    /// The first reply already carried the report; nothing was resumed.
    NotNeeded,
    /// A follow-up call in the same session recovered the report.
    Resumed,
    /// The continuation budget was spent without ever recovering it.
    Exhausted,
    /// A continuation attempt hit the CLI's rate limit; not retried further
    /// — a quota fails the same way again immediately.
    QuotaLost,
    /// No session was left to resume into, so nothing was attempted.
    NoSession,
}

/// Cost and outcome of one node's attempt to recover a missing report by
/// resuming its own seat.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ContinuationRecord {
    /// Follow-up calls made to the same seat. `0` when the outcome is
    /// [`ContinuationOutcome::NotNeeded`] or [`ContinuationOutcome::NoSession`].
    pub attempts: usize,
    /// Wall-clock time spent on those follow-up calls, summed — not counting
    /// the original call whose reply this is recovering from.
    pub cumulative_wait_ms: u64,
    /// What ended the loop.
    pub outcome: ContinuationOutcome,
}

impl ContinuationRecord {
    /// The report was already there on the first try.
    pub fn not_needed() -> Self {
        Self {
            attempts: 0,
            cumulative_wait_ms: 0,
            outcome: ContinuationOutcome::NotNeeded,
        }
    }
}

/// What happened to one operator-selected finding after the fixer ran, as
/// part of an [`OperatorFixRequest`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorFixOutcome {
    /// The request has not run yet, or never got far enough to report.
    #[default]
    Pending,
    /// The fixer's adoption report named this finding as addressed.
    Addressed,
    /// The fixer's adoption report declined it, with an argument.
    Rejected {
        /// The fixer's own reason.
        why: String,
    },
    /// The fixer never delivered a usable adoption report at all — a
    /// dropped stream, a quota hit, or a continuation budget spent without
    /// recovering one (see `graph::Runner::continue_fix_report`). Distinct
    /// from `Rejected`, which needs an argument this never produced, and
    /// never written back as "addressed" or silently left `Pending` — a
    /// gap in the report is its own outcome, not evidence either way about
    /// the finding.
    Unreported,
}

/// One finding an operator selected for [`OperatorFixRequest`], with the
/// provenance a reviewer originally gave it, copied here verbatim.
///
/// Severity and vote are snapshots, never recomputed and never treated as
/// blocking just because an operator picked the finding — only
/// [`Severity::blocks`] on the original [`ReviewRecord`] decides that. This
/// type exists so an operator's selection is an auditable *addition* to the
/// record, not a rewrite of what a reviewer actually said.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorFixFinding {
    /// Finding id, e.g. `R2-1-3`.
    pub id: String,
    /// Severity as the reviewer recorded it.
    pub severity: Severity,
    /// The reviewer seat's overall vote for the round this finding came
    /// from, if one was cast.
    #[serde(default)]
    pub reviewer_vote: Option<ReviewVote>,
    /// Review round the finding was raised in.
    pub round: usize,
    /// That round's own head — the commit the finding was actually raised
    /// against, used for the freshness check against the branch's current
    /// head at request time.
    pub round_head: String,
    /// Reviewer seat number, 1-based.
    pub reviewer: usize,
    /// Agent occupying that seat.
    pub agent: String,
    /// File the finding concerns.
    #[serde(default)]
    pub file: Option<String>,
    /// Line the finding concerns.
    #[serde(default)]
    pub line: Option<u32>,
    /// One-line summary.
    pub title: String,
    /// The argument.
    #[serde(default)]
    pub detail: String,
    /// What happened to this finding after the fixer ran.
    #[serde(default)]
    pub outcome: OperatorFixOutcome,
}

/// One `magi fix` invocation: the operator's own record of which
/// already-recorded findings they routed to a fixer, why, and what came
/// back. See [`SCHEMA`]'s doc for schema 9 on why this is a channel of its
/// own rather than a field on [`ReviewRound`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorFixRequest {
    /// When `magi fix` was invoked.
    pub requested_at: Timestamp,
    /// The operator's own reasoning. Required and never empty at the CLI —
    /// the audit trail this feature exists for.
    pub reason: String,
    /// The findings selected, each with its own provenance and outcome.
    pub findings: Vec<OperatorFixFinding>,
    /// The branch's head at the moment this request started executing.
    pub head_at_request: String,
    /// Did the operator pass `--allow-stale`?
    pub allow_stale: bool,
    /// Did any selected finding's own `round_head` differ from
    /// `head_at_request`? Kept distinct from `allow_stale` — flipping that
    /// flag does not retroactively make a request that was actually fresh
    /// read as stale, or the reverse.
    pub stale: bool,
    /// The fixer's own attempt, once dispatched.
    #[serde(default)]
    pub fix: Option<FixRecord>,
    /// Head after the fixer's commit, when it produced one.
    #[serde(default)]
    pub result_head: Option<String>,
    /// The review-only run opened to re-verify the change, when one was
    /// actually committed. `None` when nothing changed, so there was
    /// nothing new to re-review — never left implicit as "not gotten to
    /// yet".
    #[serde(default)]
    pub follow_up_review_run: Option<String>,
}

/// A command a seat's own CLI reported running, kept for `magi show` and for
/// telling "this seat's turn ended" apart from "the process it started is
/// done" — see `agent::CommandEvidence`, which is the only source this is
/// ever built from. Never something magi polled or supervised; a command the
/// CLI never reported finishing (or a CLI this crate has no adapter for at
/// all) simply has no entry here, which must read as "unknown", not as
/// "nothing ran".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    /// Graph node the seat belongs to, e.g. `"implement"`, `"fix"`.
    pub node: String,
    /// Review round this job belongs to, for a `"review"`/`"fix"` node —
    /// `None` for every other node, where rounds do not apply, and for every
    /// record written before this was tracked. Lets a reader ask "what did
    /// this seat itself actually run this round", distinct from and never
    /// substituted for magi's own recorded `ReviewRound::e2e` — an absent
    /// entry here means unobserved, not that nothing ran (see this type's
    /// own doc).
    #[serde(default)]
    pub round: Option<usize>,
    /// Seat key, e.g. `"impl-A"`.
    pub seat: String,
    /// The CLI's own id for this command.
    pub id: String,
    /// The command itself, as the CLI reported it.
    pub description: String,
    /// When this evidence was captured — the moment this seat's reply
    /// carrying it was read, not the command's own start time, which no
    /// adapter here currently has. A lower bound on staleness only.
    pub checked_at: Timestamp,
    /// What the CLI reported for it.
    pub status: JobStatus,
    /// Exit code the CLI reported.
    pub exit_code: Option<i32>,
    /// Tail of the command's own output, when reported.
    #[serde(default)]
    pub result_summary: String,
    /// Which CLI/event stream this came from, e.g. `"codex"`.
    pub source: String,
}

/// What a [`JobRecord`]'s own CLI reported for it. There is no `Running`
/// variant: nothing here is ever polled live, so "still running" and
/// "finished but never reported" are the same absence of evidence, not a
/// state this type can name — see [`JobRecord`]'s own doc.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobStatus {
    /// The command's own reported exit code was `0`.
    Completed,
    /// The command's own reported exit code was non-zero.
    Failed,
    /// The CLI reported this command but not a readable exit code.
    Unknown,
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
    /// Set only by magi itself, never inferred from `output_tail`: the
    /// configured command was never actually run because a resource it
    /// needs — right now, only the shared build cache's lease or the
    /// freshness check that must precede using it — was not available
    /// within budget. Distinct from an ordinary failure or timeout (both of
    /// which *did* run something and are evidence about the patch); this is
    /// evidence about the machine, and must never be read as a verdict on
    /// the tree it named. `#[serde(default)]` so every record written
    /// before this field existed keeps reading as `false` — exactly what it
    /// was.
    #[serde(default)]
    pub resource_blocked: bool,
}

/// Substrings that mark a Cargo/rustc/link failure: the toolchain could not
/// produce a binary to run at all, as opposed to producing one that ran and
/// failed. A Windows link race against a shared `CARGO_TARGET_DIR` (see
/// AGENTS.md, "Running magi on magi") looks exactly like a red command
/// otherwise, and a run has concluded `Blocked` on nothing but that race.
const BUILD_FAILURE_MARKERS: &[&str] = &[
    "error: could not compile",
    "error: linking with",
    "LINK : fatal error",
    "fatal error LNK",
];

impl CommandOutcome {
    /// Did it pass?
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Did this command fail because the code could not be built or linked,
    /// rather than because it ran and produced a wrong result? A failure here
    /// is not a verdict on the patch under review.
    pub fn build_failed(&self) -> bool {
        !self.ok()
            && BUILD_FAILURE_MARKERS
                .iter()
                .any(|m| self.output_tail.contains(m))
    }
}

/// One review + verify + fix round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRound {
    /// 1-based round number.
    pub round: usize,
    /// Commit the round reviewed.
    pub head: String,
    /// The commit `e2e` was actually attempted against. Set whenever an
    /// attempt was dispatched (`e2e_status()` reads `Passed`, `Failed`, or
    /// `ResourceBlocked`), naming that commit even when it equals `head` —
    /// never left implicit, because an implicit "must have been `head`" is
    /// exactly what let a later round quote an earlier round's result
    /// without saying which commit it came from. A resource-blocked attempt
    /// still targeted a specific commit even though no command finished, and
    /// leaving that unrecorded is exactly what made a *fresh* blocked
    /// attempt read the same as an untracked one from before schema 8.
    /// `None` only when nothing was attempted at all (`NotConfigured`,
    /// `Deferred`). See `SCHEMA`'s doc for schema 8 for why this broadened
    /// from only the catch-up-on-a-different-commit case.
    #[serde(default)]
    pub verified_head: Option<String>,
    /// When the attempt behind `verified_head` actually ran. `None` on
    /// every record written before schema 8, and on a round where nothing
    /// ran —
    /// both read as "unknown", not as "now" or "never asked".
    #[serde(default)]
    pub verified_at: Option<Timestamp>,
    /// Reviewer reports.
    pub reviews: Vec<ReviewRecord>,
    /// E2E command outcomes for this round.
    #[serde(default)]
    pub e2e: Vec<CommandOutcome>,
    /// True when the first verify attempt this round could not build or
    /// link, and `e2e` above holds a second attempt run before concluding.
    /// A run must never be decided on a red it could not tell from an
    /// unrelated build race.
    #[serde(default)]
    pub verify_retried: bool,
    /// True when `e2e` was intentionally left empty this round: the round
    /// already had blocking findings and another round was available, so
    /// `graph::Runner::review_loop` sent the fixer straight at them instead
    /// of spending a full verify run on a head it already knew would need
    /// another fix. Distinct from an `e2e` that is simply empty because
    /// `verify.e2e` has no commands configured — `e2e.is_empty()` alone
    /// cannot tell those apart, and conflating them is exactly how a
    /// deferred check would get painted green. A record written before this
    /// field existed defaults to `false`, which is the truth for it: every
    /// round used to run e2e unconditionally.
    #[serde(default)]
    pub e2e_deferred: bool,
    /// Why `e2e` was deferred, set only when [`Self::e2e_deferred`] is true.
    /// Carried to the fixer's prompt and shown in the report so "deferred"
    /// never reads as silence.
    #[serde(default)]
    pub e2e_defer_reason: Option<String>,
    /// Fixer response, absent when the round was already clean.
    #[serde(default)]
    pub fix: Option<FixRecord>,
    /// Findings that hold the merge.
    #[serde(default)]
    pub blocking: usize,
    /// Reviewer seats that answered (did not time out, crash, or return
    /// something unparsable).
    #[serde(default)]
    pub answered: usize,
    /// Reviewer seats the round expected an answer from — normally
    /// `graph.reviewers`, but recorded per round so a config change between
    /// runs never has to be inferred from history.
    #[serde(default)]
    pub expected: usize,
    /// Round ended with no blocking findings and green verification, judged
    /// against the seats that answered. See [`Self::incomplete`] for whether
    /// that verdict is missing input.
    #[serde(default)]
    pub clean: bool,
    /// Did the tree actually move against `base` this round, comparing the
    /// diff after the fix to the diff the reviewers saw at the start of the
    /// round?
    ///
    /// Never derived from the fixer's own `addressed`/`rejected` count: that
    /// self-report has been caught lying twice on this workload (runs `b455`
    /// and `6218`, both of which committed a real, substantial diff while
    /// reporting `0 addressed`). `git` does not lie about whether the tree
    /// changed, so this is what `graph::Runner::review_loop` counts rounds of
    /// no progress against. Absent on a round with no fix attempt (already
    /// clean, or the round the budget ran out on), where it defaults to
    /// `false` and is not consulted.
    #[serde(default)]
    pub progressed: bool,
    /// Did the seats' initial votes ([`ReviewRecord::vote`]) disagree?
    #[serde(default)]
    pub vote_split: bool,
    /// One round of revoting, run only when `vote_split`: each seat that cast
    /// an initial vote reads every seat's findings and votes, then revotes.
    /// Empty when the initial votes already agreed, the same as a solo
    /// candidate leaving `deliberation` empty.
    #[serde(default)]
    pub reconsideration: Vec<ReviewRevoteRecord>,
    /// The round's verdict: the most cautious vote among the seats that
    /// answered, using each seat's revote where reconsideration ran and its
    /// initial vote otherwise. `None` when no seat produced a usable vote —
    /// including every record written before votes existed, which is the
    /// truth for those rounds, not a gap in this one.
    #[serde(default)]
    pub verdict: Option<ReviewVote>,
}

impl ReviewRound {
    /// Did at least one reviewer seat fail to answer this round?
    pub fn incomplete(&self) -> bool {
        self.answered < self.expected
    }

    /// The honest state of this round's e2e leg.
    ///
    /// Never derive this from `e2e.is_empty()` alone anywhere else in the
    /// codebase — `NotConfigured` and `Deferred` both leave it empty, and
    /// only this method (backed by [`Self::e2e_deferred`]) tells them apart.
    /// A resource-blocked attempt is checked first and ahead of both: `e2e`
    /// is non-empty for it too, but `CommandOutcome::resource_blocked` says
    /// no command actually ran, and reading that as `Failed` is exactly how
    /// shared build-cache contention gets misreported as a verdict on the
    /// patch (see `CommandOutcome::resource_blocked`'s own doc).
    pub fn e2e_status(&self) -> E2eStatus {
        if self.e2e.iter().any(|o| o.resource_blocked) {
            E2eStatus::ResourceBlocked
        } else if !self.e2e.is_empty() {
            if self.e2e.iter().all(CommandOutcome::ok) {
                E2eStatus::Passed
            } else {
                E2eStatus::Failed
            }
        } else if self.e2e_deferred {
            E2eStatus::Deferred
        } else {
            E2eStatus::NotConfigured
        }
    }

    /// Facts about this round's verification leg, judged against
    /// `current_head` — the commit whoever is asking is actually looking at
    /// right now. `None` when there is nothing worth surfacing: no
    /// `verify.e2e` configured, or the round's own check came back green (a
    /// passing result needs no skepticism attached to it, and an unread
    /// `None` is exactly what keeps a quiet round quiet instead of padding
    /// every prompt with "everything was fine").
    ///
    /// This is the single place that turns `e2e`/`e2e_deferred`/
    /// `verified_head`/`verified_at` into text. Every prompt and report that
    /// shows a round's verification result must build its wording from this,
    /// not re-derive its own summary at the call site — a hand-rolled
    /// version at one more place is exactly how "an old red read as today's
    /// answer" comes back through a different door (see the incident this
    /// type exists to prevent, recorded alongside `SCHEMA`'s doc for schema
    /// 8).
    pub fn verification_summary(&self, current_head: &str) -> Option<VerificationSummary> {
        let status = self.e2e_status();
        if matches!(status, E2eStatus::NotConfigured | E2eStatus::Passed) {
            return None;
        }
        let commit = match &self.verified_head {
            Some(h) if h == current_head => {
                format!("commit {} (this is the head being looked at now)", short(h))
            }
            Some(h) => format!("commit {} (an earlier head, since superseded)", short(h)),
            None => "commit unknown (no command finished checking one)".to_owned(),
        };
        let checked_at = match self.verified_at {
            Some(t) => format!("checked at {t}"),
            None => "checked at: unknown (recorded before this was tracked)".to_owned(),
        };
        let result = match status {
            E2eStatus::NotConfigured | E2eStatus::Passed => unreachable!("checked above"),
            E2eStatus::Failed => "result: FAILED".to_owned(),
            E2eStatus::Deferred => format!(
                "result: not run this round yet — deferred to the fixer{}. Not passed, not \
                 failed.",
                self.e2e_defer_reason
                    .as_deref()
                    .map(|why| format!(" ({why})"))
                    .unwrap_or_default()
            ),
            E2eStatus::ResourceBlocked => "result: could not run — the shared build cache was \
                                            not available. This is evidence about the machine, \
                                            not about the patch."
                .to_owned(),
        };
        let label = format!("round {}, {commit}, {checked_at}\n{result}", self.round);
        // `Failed` names the command that actually ran and failed;
        // `ResourceBlocked` names the operation magi was waiting on (or the
        // freshness check it could not confirm) — `command` still says what
        // was attempted even though nothing finished, and leaving it out is
        // exactly how a reviewer or fixer lost the one thing this leg *can*
        // still tell them: what was being checked, not whether it passed.
        let tail = matches!(status, E2eStatus::Failed | E2eStatus::ResourceBlocked).then(|| {
            self.e2e
                .iter()
                .filter(|o| !o.ok())
                .map(|o| format!("$ {}\n{}\n", o.command, o.output_tail))
                .collect::<String>()
        });
        Some(VerificationSummary { label, tail })
    }
}

/// The honest state of a round's e2e leg. See [`ReviewRound::e2e_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E2eStatus {
    /// `verify.e2e` has no commands configured.
    NotConfigured,
    /// Skipped this round on purpose: blocking findings already required a
    /// fix, so the round went straight to the fixer instead of spending a
    /// full verify run on a head it already knew would need another pass.
    Deferred,
    /// Ran, and every command exited 0.
    Passed,
    /// Ran, and at least one command did not exit 0.
    Failed,
    /// Magi could not even get a command to run — the shared build cache's
    /// lease or freshness check was not available within budget. Evidence
    /// about the machine, never a verdict on the tree it named; must not be
    /// shown or counted the same as [`Self::Failed`].
    ResourceBlocked,
}

/// [`ReviewRound::verification_summary`]'s output: the facts, pre-worded, for
/// a prompt or report to place under its own heading. Kept as two pieces
/// rather than one pre-joined string so a caller that wants to insert its own
/// note between the label and the raw command tail (see `prompt::review`) can
/// do so without re-parsing text back apart.
#[derive(Debug, Clone)]
pub struct VerificationSummary {
    /// Round, commit, freshness and result — always present.
    pub label: String,
    /// Raw `$ command` / output tail, present for `result: FAILED` and for
    /// a resource-blocked attempt (naming the operation magi was waiting on,
    /// even though nothing finished) — absent for every other result, which
    /// has nothing to add past the label.
    pub tail: Option<String>,
}

/// The honest state of a run's final gate. See [`RunState::gate_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateStatus {
    /// Never attempted, or the last attempt was resource-blocked (the shared
    /// build cache could not be acquired or confirmed fresh in time) and
    /// needs a retry.
    NotRun,
    /// Ran with zero commands configured (`verify.gate` is empty) and
    /// therefore vacuously passed — there was nothing to check.
    PassedWithNoCommands,
    /// Ran one or more commands, and every one of them exited 0.
    Passed,
    /// Ran one or more commands, and at least one did not exit 0.
    Failed,
}

impl GateStatus {
    /// May a run in this state proceed to merge?
    pub fn ok(self) -> bool {
        matches!(self, Self::PassedWithNoCommands | Self::Passed)
    }
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

/// A seat currently mid-answer: a prompt was sent and no reply has landed yet.
///
/// This is not the whole story of "is it alive" — a daemon killed mid-wave
/// leaves its last wave's entries here forever, since nothing ran to clear
/// them. A reader must cross-check a live daemon's heartbeat
/// (`daemon::is_working_on`) before trusting one of these as "still running"
/// rather than "abandoned". [`RunState::clear_active`] is what keeps that
/// leftover from surviving into the next attempt at this run: `execute` calls
/// it before doing anything else, so a resumed run never carries a stale
/// entry into its own report before the next wave repopulates it.
///
/// Deliberately carries no agent id: an implementer's agent is no secret, but
/// a judge or reviewer seat is blind (`SeatState::key` is keyed by seat, never
/// agent, for exactly this reason), and this struct has no way to tell which
/// kind of seat it describes. The seat key alone — already in the map this
/// lives under — is what every caller needs to say which seat is running.
///
/// The same map also carries entries for shell-command work that runs
/// outside any seat — `verify.e2e`, `verify.gate` — keyed by the task's own
/// name (`"e2e"`, `"gate"`) rather than a seat key. [`Self::task`] is `Some`
/// only for those; it is how a reader tells the two kinds of entry apart
/// without a second map, a second route, or a second SSE reason to poll for
/// — see [`RunState::seats_active`] / [`RunState::tasks_active`] for the
/// accessors that split them back apart. A task entry is exactly as blind as
/// a seat entry: no agent runs it, so there is nothing to leak, and
/// [`Self::command`] carries only the shell command being run, never
/// anything about who is running it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSeat {
    /// Node the seat is answering for, e.g. `implement`, `judge`, `review`.
    /// For a task entry, the node the command list runs under (`verify`,
    /// `gate`).
    pub node: String,
    /// When this attempt — or, for a task entry, this one command — was
    /// started. A task entry's timer resets at every command boundary,
    /// because `verify.e2e` / `verify.gate` apply their timeout per command,
    /// not once across the whole list — see [`RunState::task_command`].
    pub started_at: Timestamp,
    /// The wall-clock budget for this attempt (a seat) or this one command
    /// (a task entry).
    pub timeout_secs: u64,
    /// 0 for the first ask, N for the Nth nudge or resume. For a task entry,
    /// 0 for the first pass over the command list, N for the Nth retry (see
    /// `run_e2e_with_retry`'s build/link retry).
    #[serde(default)]
    pub attempt: usize,
    /// `None` for a seat; `Some("e2e")` / `Some("gate")` for a running
    /// command-list task. This is the type tag that lets both kinds of entry
    /// share one map without a task ever being mistaken for a (blind) seat —
    /// see this struct's own doc. Omitted from JSON when absent (the common,
    /// seat case), rather than written out as a literal `null` on every one
    /// of a run's seat entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The command currently running, task entries only. Never set on a
    /// seat entry — a seat has no command, only a prompt, and a prompt is
    /// not safe to show mid-run (see this struct's blindness note).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// 1-based position of [`Self::command`] within the task's command list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// Number of commands in the task's list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

impl ActiveSeat {
    /// Seconds since this attempt was sent.
    #[must_use]
    pub fn elapsed_secs(&self, now: Timestamp) -> i64 {
        (now.as_second() - self.started_at.as_second()).max(0)
    }

    /// Seconds left before this attempt's own timeout fires, floored at zero
    /// rather than going negative once the CLI has overrun its budget.
    #[must_use]
    pub fn remaining_secs(&self, now: Timestamp) -> i64 {
        (self.timeout_secs as i64 - self.elapsed_secs(now)).max(0)
    }
}

/// Whether a process is provably still driving a run, provably not, or
/// neither — see [`RunState::liveness`]. Serialized as a lowercase string
/// (`"live"` / `"dead"` / `"unknown"`) rather than a bool: a bool has no room
/// for "could not tell", and folding that case into either `true` or `false`
/// is exactly the wrong call for a display an operator uses to decide
/// whether to wait or to act — see the schema-10 field doc on
/// [`RunState::driver_pid`] for the report it used to produce instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Liveness {
    /// Proven: a daemon's heartbeat claims the run, or `driver_pid` answers
    /// alive.
    Live,
    /// Proven: no daemon claim, and `driver_pid` answers dead.
    Dead,
    /// Neither proven — no daemon claim, and either no `driver_pid` to ask or
    /// the platform could not answer for the one recorded. Never treated as
    /// `Dead`: see [`RunState::liveness_with`].
    Unknown,
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

/// How far the winner's tree trailed the landing base, last time it was
/// checked, and what came of trying to close that gap.
///
/// Set by `graph::Runner::sync_to_base`, which runs before the review loop and
/// again before the gate: verifying against a tree that does not yet contain
/// the base's tip answers "green on the commit this run branched from", not
/// "green on what is about to land", and a merge on that answer can revert
/// whatever landed elsewhere while the run was thinking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseSync {
    /// `<remote>/<base>` tip the tree was last checked against.
    pub tip: String,
    /// Commits `tip` was ahead of the tree at that check, before any rebase
    /// this round tried to close the gap. Zero means the tree already
    /// contained `tip`.
    pub behind: usize,
    /// Rebase attempts spent so far this run, bounded by
    /// `graph::BASE_SYNC_ROUNDS`.
    pub attempts: usize,
    /// What git said, if the most recent rebase attempt conflicted or could
    /// not be pushed. `Some` here is what makes a `Blocked` run read as
    /// "stopped on the base, not on review or the gate" - the rebase is not
    /// retried again while this is set; a person has to look.
    #[serde(default)]
    pub conflict: Option<String>,
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
    /// Did this run take a reference on `extensions.worktreeConfig` being on
    /// (see [`crate::git::acquire_worktree_config`])? If so, cleanup releases
    /// it - which only actually turns the setting back off once every other
    /// run sharing this repository has released its own reference too.
    #[serde(default)]
    pub enabled_worktree_config: bool,
    /// Candidates.
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    /// Initial blind rankings.
    #[serde(default)]
    pub judgements: Vec<Judgement>,
    /// `judge` decided a solo candidate needs no panel and only logged it.
    ///
    /// `judgements` stays empty in that case — nothing to distinguish from
    /// "not yet judged" — so this is the record that makes the skip
    /// idempotent: without it, every reentry re-ran `judge`, re-logged the
    /// same event, and rewrote `status` to `Judging` over whatever a later
    /// node had already concluded.
    #[serde(default)]
    pub judge_skipped: bool,
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
    ///
    /// Never derive whether the gate has run from `gate.is_empty()` alone —
    /// use [`Self::gate_status`] instead. An empty list is ambiguous on its
    /// own: it is what an unattempted gate looks like, what a
    /// resource-blocked attempt leaves behind (see `graph::Runner::gate`'s
    /// own doc), and also what a repo with no `verify.gate` commands
    /// configured produces once it *has* run. [`Self::gate_ran`] is what
    /// tells the third case apart from the first two.
    #[serde(default)]
    pub gate: Vec<CommandOutcome>,
    /// Did `gate()` actually record an attempt — zero commands configured
    /// and vacuously passed, or one or more commands that ran to
    /// completion — as opposed to never having run, or having last hit a
    /// resource-blocked retry?
    ///
    /// `gate.is_empty()` cannot tell those apart by itself: a repo with no
    /// `verify.gate` commands leaves `gate` empty exactly like an
    /// unattempted or resource-blocked one does, and reading that empty list
    /// as "not yet run" is what stranded a review-only run in
    /// `RunStatus::Gating` forever on such a repo — see `SCHEMA`'s doc for
    /// schema 7. A record written before this field existed defaults to
    /// `false` and is migrated in [`migrate_schema`].
    #[serde(default)]
    pub gate_ran: bool,
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
    /// Seats currently mid-answer, keyed by seat.
    ///
    /// An entry exists from the moment a prompt is sent until a reply (of any
    /// kind — success, failure, quota, drop) comes back, so its keys are
    /// exactly "who hasn't answered yet" for whichever node populated it. See
    /// [`ActiveSeat`] for why a reader still has to check a live daemon
    /// before trusting one of these as "running" rather than "abandoned".
    #[serde(default)]
    pub active: BTreeMap<String, ActiveSeat>,
    /// Process id of whichever `execute()` call last drove this run —
    /// written at the very top of that method, the same place
    /// [`Self::clear_active`] runs, so a fresh reentry always overwrites the
    /// pid a previous, possibly-dead process left behind.
    ///
    /// A daemon-claimed run already has a stronger signal
    /// (`daemon::is_working_on`), but a `magi run` / `magi review` typed
    /// straight into a terminal claims nothing there — before this field
    /// existed, [`report::active_seats`] had no way to tell that run apart
    /// from one a killed process abandoned, and printed the same "no live
    /// daemon claims this run" warning over a run that was, in fact, still
    /// answering. See [`Liveness`] for how this and the daemon claim combine.
    #[serde(default)]
    pub driver_pid: Option<u32>,
    /// Last observation of the winner's pull request, when a land loop ran.
    ///
    /// Persisted rather than derived from the event log because the phone asks
    /// two questions about a run that has opened a PR - how are its checks and
    /// which round is it on - and parsing prose out of events to answer them
    /// would break the first time an event message was reworded.
    #[serde(default)]
    pub pr: Option<PrRecord>,
    /// The last look at how far the winner's tree trailed the landing base,
    /// and the rebase(s) tried to close that gap. `None` until the tree has a
    /// winner to check.
    #[serde(default)]
    pub base_sync: Option<BaseSync>,
    /// The design-deliberation stage's output, when `[graph] advise` ran it:
    /// one record per advisor seat, plus the synthesis blended into the
    /// implementer's prompt. `None` when the stage is off, has not run yet,
    /// or could not even resolve its seats - see
    /// [`crate::graph::Runner::advise`].
    #[serde(default)]
    pub advice: Option<crate::advise::Advice>,
    /// Whether the design-deliberation stage has already been attempted this
    /// run, whatever it produced. The idempotency marker `Runner::advise`
    /// checks on reentry, the same role [`Self::judge_skipped`] plays for
    /// `judge` - without it a resumed run whose stage failed (a misconfigured
    /// `[roles] advisors`, every seat quota'd) would re-run it, and re-spend
    /// the agent calls, on every single reentry before `implement`.
    #[serde(default)]
    pub advise_attempted: bool,
    /// Node log.
    #[serde(default)]
    pub events: Vec<Event>,
    /// Commands seats' own CLIs reported running, across every node — see
    /// [`JobRecord`]. Populated in [`crate::graph::wave`] as each seat
    /// answers, so a resumed run keeps what earlier waves already collected
    /// rather than losing it to a reentry. Empty on a record written before
    /// this existed, or wherever no adapter reads structured job events for
    /// the backend a seat used — both read as "no evidence", not "nothing
    /// ran".
    #[serde(default)]
    pub jobs: Vec<JobRecord>,
    /// Operator-triggered targeted fixes — see [`OperatorFixRequest`] and
    /// `SCHEMA`'s doc for schema 9. Empty on every record written before
    /// this existed, which reads correctly as "no operator fix ever
    /// requested".
    #[serde(default)]
    pub operator_fixes: Vec<OperatorFixRequest>,
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
            judge_skipped: false,
            deliberation: Vec::new(),
            votes: Vec::new(),
            tally: None,
            reviews: Vec::new(),
            gate: Vec::new(),
            gate_ran: false,
            merge: None,
            leaks: Vec::new(),
            quota: Vec::new(),
            parked: false,
            seats: BTreeMap::new(),
            active: BTreeMap::new(),
            driver_pid: None,
            pr: None,
            base_sync: None,
            advice: None,
            advise_attempted: false,
            events: Vec::new(),
            jobs: Vec::new(),
            operator_fixes: Vec::new(),
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

    /// The honest state of the final gate.
    ///
    /// Never derive this from `gate.is_empty()` alone anywhere else in the
    /// codebase — `NotRun` and `PassedWithNoCommands` both leave `gate`
    /// empty, and only this method (backed by [`Self::gate_ran`]) tells them
    /// apart. See `SCHEMA`'s doc for schema 7 for what conflating them used
    /// to do.
    pub fn gate_status(&self) -> GateStatus {
        if !self.gate_ran {
            GateStatus::NotRun
        } else if self.gate.is_empty() {
            GateStatus::PassedWithNoCommands
        } else if self.gate.iter().all(CommandOutcome::ok) {
            GateStatus::Passed
        } else {
            GateStatus::Failed
        }
    }

    /// Record that `seat` was just sent a prompt for `node`, with the given
    /// wall-clock budget. `attempt` is 0 for the first ask and N for the Nth
    /// nudge or resume, purely for display — it does not change how the seat
    /// is treated.
    pub fn seat_started(
        &mut self,
        node: &str,
        seat: &str,
        timeout: std::time::Duration,
        attempt: usize,
    ) {
        self.active.insert(
            seat.to_owned(),
            ActiveSeat {
                node: node.to_owned(),
                started_at: Timestamp::now(),
                timeout_secs: timeout.as_secs(),
                attempt,
                task: None,
                command: None,
                index: None,
                total: None,
            },
        );
    }

    /// Record that `seat` has answered, whatever the answer was.
    pub fn seat_finished(&mut self, seat: &str) {
        self.active.remove(seat);
    }

    /// Record that `task` (`"e2e"` or `"gate"` — a shell-command list run
    /// outside any seat) has just started `command`, the `index`-th of
    /// `total`. Called at every command boundary, not once for the whole
    /// list: `verify.e2e` / `verify.gate` apply `timeout` per command, so
    /// this is the only way a reader can tell "how long is left" for
    /// whichever command is actually running right now, rather than a stale
    /// budget left over from the first one.
    #[allow(clippy::too_many_arguments)]
    pub fn task_command(
        &mut self,
        task: &str,
        node: &str,
        attempt: usize,
        command: &str,
        index: usize,
        total: usize,
        timeout: std::time::Duration,
    ) {
        self.active.insert(
            task.to_owned(),
            ActiveSeat {
                node: node.to_owned(),
                started_at: Timestamp::now(),
                timeout_secs: timeout.as_secs(),
                attempt,
                task: Some(task.to_owned()),
                command: Some(command.to_owned()),
                index: Some(index),
                total: Some(total),
            },
        );
    }

    /// Record that `task` has finished its whole command list for this
    /// attempt.
    pub fn task_finished(&mut self, task: &str) {
        self.active.remove(task);
    }

    /// The seats — never task entries — currently mid-answer. What
    /// `report::active_seats` and the phone's "who has not answered yet"
    /// note need: a seat's identifier is safe to show ([`ActiveSeat`]'s doc),
    /// so nothing here filters anything out beyond the type tag itself.
    pub fn seats_active(&self) -> impl Iterator<Item = (&String, &ActiveSeat)> {
        self.active.iter().filter(|(_, a)| a.task.is_none())
    }

    /// The command-list tasks — never seat entries — currently running.
    /// Counterpart to [`Self::seats_active`]; see [`ActiveSeat::task`] for
    /// the tag both read.
    pub fn tasks_active(&self) -> impl Iterator<Item = (&String, &ActiveSeat)> {
        self.active.iter().filter(|(_, a)| a.task.is_some())
    }

    /// Drop every seat this state still lists as answering, reporting whether
    /// anything was dropped.
    ///
    /// Called first thing in `execute`, on every entry — fresh, resumed, or
    /// recovering a stall — because an entry here only means something while
    /// the process that wrote it is still asking that seat something. A
    /// process killed mid-wave leaves its last batch of seats here with
    /// nobody left to clear them, and the next process to touch this run must
    /// not let that leftover read as "still going" before it has asked
    /// anyone anything.
    pub fn clear_active(&mut self) -> bool {
        if self.active.is_empty() {
            return false;
        }
        self.active.clear();
        true
    }

    /// Does every seat this run still lists as [`Self::active`] sit past its
    /// own [`ActiveSeat::timeout_secs`]? `false` when nothing is active at
    /// all — an empty map is not evidence of anything overrunning.
    ///
    /// This alone is not proof the run is dead: a seat's own attempt can
    /// legitimately run a little past its budget while the process driving it
    /// is still tearing the attempt down. Every caller pairs this with its own
    /// `!live` reading (`daemon::is_working_on`) before treating the run as
    /// abandoned — this module cannot check that itself without depending on
    /// `crate::daemon`, and callers already have to ask that question anyway.
    #[must_use]
    pub fn active_all_overrun(&self, now: Timestamp) -> bool {
        !self.active.is_empty()
            && self
                .active
                .values()
                .all(|a| a.elapsed_secs(now) > a.timeout_secs as i64)
    }

    /// Whether a process is actually still driving this run, given whether a
    /// daemon's heartbeat claims it and a process-liveness query for
    /// [`Self::driver_pid`].
    ///
    /// A daemon claim wins outright when present — it is the stronger,
    /// independently-heartbeating signal. Absent that (every manual `magi
    /// run` / `magi review`, and every daemon-driven run whose daemon has
    /// since exited cleanly), `driver_pid` is asked directly. `query` never
    /// collapses an unavailable answer to either extreme: no `driver_pid` at
    /// all (an old run, or a schema older than this field), a pid this build
    /// cannot query, or a query that comes back inconclusive, all read as
    /// [`Liveness::Unknown`] — never [`Liveness::Dead`]. A display that
    /// guessed "dead" out of missing information would be exactly the
    /// mtime-and-task-manager guessing this type exists to replace.
    ///
    /// Kept generic over `query` so a test can inject an answer without
    /// spawning a real process query — production code goes through
    /// [`Self::liveness`], which supplies [`crate::proc::pid_status`].
    #[must_use]
    pub fn liveness_with<F>(&self, daemon_claims: bool, query: F) -> Liveness
    where
        F: FnOnce(u32) -> Option<bool>,
    {
        if daemon_claims {
            return Liveness::Live;
        }
        match self.driver_pid {
            None => Liveness::Unknown,
            Some(pid) => match query(pid) {
                Some(true) => Liveness::Live,
                Some(false) => Liveness::Dead,
                None => Liveness::Unknown,
            },
        }
    }

    /// [`Self::liveness_with`], backed by the real process-liveness query.
    #[must_use]
    pub fn liveness(&self, daemon_claims: bool) -> Liveness {
        self.liveness_with(daemon_claims, crate::proc::pid_status)
    }

    /// Clear every seat this run still lists as active and fail it, unless it
    /// had already reached a terminal status some other way.
    ///
    /// Callers must already have proven this run is dead — [`Self::active_all_overrun`]
    /// plus their own `!live` reading — before calling this; it does not
    /// check either itself. Unlike [`Self::clear_active`] (dropping a resumed
    /// run's own stale wave before repopulating it, called unconditionally at
    /// the top of every `execute()`), this is a verdict: a run left this way
    /// has nothing left to repopulate the wave, ever, and must stop reading as
    /// `implementing` (or whichever node) forever.
    pub fn abandon(&mut self, by: &str) {
        let seats: Vec<String> = self.active.keys().cloned().collect();
        self.clear_active();
        if !self.status.done() {
            self.status = RunStatus::Failed;
        }
        self.event(
            by,
            format!(
                "abandoned: seat(s) {} left behind by a killed process, past their own \
                 timeout with no live daemon claiming this run",
                seats.join(", ")
            ),
        );
    }

    /// Flush to `run.json`, atomically, under the process-global [`home`].
    pub fn save(&mut self) -> Result<()> {
        let home = home();
        self.save_under(&home)
    }

    /// [`Self::save`], rooted at an explicit `home` instead of the
    /// process-global one.
    ///
    /// For a caller that was already handed its own `home` explicitly — a
    /// housekeeping pass, mainly, for the same reason `Queue::at` and the
    /// daemon status path are parameters rather than resolved here (see
    /// `daemon::drive`'s own doc) — falling through to the global would write
    /// back through whichever directory some *other* process or test pinned
    /// into that `OnceLock` first, not the one this call was actually handed.
    pub fn save_under(&mut self, home: &Path) -> Result<()> {
        self.updated_at = Timestamp::now();
        let dir = home.join("runs").join(&self.id);
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
        migrate_schema(state)
    }
}

fn migrate_schema(mut state: RunState) -> Result<RunState> {
    // Schema 5 predates deferred e2e. Its empty e2e lists therefore mean
    // "not configured", never "deferred"; serde's field defaults retain
    // exactly that representation while this migration permits resumes.
    if state.schema == 5 {
        state.schema = 6;
    }
    // Schema 6 predates `gate_ran` and could not tell "never attempted or
    // resource-blocked" apart from "ran with zero commands configured" — see
    // `SCHEMA`'s doc for schema 7. A non-empty `gate` is a real recorded
    // attempt either way, so it is trusted as `gate_ran = true` rather than
    // spent again; an empty one is simply handed back to the next `gate()`
    // call, which re-attempts it and, for the zero-commands case, resolves
    // instantly.
    if state.schema == 6 {
        state.gate_ran = !state.gate.is_empty();
        state.schema = 7;
    }
    // Schema 7's main review loop always checked `e2e` against the round's
    // own `head`, it just never wrote that fact into `verified_head` unless
    // a catch-up run had checked a *different* commit — see `SCHEMA`'s doc
    // for schema 8. Reconstructing `Some(head)` for a round whose `e2e` held
    // a real attempt restores a fact that was always true; `verified_at` has
    // no historical value to recover and stays `None`.
    if state.schema == 7 {
        for round in &mut state.reviews {
            if round.verified_head.is_none()
                && matches!(round.e2e_status(), E2eStatus::Passed | E2eStatus::Failed)
            {
                round.verified_head = Some(round.head.clone());
            }
        }
        state.schema = 8;
    }
    // Schema 8 predates `operator_fixes`. There is nothing to reconstruct —
    // an old run simply never had one requested — so `#[serde(default)]`
    // already left it as the correct empty `Vec`; this only advances the
    // version number.
    if state.schema == 8 {
        state.schema = 9;
    }
    // Schema 9 predates `RunStatus::VerifiedNoop` and
    // `Candidate::verified_noop`. Nothing to reconstruct: an old record never
    // made the claim, `#[serde(default)]` already reads `verified_noop` as
    // `None` on every candidate, and a `VerifiedNoop` status cannot appear in
    // a schema-9 record at all — see `SCHEMA`'s doc for schema 10. This only
    // advances the version number.
    if state.schema == 9 {
        state.schema = SCHEMA;
    }
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

impl RunState {
    /// The winning candidate, once the tally has run.
    pub fn winner(&self) -> Option<&Candidate> {
        let label = self.tally.as_ref()?.winner;
        self.candidates.iter().find(|c| c.label == label)
    }

    /// Candidates eligible for judging.
    pub fn viable(&self) -> Vec<&Candidate> {
        self.candidates.iter().filter(|c| c.viable()).collect()
    }

    /// Did every candidate write nothing, and every one of them back it with
    /// evidence [`crate::graph`]'s adoption guard accepted?
    ///
    /// All-or-nothing on purpose: one candidate declaring `NO CHANGE NEEDED`
    /// while another simply failed to produce anything is not agreement, it
    /// is one candidate's unverified claim next to an ordinary loss, and the
    /// run must still read as the `Failed` it is. Only ever meaningful when
    /// [`Self::viable`] is already empty — a run with any real patch to judge
    /// never reaches the caller that asks this.
    pub fn all_candidates_verified_noop(&self) -> bool {
        !self.candidates.is_empty()
            && self
                .candidates
                .iter()
                .all(|c| c.empty && c.verified_noop.is_some())
    }

    /// Findings still open when the review loop stopped trying: the last
    /// round's, exactly when that round was not clean. Empty on a run that
    /// never reviewed, or whose last round was clean.
    ///
    /// This is the last round's findings regardless of what the fixer claims
    /// to have addressed in that same round: a round that stopped the loop
    /// (round budget spent, or no tree progress for
    /// [`crate::graph::STAGNANT_LIMIT`] rounds) never had a *following* round
    /// to confirm the fix actually landed, and the self-reported adoption
    /// count is not trusted for that judgement either — see
    /// [`ReviewRound::progressed`].
    pub fn open_findings(&self) -> Vec<&Finding> {
        match self.reviews.last() {
            Some(r) if !r.clean => r
                .reviews
                .iter()
                .flat_map(|rec| rec.findings.iter())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Every finding raised in the most recent review round, regardless of
    /// that round's own severity mix — unlike [`Self::open_findings`], not
    /// filtered to a round that was not clean. This is the pool `magi fix`
    /// reports as available to pick from: a round can conclude clean (no
    /// finding blocked merge) while still carrying minor findings nobody
    /// has acted on.
    pub fn last_round_findings(&self) -> Vec<&Finding> {
        self.reviews
            .last()
            .into_iter()
            .flat_map(|r| r.reviews.iter())
            .flat_map(|rec| rec.findings.iter())
            .collect()
    }

    /// Look up a finding by id anywhere in this run's review history,
    /// together with the round and reviewer record that raised it — the
    /// provenance `magi fix` snapshots onto [`OperatorFixFinding`].
    pub fn finding(&self, id: &str) -> Option<(&ReviewRound, &ReviewRecord, &Finding)> {
        self.reviews.iter().find_map(|round| {
            round.reviews.iter().find_map(|rec| {
                rec.findings
                    .iter()
                    .find(|f| f.id == id)
                    .map(|f| (round, rec, f))
            })
        })
    }

    /// Did this run reach a mergeable status (`Ready` or `Merged`) with
    /// review findings still open?
    ///
    /// That combination is the point of the review hand-off: the review
    /// round budget (or an unproductive round, see [`ReviewRound::progressed`])
    /// was spent while gate and e2e stayed green, so the run was handed off
    /// rather than blocked — but the findings did not disappear, and whoever
    /// reads the result should be told they are still there.
    pub fn handed_off_with_open_findings(&self) -> bool {
        matches!(self.status, RunStatus::Ready | RunStatus::Merged)
            && self.reviews.last().is_some_and(|r| !r.clean)
    }

    /// Reached `Ready` because `[merge] mode = "none"` left it there by
    /// design, never to be picked up by the PR-polling merge watcher — as
    /// opposed to a `Ready` that is still a plausible landing candidate (a
    /// PR closed without merging, or a re-entry onto an already-concluded
    /// node). Both leave `status` at `Ready`; only this one leaves the
    /// winning branch permanently unwatched, which is what a caller needs to
    /// know before labelling the run in a listing.
    pub fn unmerged_by_design(&self) -> bool {
        self.status == RunStatus::Ready
            && self
                .merge
                .as_ref()
                .is_some_and(|m| m.mode == MergeMode::None)
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

/// The short form of a commit, for a label a human or an LLM reads.
fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
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

    fn overrun_seat(now: Timestamp, elapsed_secs: i64, timeout_secs: u64) -> ActiveSeat {
        ActiveSeat {
            node: "implement".to_owned(),
            started_at: now - jiff::SignedDuration::new(elapsed_secs, 0),
            timeout_secs,
            attempt: 0,
            task: None,
            command: None,
            index: None,
            total: None,
        }
    }

    #[test]
    fn active_all_overrun_requires_every_seat_past_its_own_timeout() {
        let mut s = state();
        let now = Timestamp::now();
        assert!(
            !s.active_all_overrun(now),
            "nothing active is not evidence of anything"
        );

        s.active
            .insert("impl-A".to_owned(), overrun_seat(now, 21_000, 3_600));
        assert!(
            s.active_all_overrun(now),
            "21000s elapsed against a 3600s budget"
        );

        // A seat still well within its own budget means the run is not
        // provably dead, however far its sibling has overrun.
        s.active
            .insert("impl-B".to_owned(), overrun_seat(now, 0, 3_600));
        assert!(!s.active_all_overrun(now));
    }

    /// A daemon claim wins outright, whatever `driver_pid` or the query says
    /// — the stronger, independently-heartbeating signal.
    #[test]
    fn liveness_reads_live_from_a_daemon_claim_alone() {
        let mut s = state();
        s.driver_pid = None;
        assert_eq!(
            s.liveness_with(true, |_| None),
            Liveness::Live,
            "a daemon claim needs no pid to back it up"
        );
    }

    /// The gap `driver_pid` closes: no daemon claim (every manual `magi run`
    /// / `magi review`), but the recorded pid answers alive.
    #[test]
    fn liveness_reads_live_from_a_confirmed_pid_without_a_daemon_claim() {
        let mut s = state();
        s.driver_pid = Some(4242);
        assert_eq!(
            s.liveness_with(false, |pid| {
                assert_eq!(pid, 4242);
                Some(true)
            }),
            Liveness::Live
        );
    }

    /// No daemon claim and the recorded pid confirmed gone: the only case
    /// that reads as provably dead.
    #[test]
    fn liveness_reads_dead_only_from_a_confirmed_dead_pid() {
        let mut s = state();
        s.driver_pid = Some(4242);
        assert_eq!(s.liveness_with(false, |_| Some(false)), Liveness::Dead);
    }

    /// Missing information never collapses to `Dead`: an old run with no
    /// `driver_pid` at all, and a `driver_pid` this build could not query,
    /// both read as `Unknown` — never a guess.
    #[test]
    fn liveness_never_guesses_dead_out_of_missing_information() {
        let mut s = state();
        s.driver_pid = None;
        assert_eq!(
            s.liveness_with(false, |_| panic!("no pid to query")),
            Liveness::Unknown,
            "no driver_pid recorded at all — an old run predating this field"
        );

        s.driver_pid = Some(4242);
        assert_eq!(
            s.liveness_with(false, |_| None),
            Liveness::Unknown,
            "a pid to ask, but the platform could not answer for it"
        );
    }

    #[test]
    fn abandon_clears_active_and_fails_a_non_terminal_run() {
        let mut s = state();
        s.status = RunStatus::Implementing;
        let now = Timestamp::now();
        s.active
            .insert("impl-A".to_owned(), overrun_seat(now, 21_000, 3_600));

        s.abandon("daemon");

        assert!(s.active.is_empty());
        assert_eq!(s.status, RunStatus::Failed);
        assert!(
            s.events
                .last()
                .expect("an event was logged")
                .message
                .contains("impl-A"),
            "the event names the abandoned seat"
        );
    }

    #[test]
    fn abandon_never_overwrites_a_status_already_terminal() {
        let mut s = state();
        s.status = RunStatus::Ready;
        let now = Timestamp::now();
        s.active
            .insert("impl-A".to_owned(), overrun_seat(now, 21_000, 3_600));

        s.abandon("daemon");

        assert!(s.active.is_empty());
        assert_eq!(
            s.status,
            RunStatus::Ready,
            "a run already done must not be relabelled Failed"
        );
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
            verified_noop: None,
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
    fn build_failure_is_distinguished_from_a_failing_test() {
        let link_race = CommandOutcome {
            command: "cargo test".to_owned(),
            code: Some(1),
            output_tail: "LINK : fatal error LNK1104: cannot open file \
                          'graph_dirty_tree-71d4dc8e.exe'\n\
                          error: could not compile `magi-cli` (test \"graph_dirty_tree\")"
                .to_owned(),
            duration_ms: 500,
            resource_blocked: false,
        };
        assert!(!link_race.ok());
        assert!(link_race.build_failed());

        let failing_test = CommandOutcome {
            command: "cargo test".to_owned(),
            code: Some(101),
            output_tail: "thread 'it_works' panicked at 'assertion failed'".to_owned(),
            duration_ms: 500,
            resource_blocked: false,
        };
        assert!(!failing_test.ok());
        assert!(
            !failing_test.build_failed(),
            "a real test failure must not be classed as a build failure"
        );

        let passing = CommandOutcome {
            command: "cargo test".to_owned(),
            code: Some(0),
            output_tail: String::new(),
            duration_ms: 500,
            resource_blocked: false,
        };
        assert!(passing.ok());
        assert!(!passing.build_failed());
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

    fn finding(id: &str, severity: crate::verdict::Severity) -> crate::verdict::Finding {
        crate::verdict::Finding {
            id: id.to_owned(),
            severity,
            file: None,
            line: None,
            title: "x".to_owned(),
            detail: String::new(),
        }
    }

    fn round(clean: bool, findings: Vec<crate::verdict::Finding>) -> ReviewRound {
        ReviewRound {
            round: 1,
            head: "h".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![ReviewRecord {
                attempts: 0,
                reviewer: 1,
                agent: "a".to_owned(),
                summary: String::new(),
                findings,
                vote: None,
                failed: None,
                duration_ms: 0,
            }],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 1,
            expected: 1,
            clean,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }
    }

    #[test]
    fn e2e_status_tells_deferred_apart_from_not_configured() {
        let mut r = round(false, Vec::new());
        assert_eq!(r.e2e_status(), E2eStatus::NotConfigured);

        r.e2e_deferred = true;
        assert_eq!(
            r.e2e_status(),
            E2eStatus::Deferred,
            "an empty e2e must not read as unconfigured once it was deferred on purpose"
        );

        r.e2e = vec![CommandOutcome {
            command: "test".to_owned(),
            code: Some(0),
            output_tail: String::new(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        assert_eq!(
            r.e2e_status(),
            E2eStatus::Passed,
            "a round with real outcomes is never read as deferred, even if the flag is still set"
        );
    }

    #[test]
    fn e2e_status_reports_a_real_failure_as_failed_not_deferred() {
        let mut r = round(false, Vec::new());
        r.e2e = vec![CommandOutcome {
            command: "test".to_owned(),
            code: Some(1),
            output_tail: "boom".to_owned(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        assert_eq!(r.e2e_status(), E2eStatus::Failed);
    }

    #[test]
    fn e2e_status_never_reads_a_resource_block_as_a_failure() {
        // The exact shape of contention on the shared build cache: `e2e`
        // holds one outcome, and it is `resource_blocked`, never a command
        // that actually ran and produced a red exit code.
        let mut r = round(false, Vec::new());
        r.e2e = vec![CommandOutcome {
            command: "(waiting for the shared build cache)".to_owned(),
            code: None,
            output_tail: "contended".to_owned(),
            duration_ms: 0,
            resource_blocked: true,
        }];
        assert_eq!(
            r.e2e_status(),
            E2eStatus::ResourceBlocked,
            "magi's own inability to get a command to run must not read as a verdict on the \
             patch"
        );
    }

    #[test]
    fn verification_summary_is_silent_when_there_is_nothing_worth_saying() {
        let mut r = round(true, Vec::new());
        assert!(
            r.verification_summary("h").is_none(),
            "no verify.e2e configured: nothing to surface"
        );
        r.e2e = vec![CommandOutcome {
            command: "test".to_owned(),
            code: Some(0),
            output_tail: String::new(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        assert!(
            r.verification_summary("h").is_none(),
            "a green result needs no skepticism attached to it"
        );
    }

    #[test]
    fn verification_summary_tells_the_current_head_apart_from_an_earlier_one() {
        let mut r = round(false, Vec::new());
        r.head = "h1".to_owned();
        r.e2e = vec![CommandOutcome {
            command: "test".to_owned(),
            code: Some(1),
            output_tail: "boom".to_owned(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        r.verified_head = Some("h1".to_owned());
        r.verified_at = Some(Timestamp::now());

        let fresh = r.verification_summary("h1").expect("a failure is surfaced");
        assert!(
            fresh.label.contains("this is the head being looked at now"),
            "{}",
            fresh.label
        );
        assert_eq!(fresh.tail.as_deref(), Some("$ test\nboom\n"));

        let stale = r.verification_summary("h2").expect("still surfaced");
        assert!(
            stale.label.contains("an earlier head, since superseded"),
            "a result about a different commit than the one being looked at now must say so, \
             not read as current: {}",
            stale.label
        );
    }

    #[test]
    fn verification_summary_marks_a_resource_block_and_a_deferral_distinctly_from_a_failure() {
        let mut r = round(false, Vec::new());
        r.e2e = vec![CommandOutcome {
            command: "(waiting for the shared build cache)".to_owned(),
            code: None,
            output_tail: "contended".to_owned(),
            duration_ms: 0,
            resource_blocked: true,
        }];
        let blocked = r
            .verification_summary("h")
            .expect("a resource block is still surfaced, never silent");
        assert!(blocked.label.contains("could not run"));
        // No command actually ran, but which operation was attempted is
        // still a fact worth showing — never silent past the label either.
        let tail = blocked
            .tail
            .expect("the attempted operation is still named");
        assert!(tail.contains("(waiting for the shared build cache)"));
        assert!(tail.contains("contended"));

        let mut d = round(false, Vec::new());
        d.e2e_deferred = true;
        d.e2e_defer_reason = Some("2 blocking finding(s) already required a fix".to_owned());
        let deferred = d.verification_summary("h").expect("deferred is surfaced");
        assert!(deferred.label.contains("deferred to the fixer"));
        assert!(deferred.label.contains("2 blocking finding(s)"));
        assert!(deferred.tail.is_none());
    }

    #[test]
    fn verification_summary_says_unknown_rather_than_guessing_a_time_or_a_commit() {
        let mut r = round(false, Vec::new());
        r.e2e = vec![CommandOutcome {
            command: "test".to_owned(),
            code: Some(1),
            output_tail: "boom".to_owned(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        // verified_head/verified_at left at their default `None` — exactly
        // the shape a schema-7 round with no reconstructable timestamp has.
        let summary = r.verification_summary("h").expect("a failure is surfaced");
        assert!(summary.label.contains("commit unknown"));
        assert!(summary.label.contains("checked at: unknown"));
    }

    #[test]
    fn gate_status_tells_not_run_apart_from_passed_with_no_commands() {
        let mut s = state();
        assert_eq!(s.gate_status(), GateStatus::NotRun);

        s.gate_ran = true;
        assert_eq!(
            s.gate_status(),
            GateStatus::PassedWithNoCommands,
            "an empty gate must read as a real pass once gate_ran says it actually ran"
        );

        s.gate = vec![CommandOutcome {
            command: "cargo make check".to_owned(),
            code: Some(0),
            output_tail: String::new(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        assert_eq!(s.gate_status(), GateStatus::Passed);

        s.gate[0].code = Some(1);
        assert_eq!(s.gate_status(), GateStatus::Failed);

        s.gate_ran = false;
        assert_eq!(
            s.gate_status(),
            GateStatus::NotRun,
            "gate_ran false must win even over a non-empty gate left from a stale record"
        );
    }

    #[test]
    fn open_findings_is_empty_when_the_last_round_was_clean() {
        let mut s = state();
        s.reviews = vec![round(
            true,
            vec![finding("R1-1-1", crate::verdict::Severity::Minor)],
        )];
        assert!(s.open_findings().is_empty());
    }

    #[test]
    fn open_findings_reads_the_last_non_clean_round() {
        let mut s = state();
        s.reviews = vec![round(
            false,
            vec![finding("R1-1-1", crate::verdict::Severity::Major)],
        )];
        let open = s.open_findings();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "R1-1-1");
    }

    #[test]
    fn handed_off_with_open_findings_needs_a_mergeable_status_and_an_open_round() {
        let mut s = state();
        s.reviews = vec![round(
            false,
            vec![finding("R1-1-1", crate::verdict::Severity::Major)],
        )];

        s.status = RunStatus::Blocked;
        assert!(
            !s.handed_off_with_open_findings(),
            "a blocked run is not a hand-off"
        );

        s.status = RunStatus::Ready;
        assert!(s.handed_off_with_open_findings());

        s.reviews = vec![round(true, Vec::new())];
        assert!(
            !s.handed_off_with_open_findings(),
            "a clean last round has nothing to hand off"
        );
    }

    #[test]
    fn unmerged_by_design_is_only_ready_reached_via_merge_mode_none() {
        let mut s = state();

        s.status = RunStatus::Ready;
        assert!(
            !s.unmerged_by_design(),
            "no merge outcome recorded at all must not be flagged"
        );

        s.merge = Some(MergeOutcome {
            mode: MergeMode::None,
            ok: true,
            detail: "git merge --no-ff magi/x/A".to_owned(),
        });
        assert!(
            s.unmerged_by_design(),
            "Ready reached through mode none is the case this exists to flag"
        );

        // A PR closed without merging also leaves `status` at `Ready`, but
        // through `mode = "pr"` — a run that may still have been landable by
        // a person watching the PR, unlike the honest mode-none no-op.
        s.merge = Some(MergeOutcome {
            mode: MergeMode::Pr,
            ok: false,
            detail: "https://example.com/pr/1 was closed without merging".to_owned(),
        });
        assert!(
            !s.unmerged_by_design(),
            "a closed pull request is a different Ready and must not be relabelled"
        );

        // Same signal must not fire before the run actually got there.
        s.status = RunStatus::Gating;
        s.merge = Some(MergeOutcome {
            mode: MergeMode::None,
            ok: true,
            detail: "git merge --no-ff magi/x/A".to_owned(),
        });
        assert!(
            !s.unmerged_by_design(),
            "status must actually be Ready, not merely have a stale mode-none merge record"
        );
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
    fn a_round_recorded_before_e2e_deferral_existed_still_loads() {
        // Exactly the shape a pre-existing `run.json` has for a round: no
        // `e2e_deferred`, no `e2e_defer_reason`. Every round used to run e2e
        // unconditionally, so the honest reading of an old record's silence
        // on this is "it was not deferred" — `false`/`None`, not a load
        // failure and not a schema bump (see the `SCHEMA` doc comment: a
        // purely additive field whose absence has one unambiguous meaning
        // does not need one).
        let body = r#"{
            "round": 1,
            "head": "deadbeef",
            "reviews": [],
            "e2e": [],
            "verify_retried": false,
            "fix": null,
            "blocking": 0,
            "answered": 1,
            "expected": 1,
            "clean": true
        }"#;
        let r: ReviewRound = serde_json::from_str(body).expect("an old-shaped round must load");
        assert!(!r.e2e_deferred);
        assert!(r.e2e_defer_reason.is_none());
        assert_eq!(r.e2e_status(), E2eStatus::NotConfigured);
    }

    #[test]
    fn schema_five_state_migrates_old_empty_e2e_and_legacy_verify_budget() {
        let mut value = serde_json::to_value(state()).expect("serialize state");
        let object = value.as_object_mut().expect("state object");
        object.insert("schema".to_owned(), serde_json::json!(5));
        let graph = object["config"]["graph"]
            .as_object_mut()
            .expect("graph object");
        graph.insert("timeout_review".to_owned(), serde_json::json!(3600));
        graph.remove("timeout_verify");
        let review = object["reviews"].as_array_mut().expect("reviews");
        review.push(serde_json::json!({
            "round": 1, "head": "old", "reviews": [], "e2e": [],
            "verify_retried": false, "blocking": 0, "answered": 1,
            "expected": 1, "clean": true
        }));
        let old: RunState = serde_json::from_value(value).expect("schema-5 shape parses");
        let migrated = migrate_schema(old).expect("schema 5 migrates");
        assert_eq!(migrated.schema, SCHEMA);
        assert_eq!(migrated.config.graph.verify_timeout(), 3600);
        assert_eq!(migrated.reviews[0].e2e_status(), E2eStatus::NotConfigured);
    }

    #[test]
    fn schema_six_state_with_a_recorded_gate_migrates_to_gate_ran_true() {
        let mut value = serde_json::to_value(state()).expect("serialize state");
        let object = value.as_object_mut().expect("state object");
        object.insert("schema".to_owned(), serde_json::json!(6));
        object.insert(
            "gate".to_owned(),
            serde_json::json!([{
                "command": "cargo make check",
                "code": 0,
                "output_tail": "",
                "duration_ms": 0,
                "resource_blocked": false
            }]),
        );
        let old: RunState = serde_json::from_value(value).expect("schema-6 shape parses");
        let migrated = migrate_schema(old).expect("schema 6 migrates");
        assert_eq!(migrated.schema, SCHEMA);
        assert!(
            migrated.gate_ran,
            "a non-empty recorded gate is a real attempt, not an unrun one"
        );
        assert_eq!(migrated.gate_status(), GateStatus::Passed);
    }

    #[test]
    fn schema_six_state_with_an_empty_gate_migrates_to_gate_ran_false_and_is_retried() {
        // The exact shape of the stuck `shoka` run this schema bump fixes:
        // `verify.gate` empty, `gate` empty, schema 6. It must come back as
        // "not yet run" so the next `gate()` call re-attempts it — and for a
        // repo with no gate commands configured, that resolves instantly to
        // `PassedWithNoCommands` instead of staying stuck forever.
        let mut value = serde_json::to_value(state()).expect("serialize state");
        let object = value.as_object_mut().expect("state object");
        object.insert("schema".to_owned(), serde_json::json!(6));
        object.insert("gate".to_owned(), serde_json::json!([]));
        let old: RunState = serde_json::from_value(value).expect("schema-6 shape parses");
        let migrated = migrate_schema(old).expect("schema 6 migrates");
        assert_eq!(migrated.schema, SCHEMA);
        assert!(
            !migrated.gate_ran,
            "an empty gate on schema 6 is ambiguous and must be treated as unrun"
        );
        assert_eq!(migrated.gate_status(), GateStatus::NotRun);
    }

    #[test]
    fn schema_seven_state_reconstructs_verified_head_for_a_round_that_actually_ran_e2e() {
        // Schema 7's main review loop always checked `e2e` against the
        // round's own `head` — it just never wrote that into `verified_head`
        // unless a catch-up run had checked a *different* commit. Migrating
        // to schema 8 restores that always-true fact instead of leaving a
        // reader to assume it.
        let mut value = serde_json::to_value(state()).expect("serialize state");
        let object = value.as_object_mut().expect("state object");
        object.insert("schema".to_owned(), serde_json::json!(7));
        let reviews = object["reviews"].as_array_mut().expect("reviews");
        reviews.push(serde_json::json!({
            "round": 1, "head": "deadbeef", "reviews": [],
            "e2e": [{
                "command": "cargo test", "code": 0, "output_tail": "",
                "duration_ms": 0, "resource_blocked": false
            }],
            "verify_retried": false, "blocking": 0, "answered": 1,
            "expected": 1, "clean": true
        }));
        let old: RunState = serde_json::from_value(value).expect("schema-7 shape parses");
        let migrated = migrate_schema(old).expect("schema 7 migrates");
        assert_eq!(migrated.schema, SCHEMA);
        assert_eq!(
            migrated.reviews[0].verified_head.as_deref(),
            Some("deadbeef"),
            "a schema-7 round's main-loop e2e was always against its own head, even though the \
             field never said so"
        );
        assert!(
            migrated.reviews[0].verified_at.is_none(),
            "no historical timestamp exists to reconstruct; unknown stays unknown, not a \
             guessed 'now'"
        );
    }

    #[test]
    fn schema_seven_state_leaves_a_deferred_round_with_no_verified_head() {
        let mut value = serde_json::to_value(state()).expect("serialize state");
        let object = value.as_object_mut().expect("state object");
        object.insert("schema".to_owned(), serde_json::json!(7));
        let reviews = object["reviews"].as_array_mut().expect("reviews");
        reviews.push(serde_json::json!({
            "round": 1, "head": "deadbeef", "reviews": [],
            "e2e": [], "e2e_deferred": true,
            "verify_retried": false, "blocking": 1, "answered": 1,
            "expected": 1, "clean": false
        }));
        let old: RunState = serde_json::from_value(value).expect("schema-7 shape parses");
        let migrated = migrate_schema(old).expect("schema 7 migrates");
        assert!(
            migrated.reviews[0].verified_head.is_none(),
            "a deferred round never ran e2e; there is nothing to reconstruct"
        );
    }

    #[test]
    fn schema_six_serialization_is_rejected_by_a_schema_five_reader() {
        let body = serde_json::to_value(state()).expect("serialize state");
        assert_eq!(body["schema"], serde_json::json!(SCHEMA));
        assert_ne!(body["schema"], serde_json::json!(5));
    }

    #[test]
    fn seat_started_and_finished_track_who_has_not_answered_yet() {
        let mut s = state();
        s.seat_started("judge", "judge-1", std::time::Duration::from_secs(60), 0);
        s.seat_started("judge", "judge-2", std::time::Duration::from_secs(60), 0);
        assert_eq!(s.active.len(), 2, "both seats are still out");

        s.seat_finished("judge-1");
        assert_eq!(
            s.active.keys().collect::<Vec<_>>(),
            vec!["judge-2"],
            "only the seat that answered drops out; judge-2 is still waited on"
        );
    }

    /// `seats_active` / `tasks_active` are the accessors report/web read
    /// instead of `active` directly, so neither ever counts the other kind of
    /// entry as a seat — a `verify.e2e` task must never inflate a quorum or
    /// seat count, and a seat must never show up in a task listing.
    #[test]
    fn seats_active_and_tasks_active_never_cross_over() {
        let mut s = state();
        s.seat_started("judge", "judge-1", std::time::Duration::from_secs(60), 0);
        s.task_command(
            "e2e",
            "verify",
            0,
            "cargo test",
            1,
            2,
            std::time::Duration::from_secs(600),
        );

        assert_eq!(
            s.seats_active()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["judge-1"]
        );
        assert_eq!(
            s.tasks_active()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["e2e"]
        );

        // A command boundary updates the same entry in place — still one
        // task, never a second one accumulating alongside it.
        s.task_command(
            "e2e",
            "verify",
            0,
            "cargo clippy",
            2,
            2,
            std::time::Duration::from_secs(600),
        );
        assert_eq!(s.tasks_active().count(), 1);
        assert_eq!(s.active["e2e"].command.as_deref(), Some("cargo clippy"));

        s.task_finished("e2e");
        assert!(s.tasks_active().next().is_none());
        assert_eq!(
            s.seats_active()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["judge-1"],
            "clearing the task must not touch the seat entry"
        );
    }

    #[test]
    fn a_retry_is_recorded_as_a_later_attempt_on_the_same_seat() {
        let mut s = state();
        s.seat_started("review", "review-2", std::time::Duration::from_secs(30), 0);
        s.seat_finished("review-2");
        // A nudge re-asks the same seat; attempt says this is not the first
        // time, which is the only trace a nudge otherwise leaves behind.
        s.seat_started("review", "review-2", std::time::Duration::from_secs(30), 1);
        assert_eq!(s.active["review-2"].attempt, 1);
    }

    #[test]
    fn active_seat_reports_elapsed_and_remaining_time() {
        let now = Timestamp::now();
        let started = now - jiff::SignedDuration::from_secs(30);
        let seat = ActiveSeat {
            node: "judge".to_owned(),
            started_at: started,
            timeout_secs: 100,
            attempt: 0,
            task: None,
            command: None,
            index: None,
            total: None,
        };
        assert_eq!(seat.elapsed_secs(now), 30);
        assert_eq!(seat.remaining_secs(now), 70);
    }

    #[test]
    fn remaining_time_never_goes_negative_past_the_timeout() {
        // `agy`'s own print-timeout occasionally overruns by a hair before the
        // kill lands; a naive subtraction would print a negative "time left".
        let now = Timestamp::now();
        let started = now - jiff::SignedDuration::from_secs(200);
        let seat = ActiveSeat {
            node: "implement".to_owned(),
            started_at: started,
            timeout_secs: 100,
            attempt: 1,
            task: None,
            command: None,
            index: None,
            total: None,
        };
        assert_eq!(seat.remaining_secs(now), 0);
    }

    #[test]
    fn clear_active_drops_stale_seats_and_reports_whether_it_did() {
        let mut s = state();
        assert!(!s.clear_active(), "nothing to clear on a fresh run");
        s.seat_started(
            "implement",
            "impl-B",
            std::time::Duration::from_secs(3600),
            0,
        );
        assert!(s.clear_active(), "a leftover entry is reported as cleared");
        assert!(s.active.is_empty());
    }

    #[test]
    fn active_seat_carries_nothing_that_could_be_read_as_output_bytes() {
        // `agy` prints exactly one JSON object, at the very end (see
        // `agent::dropped_stream`'s doc comment) — a seat can sit at zero
        // captured bytes for its whole timeout while working normally. So
        // `ActiveSeat` records only the wall-clock facts (when it started,
        // its budget, which attempt), never a byte count, which is what
        // keeps a reader from being able to build "0 bytes => dead" out of
        // it even by accident.
        let seat = ActiveSeat {
            node: "implement".to_owned(),
            started_at: Timestamp::now(),
            timeout_secs: 60,
            attempt: 0,
            task: None,
            command: None,
            index: None,
            total: None,
        };
        let value = serde_json::to_value(&seat).unwrap();
        let keys: std::collections::BTreeSet<String> =
            value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "node".to_owned(),
                "started_at".to_owned(),
                "timeout_secs".to_owned(),
                "attempt".to_owned(),
            ]),
            "a byte count here would be a lever to declare a silent-but-healthy seat dead, and \
             the task-only fields must stay absent (not null) on an ordinary seat entry"
        );
    }

    /// The same guarantee as
    /// [`active_seat_carries_nothing_that_could_be_read_as_output_bytes`],
    /// extended to a task entry: `verify.e2e` / `verify.gate` are exactly as
    /// silent as `agy` between commands, so a running command-list task must
    /// never carry anything a reader could mistake for output-byte evidence
    /// either.
    #[test]
    fn task_active_seat_carries_nothing_that_could_be_read_as_output_bytes() {
        let seat = ActiveSeat {
            node: "verify".to_owned(),
            started_at: Timestamp::now(),
            timeout_secs: 600,
            attempt: 0,
            task: Some("e2e".to_owned()),
            command: Some("cargo test".to_owned()),
            index: Some(1),
            total: Some(3),
        };
        let value = serde_json::to_value(&seat).unwrap();
        let keys: std::collections::BTreeSet<String> =
            value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "node".to_owned(),
                "started_at".to_owned(),
                "timeout_secs".to_owned(),
                "attempt".to_owned(),
                "task".to_owned(),
                "command".to_owned(),
                "index".to_owned(),
                "total".to_owned(),
            ]),
        );
    }

    #[test]
    fn an_old_run_json_without_active_seats_still_loads() {
        // Schema did not bump for this field: an already-written run.json
        // simply lacks the key, and `#[serde(default)]` must fill it in
        // rather than fail the whole read.
        let s = state();
        let mut value = serde_json::to_value(&s).unwrap();
        value.as_object_mut().unwrap().remove("active");
        let back: RunState = serde_json::from_value(value).unwrap();
        assert!(back.active.is_empty());
        assert_eq!(back.schema, SCHEMA);
    }

    #[test]
    fn an_old_run_json_without_jobs_still_loads() {
        // No schema bump for this field either, for the same reason: an
        // empty `jobs` list on an old record means exactly what it always
        // meant for that record — no adapter existed yet to report one —
        // and `#[serde(default)]` fills it in rather than failing the read.
        let s = state();
        let mut value = serde_json::to_value(&s).unwrap();
        value.as_object_mut().unwrap().remove("jobs");
        let back: RunState = serde_json::from_value(value).unwrap();
        assert!(back.jobs.is_empty());
        assert_eq!(back.schema, SCHEMA);
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
            verified_noop: None,
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
