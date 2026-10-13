//! Carrying an earlier attempt's evidence into the run that takes its branch over.
//!
//! A task whose previous run stopped on a failed verification used to reopen
//! its branch as a context-free review-only run: the new reviewers and fixer
//! never saw the failing command's output, the review decisions or the
//! unresolved findings, and round 1 started from scratch.
//!
//! [`assess`] is pure. From the earlier run's record and the relation of the
//! branch tip to the commit that run reviewed it decides between
//!
//! * [`Mode::VerifyRepair`]: the earlier panel found nothing blocking and only
//!   verification failed, on exactly the commit that is about to be reviewed.
//!   The fixer gets the failure first, then the review is scoped to what
//!   changed since the reviewed commit.
//! * [`Mode::FullReview`]: anything doubtful. The reviewers still see the
//!   earlier context, but nothing is scoped and no approval is carried; each
//!   [`RecoveryRecord::reasons`] entry says why.
//!
//! Inherited verification is evidence for a prompt, never a result: it is not
//! copied into [`crate::run::ReviewRound`], so verification always runs again
//! on the commit that is going to land.

use serde::{Deserialize, Serialize};

use crate::run::{CommandOutcome, E2eStatus, ReviewRound, RunState};

/// Restore an interrupted repair and confirm its original commit before any review.
pub(crate) async fn restore_repair(wt: &std::path::Path, before: &str) -> anyhow::Result<()> {
    crate::git::restore_to(wt, before).await?;
    let tip = crate::git::rev_parse(wt, "HEAD").await?;
    anyhow::ensure!(tip == before, "the winner is at {tip}, not {before}");
    Ok(())
}

fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}
use crate::verdict::{Finding, ReviewVote};

/// How many times evidence may be handed from one run to the next. Past this
/// the successor starts as a plain full review.
pub const MAX_GENERATION: u32 = 2;

/// Longest output tail carried into a prompt, in bytes.
pub const TAIL_MAX: usize = 6_000;

/// Longest scoped diff shown to a reviewer, in bytes.
pub const SINCE_DIFF_MAX: usize = 60_000;

/// What the review after a takeover does with the earlier run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// One repair fix with the failure in hand, then a review scoped to the
    /// changes since the reviewed commit.
    VerifyRepair,
    /// The whole patch is reviewed again; the earlier run is context only.
    FullReview,
}

/// Which check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// The round's `verify.e2e`.
    E2e,
    /// The final `verify.gate`.
    Gate,
}

/// The failed verification the earlier run stopped on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Which check.
    pub source: Source,
    /// The command, as configured.
    pub command: String,
    /// Exit code, `None` on timeout or signal.
    pub code: Option<i32>,
    /// Scrubbed, clipped tail of its output.
    pub output_tail: String,
    /// The commit it was run against.
    pub head: String,
}

/// One earlier review round, reduced to its decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorRound {
    /// Round number in the earlier run.
    pub round: usize,
    /// The commit it reviewed.
    pub head: String,
    /// Findings that held the merge.
    pub blocking: usize,
    /// Seats that answered, of `expected`.
    pub answered: usize,
    /// Seats the round expected.
    pub expected: usize,
    /// The round's verdict, when a vote was usable.
    pub verdict: Option<ReviewVote>,
}

/// Where the one repair fix stands. Saved before the fixer is called, so a
/// resume never repairs twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Repair {
    /// Not attempted (yet, or not applicable).
    NotStarted,
    /// The fixer was called from this commit and has not been settled.
    Started {
        /// HEAD before the call.
        before: String,
    },
    /// Settled.
    Done {
        /// HEAD before the call.
        before: String,
        /// HEAD after it.
        after: String,
        /// The fixer's own commit or rescue commit moved HEAD.
        committed: bool,
        /// Why nothing was kept (the worktree was restored to `before`).
        failed: Option<String>,
    },
}

/// What a run inherited from the attempt it took over, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRecord {
    /// The earlier run.
    pub from_run: String,
    /// How many hand-overs of evidence lie behind this one.
    pub generation: u32,
    /// The decision.
    pub mode: Mode,
    /// For [`Mode::FullReview`]: every reason a narrower recovery was not
    /// justified. Empty for [`Mode::VerifyRepair`].
    #[serde(default)]
    pub reasons: Vec<String>,
    /// The earlier run's last reviewed commit, when it had one.
    #[serde(default)]
    pub reviewed_head: Option<String>,
    /// The earlier rounds' decisions.
    #[serde(default)]
    pub rounds: Vec<PriorRound>,
    /// Findings the last earlier round left open.
    #[serde(default)]
    pub open_findings: Vec<Finding>,
    /// The failed verification, when one was recorded.
    #[serde(default)]
    pub evidence: Option<Evidence>,
    /// The task's own statement replaced the commit-log stand-in.
    #[serde(default)]
    pub criteria_inherited: bool,
    /// The repair fix.
    #[serde(default = "not_started")]
    pub repair: Repair,
}

fn not_started() -> Repair {
    Repair::NotStarted
}

/// How the branch tip relates to the commit the earlier run reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The same commit.
    Same,
    /// The reviewed commit is a proper ancestor: commits were added after it.
    Descendant,
    /// Not an ancestor: rebased, rewritten or unknown to this repository.
    Diverged,
}

/// Clip `text` to its last `max` bytes on a char boundary.
pub fn tail_of(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[...]\n{}", &text[start..])
}

/// Clip `text` to its first `max` bytes on a char boundary.
pub fn head_of(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[... truncated]", &text[..end])
}

fn evidence_from(round: &ReviewRound, prior: &RunState) -> Option<Evidence> {
    let scrub =
        |s: &str| crate::scrub::scrub(&tail_of(s, TAIL_MAX), &crate::scrub::Identity::current());
    let failed = |o: &&CommandOutcome| !o.ok() && !o.resource_blocked;
    if round.e2e_status() == E2eStatus::Failed
        && let (Some(o), Some(head)) = (round.e2e.iter().find(failed), &round.verified_head)
    {
        return Some(Evidence {
            source: Source::E2e,
            command: o.command.clone(),
            code: o.code,
            output_tail: scrub(&o.output_tail),
            head: head.clone(),
        });
    }
    // The gate runs on the tree the last round left; it is only evidence about
    // that round's head while no gate fix moved the tree afterwards.
    if prior.gate_ran
        && prior.gate_fixes.is_empty()
        && let Some(o) = prior.gate.iter().find(failed)
    {
        return Some(Evidence {
            source: Source::Gate,
            command: o.command.clone(),
            code: o.code,
            output_tail: scrub(&o.output_tail),
            head: round.head.clone(),
        });
    }
    None
}

/// The record for a run that takes `prior` over. Pure.
///
/// `prior` is `None` when the earlier attempt's record could not be read.
/// `base_now` is the base commit the new run reviews against and `relation`
/// how the branch tip stands to the earlier run's last reviewed commit.
pub fn assess(
    from_run: &str,
    prior: Option<&RunState>,
    base_now: &str,
    relation: Relation,
) -> RecoveryRecord {
    let mut rec = RecoveryRecord {
        from_run: from_run.to_owned(),
        generation: 0,
        mode: Mode::FullReview,
        reasons: Vec::new(),
        reviewed_head: None,
        rounds: Vec::new(),
        open_findings: Vec::new(),
        evidence: None,
        criteria_inherited: false,
        repair: Repair::NotStarted,
    };
    let Some(prior) = prior else {
        rec.reasons
            .push("the earlier run's record could not be read".to_owned());
        return rec;
    };
    rec.generation = prior.recovery.as_ref().map_or(0, |r| r.generation) + 1;
    rec.rounds = prior
        .reviews
        .iter()
        .map(|r| PriorRound {
            round: r.round,
            head: r.head.clone(),
            blocking: r.blocking,
            answered: r.answered,
            expected: r.expected,
            verdict: r.verdict,
        })
        .collect();
    let Some(last) = prior.reviews.last() else {
        rec.reasons
            .push("the earlier run never completed a review round".to_owned());
        return rec;
    };
    rec.reviewed_head = Some(last.head.clone());
    rec.open_findings = last
        .reviews
        .iter()
        .flat_map(|r| r.findings.iter().cloned())
        .collect();
    rec.evidence = evidence_from(last, prior);

    if rec.generation > MAX_GENERATION {
        rec.reasons.push(format!(
            "evidence has already been handed over {MAX_GENERATION} times"
        ));
    }
    if last.answered < last.expected || last.reviews.iter().any(|r| r.failed.is_some()) {
        rec.reasons.push(format!(
            "only {} of {} reviewer seat(s) answered the last round; a panel that did not \
             answer is not a clean one",
            last.answered, last.expected
        ));
    }
    if last.blocking > 0 {
        rec.reasons.push(format!(
            "the last round still had {} blocking finding(s)",
            last.blocking
        ));
    }
    match &rec.evidence {
        None => rec
            .reasons
            .push("no failed verification with a known command was recorded".to_owned()),
        Some(e) if e.output_tail.trim().is_empty() => rec
            .reasons
            .push("the failed verification kept no output to act on".to_owned()),
        Some(e) if e.head != last.head => rec.reasons.push(
            "the failed verification was about a different commit than the one reviewed".to_owned(),
        ),
        Some(_) => {}
    }
    match relation {
        Relation::Same => {}
        Relation::Descendant => rec.reasons.push(
            "commits were added after the reviewed commit; they were never reviewed".to_owned(),
        ),
        Relation::Diverged => rec.reasons.push(
            "the reviewed commit is not an ancestor of the branch: it was rebased or rewritten"
                .to_owned(),
        ),
    }
    if prior.base_commit != base_now {
        rec.reasons.push(format!(
            "the base moved from {} to {}",
            short(&prior.base_commit),
            short(base_now)
        ));
    }
    if rec.reasons.is_empty() {
        rec.mode = Mode::VerifyRepair;
    }
    rec
}

/// Re-check a [`Mode::VerifyRepair`] record against the tree the review is
/// about to look at (a base sync may have rebased it after the takeover was
/// assessed). Returns true when it was downgraded. Pure.
pub fn recheck(rec: &mut RecoveryRecord, head: &str) -> bool {
    if rec.mode != Mode::VerifyRepair {
        return false;
    }
    let Some(reviewed) = rec.reviewed_head.as_deref() else {
        return false;
    };
    if reviewed == head {
        return false;
    }
    rec.mode = Mode::FullReview;
    rec.reasons.push(format!(
        "the branch moved from {} to {} before the review started (rebased onto the base or \
         changed); the earlier approval does not carry",
        short(reviewed),
        short(head)
    ));
    true
}

impl RecoveryRecord {
    /// One line for the event log, `magi show` and the web.
    pub fn summary(&self) -> String {
        let from = crate::run::short_of(&self.from_run);
        match self.mode {
            Mode::VerifyRepair => format!(
                "inherited from run {from}: reviewed commit, {} round(s), failed verification \
                 `{}`; repair first, then a review scoped to the changes since",
                self.rounds.len(),
                self.evidence.as_ref().map_or("?", |e| e.command.as_str())
            ),
            Mode::FullReview => format!(
                "full review after run {from}: {}",
                if self.reasons.is_empty() {
                    "no narrower recovery applied".to_owned()
                } else {
                    self.reasons.join("; ")
                }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::ReviewRecord;
    use crate::verdict::Severity;

    fn outcome(code: i32, tail: &str) -> CommandOutcome {
        CommandOutcome {
            command: "cargo test".to_owned(),
            code: Some(code),
            output_tail: tail.to_owned(),
            duration_ms: 1,
            resource_blocked: false,
        }
    }

    fn seat(failed: bool, findings: Vec<Finding>) -> ReviewRecord {
        ReviewRecord {
            reviewer: 1,
            agent: "a".to_owned(),
            summary: String::new(),
            findings,
            vote: Some(ReviewVote::Approve),
            failed: failed.then(|| "timed out".to_owned()),
            duration_ms: 0,
            attempts: 0,
        }
    }

    fn round(head: &str, e2e: Vec<CommandOutcome>) -> ReviewRound {
        ReviewRound {
            round: 3,
            head: head.to_owned(),
            verified_head: Some(head.to_owned()),
            verified_at: None,
            reviews: vec![seat(false, Vec::new())],
            e2e,
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 1,
            expected: 1,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: Some(ReviewVote::Approve),
        }
    }

    fn prior(round: ReviewRound) -> RunState {
        let mut s = RunState::new(
            std::path::PathBuf::from("/tmp/x"),
            "main".to_owned(),
            "base1".to_owned(),
            "task".to_owned(),
            crate::config::Config::default(),
        );
        s.reviews.push(round);
        s
    }

    fn failing() -> RunState {
        prior(round("aaa111", vec![outcome(101, "1 failed: quiet_spawn")]))
    }

    #[test]
    fn unchanged_commit_with_a_nonblocking_failed_verification_is_a_verify_repair() {
        let r = assess("r1", Some(&failing()), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::VerifyRepair, "{:?}", r.reasons);
        assert!(r.reasons.is_empty());
        assert_eq!(r.generation, 1);
        let e = r.evidence.unwrap();
        assert_eq!(e.command, "cargo test");
        assert_eq!(e.head, "aaa111");
        assert!(e.output_tail.contains("quiet_spawn"));
    }

    #[test]
    fn descendant_rebased_and_moved_base_widen_the_review() {
        let p = failing();
        let d = assess("r1", Some(&p), "base1", Relation::Descendant);
        assert_eq!(d.mode, Mode::FullReview);
        assert!(d.reasons[0].contains("never reviewed"));
        let x = assess("r1", Some(&p), "base1", Relation::Diverged);
        assert_eq!(x.mode, Mode::FullReview);
        assert!(x.reasons[0].contains("rebased"));
        let b = assess("r1", Some(&p), "base2", Relation::Same);
        assert_eq!(b.mode, Mode::FullReview);
        assert!(b.reasons[0].contains("base moved"));
        // The evidence still travels as context.
        assert!(b.evidence.is_some());
    }

    #[test]
    fn missing_or_empty_evidence_falls_back_to_a_full_review() {
        let none = prior(round("aaa111", Vec::new()));
        let r = assess("r1", Some(&none), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::FullReview);
        assert!(r.reasons[0].contains("no failed verification"));
        let empty = prior(round("aaa111", vec![outcome(1, "  \n")]));
        let r = assess("r1", Some(&empty), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::FullReview);
        assert!(r.reasons[0].contains("no output"));
        let r = assess("r1", None, "base1", Relation::Same);
        assert_eq!(r.mode, Mode::FullReview);
        assert!(r.reasons[0].contains("could not be read"));
    }

    #[test]
    fn evidence_about_another_commit_is_not_reused() {
        let mut p = failing();
        p.reviews[0].verified_head = Some("bbb222".to_owned());
        let r = assess("r1", Some(&p), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::FullReview);
        assert!(r.reasons[0].contains("different commit"));
    }

    #[test]
    fn a_timed_out_seat_or_a_blocking_finding_is_never_a_clean_panel() {
        let mut p = failing();
        p.reviews[0].answered = 0;
        p.reviews[0].reviews[0] = seat(true, Vec::new());
        let r = assess("r1", Some(&p), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::FullReview);
        assert!(r.reasons[0].contains("did not answer"));

        let mut p = failing();
        p.reviews[0].blocking = 1;
        p.reviews[0].reviews[0].findings = vec![Finding {
            id: "R3-1-1".to_owned(),
            severity: Severity::Major,
            file: None,
            line: None,
            title: "t".to_owned(),
            detail: String::new(),
        }];
        let r = assess("r1", Some(&p), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::FullReview);
        assert_eq!(r.open_findings.len(), 1);
    }

    #[test]
    fn generations_are_bounded() {
        let mut p = failing();
        let mut inherited = assess("r0", Some(&failing()), "base1", Relation::Same);
        inherited.generation = MAX_GENERATION;
        p.recovery = Some(inherited);
        let r = assess("r1", Some(&p), "base1", Relation::Same);
        assert_eq!(r.generation, MAX_GENERATION + 1);
        assert_eq!(r.mode, Mode::FullReview);
        assert!(r.reasons[0].contains("handed over"));
    }

    #[test]
    fn a_gate_failure_counts_only_while_no_gate_fix_moved_the_tree() {
        let mut p = prior(round("aaa111", Vec::new()));
        p.gate_ran = true;
        p.gate = vec![outcome(2, "gate red")];
        let r = assess("r1", Some(&p), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::VerifyRepair);
        assert_eq!(r.evidence.unwrap().source, Source::Gate);
        p.gate_fixes.push(crate::run::GateFixRecord {
            agent: "a".to_owned(),
            failed: Vec::new(),
            notes: String::new(),
            committed: true,
            error: None,
        });
        let r = assess("r1", Some(&p), "base1", Relation::Same);
        assert_eq!(r.mode, Mode::FullReview);
    }

    #[test]
    fn recheck_downgrades_a_moved_branch() {
        let mut r = assess("r1", Some(&failing()), "base1", Relation::Same);
        assert!(!recheck(&mut r, "aaa111"));
        assert_eq!(r.mode, Mode::VerifyRepair);
        assert!(recheck(&mut r, "ccc333"));
        assert_eq!(r.mode, Mode::FullReview);
        assert!(r.reasons[0].contains("moved"));
    }

    #[test]
    fn tails_are_clipped_on_char_boundaries() {
        let s = "あ".repeat(100);
        assert!(tail_of(&s, 10).len() < 30);
        assert!(head_of(&s, 10).starts_with('あ'));
        assert_eq!(tail_of("ok", 10), "ok");
    }
}
